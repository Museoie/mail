import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createUser, type TestServer, type TestUser } from "../harness";
import { ImapClient, openSession } from "../harness/imap";
import { deliver, mailDomain, startImapServer, type DeliveredMail } from "./support";

let server: TestServer;
let userCount = 0;

interface Box {
  user: TestUser;
  imap: ImapClient;
  mails: DeliveredMail[];
}

/** A fresh account with `count` unread mails for `hello@`, and a session with INBOX selected. */
const inbox = async (count: number): Promise<Box> => {
  const user = await createUser(server, `store${++userCount}`);
  const mails: DeliveredMail[] = [];
  for (let i = 0; i < count; i++) {
    mails.push(await deliver(server, { to: `hello@${mailDomain(server, user.username)}`, subject: `Mail ${i + 1}` }));
  }
  const imap = await openSession(server, user.username, user.password, "INBOX");
  return { user, imap, mails };
};

/** The flag columns of a stored mail. */
const columns = async (mail: DeliveredMail) => {
  const [row] = await server.db.query(
    "SELECT read, saved, deleted, draft, answered, expunged FROM mails WHERE mail_id = $1",
    [mail.mail_id]
  );
  return row!;
};
const CLEAR = { read: false, saved: false, deleted: false, draft: false, answered: false, expunged: false };

/** The mailboxes a mail is filed under, as `name#uid`. */
const filedUnder = async (mail: DeliveredMail): Promise<string[]> => {
  const rows = await server.db.query<{ mailbox: string; uid: string }>(
    "SELECT mailbox, uid FROM mail_mailbox_uid WHERE mail_id = $1 ORDER BY mailbox",
    [mail.mail_id]
  );
  return rows.map((row) => `${row.mailbox}#${row.uid}`);
};

beforeAll(async () => {
  server = await startImapServer();
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

describe("STORE: +FLAGS, -FLAGS and FLAGS", () => {
  test("+FLAGS adds, -FLAGS removes, and each answers with the message's whole flag list", async () => {
    const { imap, mails } = await inbox(1);
    expect((await imap.command("STORE 1 +FLAGS (\\Seen)")).lines).toEqual([
      "* 1 FETCH (FLAGS (\\Seen))",
      "A3 OK STORE completed",
    ]);
    expect((await imap.command("STORE 1 +FLAGS (\\Answered)")).lines).toEqual([
      "* 1 FETCH (FLAGS (\\Seen \\Answered))",
      "A4 OK STORE completed",
    ]);
    expect((await imap.command("STORE 1 -FLAGS (\\Seen)")).lines).toEqual([
      "* 1 FETCH (FLAGS (\\Answered))",
      "A5 OK STORE completed",
    ]);
    expect(await columns(mails[0]!)).toEqual({ ...CLEAR, answered: true });
    imap.close();
  });

  test("FLAGS replaces the whole list", async () => {
    const { imap, mails } = await inbox(1);
    await imap.command("STORE 1 +FLAGS.SILENT (\\Seen \\Deleted)");
    expect((await imap.command("STORE 1 FLAGS (\\Answered \\Flagged)")).lines).toEqual([
      "* 1 FETCH (FLAGS (\\Flagged \\Answered))",
      "A4 OK STORE completed",
    ]);
    expect(await columns(mails[0]!)).toEqual({ ...CLEAR, saved: true, answered: true });
    expect((await imap.command("STORE 1 FLAGS ()")).lines).toEqual(["* 1 FETCH (FLAGS ())", "A5 OK STORE completed"]);
    expect(await columns(mails[0]!)).toEqual(CLEAR);
    imap.close();
  });

  const flags: [string, keyof typeof CLEAR][] = [
    ["\\Seen", "read"],
    ["\\Flagged", "saved"],
    ["\\Deleted", "deleted"],
    ["\\Answered", "answered"],
  ];

  test("each system flag is one column of the mail's row", async () => {
    const { imap, mails } = await inbox(1);
    for (const [flag, column] of flags) {
      const added = await imap.command(`STORE 1 +FLAGS (${flag})`);
      expect({ flag, untagged: added.untagged }).toEqual({ flag, untagged: [`* 1 FETCH (FLAGS (${flag}))`] });
      expect({ flag, row: await columns(mails[0]!) }).toEqual({ flag, row: { ...CLEAR, [column]: true } });
      expect((await imap.command("FETCH 1 FLAGS")).untagged).toEqual([`* 1 FETCH (FLAGS (${flag}))`]);
      const removed = await imap.command(`STORE 1 -FLAGS (${flag})`);
      expect({ flag, untagged: removed.untagged }).toEqual({ flag, untagged: ["* 1 FETCH (FLAGS ())"] });
      expect({ flag, row: await columns(mails[0]!) }).toEqual({ flag, row: CLEAR });
    }
    imap.close();
  });

  test("flags are always listed as \\Seen \\Flagged \\Deleted \\Draft \\Answered, whatever order they were set in", async () => {
    const { imap } = await inbox(1);
    const result = await imap.command("STORE 1 FLAGS (\\Answered \\Deleted \\Flagged \\Seen)");
    expect(result.untagged).toEqual(["* 1 FETCH (FLAGS (\\Seen \\Flagged \\Deleted \\Answered))"]);
    imap.close();
  });

  test("adding a flag that is set, or removing one that is not, still answers with the list", async () => {
    const { imap } = await inbox(1);
    await imap.command("STORE 1 +FLAGS.SILENT (\\Seen)");
    expect((await imap.command("STORE 1 +FLAGS (\\Seen)")).untagged).toEqual(["* 1 FETCH (FLAGS (\\Seen))"]);
    expect((await imap.command("STORE 1 -FLAGS (\\Flagged)")).untagged).toEqual(["* 1 FETCH (FLAGS (\\Seen))"]);
    imap.close();
  });

  test("the .SILENT forms change the flags and send nothing untagged", async () => {
    const { imap, mails } = await inbox(1);
    expect((await imap.command("STORE 1 +FLAGS.SILENT (\\Seen \\Flagged)")).lines).toEqual(["A3 OK STORE completed"]);
    expect(await columns(mails[0]!)).toEqual({ ...CLEAR, read: true, saved: true });
    expect((await imap.command("STORE 1 -FLAGS.SILENT (\\Seen)")).lines).toEqual(["A4 OK STORE completed"]);
    expect(await columns(mails[0]!)).toEqual({ ...CLEAR, saved: true });
    expect((await imap.command("STORE 1 FLAGS.SILENT (\\Answered)")).lines).toEqual(["A5 OK STORE completed"]);
    expect(await columns(mails[0]!)).toEqual({ ...CLEAR, answered: true });
    expect((await imap.command("STORE 1 FLAGS.SILENT ()")).lines).toEqual(["A6 OK STORE completed"]);
    expect(await columns(mails[0]!)).toEqual(CLEAR);
    imap.close();
  });

  test("every STORE stamps the messages it addressed with a new mod-sequence, changed or not", async () => {
    const { imap, mails } = await inbox(2);
    const modseq = async (mail: DeliveredMail) =>
      Number((await server.db.query("SELECT modseq FROM mails WHERE mail_id = $1", [mail.mail_id]))[0]!.modseq);
    const before = await modseq(mails[0]!);
    await imap.command("STORE 1 +FLAGS.SILENT (\\Seen)");
    const first = await modseq(mails[0]!);
    expect(first).toBeGreaterThan(before);
    await imap.command("STORE 1 +FLAGS.SILENT (\\Seen)");
    expect(await modseq(mails[0]!)).toBe(first + 1);
    // One value for the whole command, shared by every message in the range.
    await imap.command("STORE 1:2 +FLAGS.SILENT (\\Answered)");
    expect(await modseq(mails[0]!)).toBe(first + 2);
    expect(await modseq(mails[1]!)).toBe(first + 2);
    imap.close();
  });
});

describe("STORE: flag syntax", () => {
  test("a single flag may be given without parentheses; a second one is ignored", async () => {
    const { imap } = await inbox(1);
    expect((await imap.command("STORE 1 +FLAGS \\Seen")).untagged).toEqual(["* 1 FETCH (FLAGS (\\Seen))"]);
    expect((await imap.command("STORE 1 +FLAGS \\Answered \\Flagged")).untagged).toEqual([
      "* 1 FETCH (FLAGS (\\Seen \\Answered))",
    ]);
    imap.close();
  });

  test("the item name is case-insensitive; an unknown one is BAD", async () => {
    const { imap } = await inbox(1);
    expect((await imap.command("STORE 1 +flags.silent (\\Seen)")).lines).toEqual(["A3 OK STORE completed"]);
    expect((await imap.command("STORE 1 flags (\\Seen)")).untagged).toEqual(["* 1 FETCH (FLAGS (\\Seen))"]);
    expect((await imap.command("STORE 1 FLAGS.QUIET (\\Seen)")).lines).toEqual([
      "A5 BAD Invalid store operation: FLAGS.QUIET",
    ]);
    expect((await imap.command("STORE 1 ++FLAGS (\\Seen)")).lines).toEqual(["A6 BAD Invalid store operation: ++FLAGS"]);
    imap.close();
  });

  test("flag names are matched exactly: another letter case is not the flag", async () => {
    // NOTE: looks like a bug. RFC 3501 flag names are case-insensitive;
    // "\seen" and "\FLAGGED" are accepted here and silently do nothing.
    const { imap, mails } = await inbox(1);
    expect((await imap.command("STORE 1 +FLAGS (\\seen \\FLAGGED \\deleted)")).lines).toEqual([
      "* 1 FETCH (FLAGS ())",
      "A3 OK STORE completed",
    ]);
    expect(await columns(mails[0]!)).toEqual(CLEAR);
    imap.close();
  });

  test("keywords and unknown system flags are accepted and dropped", async () => {
    // NOTE: PERMANENTFLAGS advertises \* (new keywords may be created), but
    // no keyword is ever stored: the STORE completes OK and nothing changes.
    const { imap, mails } = await inbox(1);
    await imap.command("STORE 1 +FLAGS.SILENT (\\Seen)");
    for (const list of ["($Forwarded)", "(custom NonJunk)", "(\\Recent)", "(\\Bogus)", "($MDNSent \\Seen)"]) {
      const result = await imap.command(`STORE 1 +FLAGS ${list}`);
      expect({ list, lines: result.lines }).toEqual({
        list,
        lines: ["* 1 FETCH (FLAGS (\\Seen))", `${result.tag} OK STORE completed`],
      });
    }
    // Replacing the list with only a keyword clears every flag.
    expect((await imap.command("STORE 1 FLAGS ($Forwarded)")).untagged).toEqual(["* 1 FETCH (FLAGS ())"]);
    expect(await columns(mails[0]!)).toEqual(CLEAR);
    expect((await imap.command("SEARCH KEYWORD $Forwarded")).untagged).toEqual(["* SEARCH "]);
    imap.close();
  });

  test("an empty list adds or removes nothing; a missing list is BAD; an unclosed one is tolerated", async () => {
    const { imap } = await inbox(1);
    expect((await imap.command("STORE 1 +FLAGS ()")).untagged).toEqual(["* 1 FETCH (FLAGS ())"]);
    expect((await imap.command("STORE 1 -FLAGS ()")).untagged).toEqual(["* 1 FETCH (FLAGS ())"]);
    expect((await imap.command("STORE 1 +FLAGS")).lines).toEqual(["A5 BAD Invalid flag in STORE"]);
    expect((await imap.command("STORE 1 FLAGS (\\Seen")).untagged).toEqual(["* 1 FETCH (FLAGS (\\Seen))"]);
    imap.close();
  });
});

describe("STORE: message sets", () => {
  /** Untagged answers sorted, since their order within one command is not fixed. */
  const store = async (imap: ImapClient, args: string) => {
    const result = await imap.command(`STORE ${args}`);
    return { status: result.tagged!.replace(/^A\d+ /, ""), untagged: [...result.untagged].sort() };
  };
  const ok = "OK STORE completed";

  test("a range, a list and 1:* address the messages they name", async () => {
    const { imap } = await inbox(4);
    expect(await store(imap, "2:3 +FLAGS (\\Seen)")).toEqual({
      status: ok,
      untagged: ["* 2 FETCH (FLAGS (\\Seen))", "* 3 FETCH (FLAGS (\\Seen))"],
    });
    expect(await store(imap, "1,4 +FLAGS (\\Flagged)")).toEqual({
      status: ok,
      untagged: ["* 1 FETCH (FLAGS (\\Flagged))", "* 4 FETCH (FLAGS (\\Flagged))"],
    });
    expect(await store(imap, "1:* +FLAGS (\\Answered)")).toEqual({
      status: ok,
      untagged: [
        "* 1 FETCH (FLAGS (\\Flagged \\Answered))",
        "* 2 FETCH (FLAGS (\\Seen \\Answered))",
        "* 3 FETCH (FLAGS (\\Seen \\Answered))",
        "* 4 FETCH (FLAGS (\\Flagged \\Answered))",
      ],
    });
    expect(await store(imap, "3:2 -FLAGS (\\Seen)")).toEqual({
      status: ok,
      untagged: ["* 2 FETCH (FLAGS (\\Answered))", "* 3 FETCH (FLAGS (\\Answered))"],
    });
    imap.close();
  });

  test("* is the last message", async () => {
    const { imap } = await inbox(3);
    expect(await store(imap, "* +FLAGS (\\Seen)")).toEqual({ status: ok, untagged: ["* 3 FETCH (FLAGS (\\Seen))"] });
    expect(await store(imap, "2:* +FLAGS (\\Flagged)")).toEqual({
      status: ok,
      untagged: ["* 2 FETCH (FLAGS (\\Flagged))", "* 3 FETCH (FLAGS (\\Seen \\Flagged))"],
    });
    imap.close();
  });

  test("numbers past the end, and 0, match nothing and complete OK", async () => {
    const { imap, mails } = await inbox(2);
    expect(await store(imap, "3 +FLAGS (\\Seen)")).toEqual({ status: ok, untagged: [] });
    expect(await store(imap, "0 +FLAGS (\\Seen)")).toEqual({ status: ok, untagged: [] });
    expect(await store(imap, "5:9 +FLAGS (\\Seen)")).toEqual({ status: ok, untagged: [] });
    expect(await store(imap, "2:9 +FLAGS (\\Seen)")).toEqual({ status: ok, untagged: ["* 2 FETCH (FLAGS (\\Seen))"] });
    expect(await columns(mails[0]!)).toEqual(CLEAR);
    imap.close();
  });

  test("a message named by two parts of the set is answered once per part", async () => {
    const { imap } = await inbox(3);
    expect(await store(imap, "2:3,3 +FLAGS (\\Seen)")).toEqual({
      status: ok,
      untagged: ["* 2 FETCH (FLAGS (\\Seen))", "* 3 FETCH (FLAGS (\\Seen))", "* 3 FETCH (FLAGS (\\Seen))"],
    });
    imap.close();
  });

  test("on an empty mailbox any set completes OK", async () => {
    const { imap } = await inbox(0);
    expect(await store(imap, "1:* +FLAGS (\\Seen)")).toEqual({ status: ok, untagged: [] });
    expect((await imap.command("UID STORE 1:* +FLAGS (\\Seen)")).lines).toEqual(["A4 OK STORE completed"]);
    imap.close();
  });
});

describe("UID STORE", () => {
  test("addresses by UID and answers with UID before FLAGS", async () => {
    const { imap, mails } = await inbox(3);
    // UIDs 1 and 3 remain, at sequence numbers 1 and 2.
    await imap.command("STORE 2 +FLAGS.SILENT (\\Deleted)");
    await imap.command("EXPUNGE");
    expect((await imap.command("UID STORE 3 +FLAGS (\\Flagged)")).lines).toEqual([
      "* 2 FETCH (UID 3 FLAGS (\\Flagged))",
      "A5 OK STORE completed",
    ]);
    expect(await columns(mails[2]!)).toEqual({ ...CLEAR, saved: true });
    expect((await imap.command("UID STORE 3 -FLAGS.SILENT (\\Flagged)")).lines).toEqual(["A6 OK STORE completed"]);
    const all = await imap.command("UID STORE 1:* FLAGS (\\Seen)");
    expect([...all.untagged].sort()).toEqual(["* 1 FETCH (UID 1 FLAGS (\\Seen))", "* 2 FETCH (UID 3 FLAGS (\\Seen))"]);
    imap.close();
  });

  test("an expunged or unknown UID matches nothing and completes OK", async () => {
    const { imap } = await inbox(3);
    await imap.command("STORE 2 +FLAGS.SILENT (\\Deleted)");
    await imap.command("EXPUNGE");
    expect((await imap.command("UID STORE 2 +FLAGS (\\Seen)")).lines).toEqual(["A5 OK STORE completed"]);
    expect((await imap.command("UID STORE 99 +FLAGS (\\Seen)")).lines).toEqual(["A6 OK STORE completed"]);
    expect((await imap.command("UID STORE 9:* +FLAGS (\\Seen)")).lines).toEqual(["A7 OK STORE completed"]);
    expect((await imap.command("UID STORE 3:* +FLAGS (\\Seen)")).lines).toEqual([
      "* 2 FETCH (UID 3 FLAGS (\\Seen))",
      "A8 OK STORE completed",
    ]);
    imap.close();
  });
});

describe("STORE: where a flag files the message", () => {
  test("\\Flagged files the message into Starred under a UID of that mailbox; clearing it takes it out", async () => {
    const { imap, mails } = await inbox(2);
    expect(await filedUnder(mails[1]!)).toEqual(["INBOX#2", "INBOX/accounts/hello#2"]);
    await imap.command("STORE 2 +FLAGS.SILENT (\\Flagged)");
    expect(await filedUnder(mails[1]!)).toEqual(["INBOX#2", "INBOX/accounts/hello#2", "Starred#1"]);
    expect((await imap.command("STATUS Starred (MESSAGES UIDNEXT)")).untagged).toEqual([
      '* STATUS "Starred" (MESSAGES 1 UIDNEXT 2)',
    ]);

    await imap.command("STORE 2 -FLAGS.SILENT (\\Flagged)");
    expect(await filedUnder(mails[1]!)).toEqual(["INBOX#2", "INBOX/accounts/hello#2"]);
    expect((await imap.command("STATUS Starred (MESSAGES UIDNEXT)")).untagged).toEqual([
      '* STATUS "Starred" (MESSAGES 0 UIDNEXT 2)',
    ]);

    // Flagging it again files it under a fresh UID; the old one is not reused.
    await imap.command("STORE 2 +FLAGS.SILENT (\\Flagged)");
    expect(await filedUnder(mails[1]!)).toEqual(["INBOX#2", "INBOX/accounts/hello#2", "Starred#2"]);
    imap.close();
  });

  test("the starred message is the same message: it is in INBOX and Starred at once, with one set of flags", async () => {
    const { imap } = await inbox(2);
    await imap.command("STORE 2 +FLAGS.SILENT (\\Flagged)");
    await imap.command("SELECT Starred");
    expect((await imap.command("FETCH 1 (UID FLAGS BODY.PEEK[HEADER.FIELDS (SUBJECT)])")).untagged).toEqual([
      "* 1 FETCH (UID 1 FLAGS (\\Flagged) BODY[HEADER.FIELDS (SUBJECT)] {19}\r\nSubject: Mail 2\r\n\r\n)",
    ]);
    await imap.command("STORE 1 +FLAGS.SILENT (\\Seen)");
    await imap.command("SELECT INBOX");
    expect((await imap.command("FETCH 2 (UID FLAGS)")).untagged).toEqual(["* 2 FETCH (UID 2 FLAGS (\\Seen \\Flagged))"]);
    imap.close();
  });

  test("clearing \\Flagged from inside Starred removes the message from it, with no EXPUNGE sent", async () => {
    // NOTE: the message leaves the selected mailbox but the session is told
    // nothing, so its sequence numbers go stale (the remaining message keeps
    // number 2) until the mailbox is selected again.
    const { imap } = await inbox(2);
    await imap.command("STORE 1:2 +FLAGS.SILENT (\\Flagged)");
    await imap.command("SELECT Starred");
    expect((await imap.command("STORE 1 -FLAGS (\\Flagged)")).lines).toEqual([
      "* 1 FETCH (FLAGS ())",
      "A5 OK STORE completed",
    ]);
    expect((await imap.command("FETCH 1:* (UID FLAGS)")).lines).toEqual([
      "* 2 FETCH (UID 2 FLAGS (\\Flagged))",
      "A6 OK FETCH completed",
    ]);
    expect((await imap.command("SELECT Starred")).untagged[0]).toBe("* 1 EXISTS");
    expect((await imap.command("FETCH 1:* (UID FLAGS)")).untagged).toEqual(["* 1 FETCH (UID 2 FLAGS (\\Flagged))"]);
    imap.close();
  });

  test("\\Deleted files the message into Trash as well; it stays in INBOX until EXPUNGE", async () => {
    const { imap, mails } = await inbox(2);
    await imap.command("STORE 1 +FLAGS.SILENT (\\Deleted)");
    expect(await filedUnder(mails[0]!)).toEqual(["INBOX#1", "INBOX/accounts/hello#1", "Trash#1"]);
    expect((await imap.command("STATUS Trash (MESSAGES)")).untagged).toEqual(['* STATUS "Trash" (MESSAGES 1)']);
    expect((await imap.command("STATUS INBOX (MESSAGES)")).untagged).toEqual(['* STATUS "INBOX" (MESSAGES 2)']);
    await imap.command("STORE 1 -FLAGS.SILENT (\\Deleted)");
    expect(await filedUnder(mails[0]!)).toEqual(["INBOX#1", "INBOX/accounts/hello#1"]);
    expect((await imap.command("STATUS Trash (MESSAGES)")).untagged).toEqual(['* STATUS "Trash" (MESSAGES 0)']);
    imap.close();
  });

  test("\\Draft moves the message out of INBOX into Drafts, keeping its UID, with no EXPUNGE sent", async () => {
    // NOTE: same stale-view problem as above: the session that set \Draft
    // keeps numbering the mailbox as if the message were still there.
    const { imap, mails } = await inbox(3);
    expect((await imap.command("STORE 2 +FLAGS (\\Draft)")).lines).toEqual([
      "* 2 FETCH (FLAGS (\\Draft))",
      "A3 OK STORE completed",
    ]);
    expect(await columns(mails[1]!)).toEqual({ ...CLEAR, draft: true });
    expect((await imap.command("FETCH 1:* (UID)")).lines).toEqual([
      "* 1 FETCH (UID 1)",
      "* 3 FETCH (UID 3)",
      "A4 OK FETCH completed",
    ]);
    expect((await imap.command("STATUS INBOX (MESSAGES)")).untagged).toEqual(['* STATUS "INBOX" (MESSAGES 2)']);
    expect((await imap.command('STATUS "INBOX/accounts/hello" (MESSAGES)')).untagged).toEqual([
      '* STATUS "INBOX/accounts/hello" (MESSAGES 2)',
    ]);

    expect((await imap.command("SELECT Drafts")).untagged[0]).toBe("* 1 EXISTS");
    expect((await imap.command("FETCH 1 (UID FLAGS)")).untagged).toEqual(["* 1 FETCH (UID 2 FLAGS (\\Draft))"]);
    // Clearing the flag there sends it back to INBOX.
    await imap.command("STORE 1 -FLAGS.SILENT (\\Draft)");
    expect((await imap.command("SELECT INBOX")).untagged[0]).toBe("* 3 EXISTS");
    expect((await imap.command("FETCH 2 (UID FLAGS)")).untagged).toEqual(["* 2 FETCH (UID 2 FLAGS ())"]);
    imap.close();
  });

  test("a per-address mailbox stores to the same message INBOX shows", async () => {
    const { imap, mails } = await inbox(2);
    await imap.command('SELECT "INBOX/accounts/hello"');
    expect((await imap.command("STORE 2 +FLAGS (\\Seen \\Answered)")).untagged).toEqual([
      "* 2 FETCH (FLAGS (\\Seen \\Answered))",
    ]);
    expect(await columns(mails[1]!)).toEqual({ ...CLEAR, read: true, answered: true });
    await imap.command("SELECT INBOX");
    expect((await imap.command("FETCH 2 FLAGS")).untagged).toEqual(["* 2 FETCH (FLAGS (\\Seen \\Answered))"]);
    imap.close();
  });
});

describe("STORE: refusals", () => {
  test("on an examined mailbox every form is refused and nothing changes", async () => {
    const { imap, mails } = await inbox(1);
    await imap.command("EXAMINE INBOX");
    const refusal = "NO [READ-ONLY] Mailbox is read-only";
    for (const command of [
      "STORE 1 +FLAGS (\\Seen)",
      "STORE 1 -FLAGS (\\Seen)",
      "STORE 1 FLAGS ()",
      "STORE 1 +FLAGS.SILENT (\\Deleted)",
      "UID STORE 1 +FLAGS (\\Flagged)",
      "STORE 9 +FLAGS (\\Seen)",
      "STORE 1 (UNCHANGEDSINCE 99) +FLAGS (\\Seen)",
    ]) {
      const result = await imap.command(command);
      expect({ command, lines: result.lines }).toEqual({ command, lines: [`${result.tag} ${refusal}`] });
    }
    expect(await columns(mails[0]!)).toEqual(CLEAR);
    imap.close();
  });

  test("with no mailbox selected STORE is BAD", async () => {
    const { imap } = await inbox(1);
    await imap.command("CLOSE");
    expect((await imap.command("STORE 1 +FLAGS (\\Seen)")).lines).toEqual(["A4 BAD No mailbox selected"]);
    expect((await imap.command("UID STORE 1 +FLAGS (\\Seen)")).lines).toEqual(["A5 BAD No mailbox selected"]);
    imap.close();
  });

  test("a second connection sees the new flags the next time it fetches them, not before", async () => {
    const { user, imap } = await inbox(1);
    const other = await openSession(server, user.username, user.password, "INBOX");
    await imap.command("STORE 1 +FLAGS.SILENT (\\Seen \\Flagged)");
    // Nothing is pushed on NOOP.
    expect((await other.command("NOOP")).lines).toEqual(["A3 OK NOOP completed"]);
    expect((await other.command("FETCH 1 FLAGS")).untagged).toEqual(["* 1 FETCH (FLAGS (\\Seen \\Flagged))"]);
    other.close();
    imap.close();
  });
});
