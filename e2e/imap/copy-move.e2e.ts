import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createUser, type TestServer, type TestUser } from "../harness";
import { ImapClient, openSession } from "../harness/imap";
import {
  deliver,
  mailDomain,
  sendOverHttp,
  startImapServer,
  storedUidValidity,
  type DeliveredMail,
} from "./support";

let server: TestServer;
let userCount = 0;

interface Box {
  user: TestUser;
  imap: ImapClient;
  mails: DeliveredMail[];
  validity: number;
}

/**
 * A fresh account with `count` mails for `hello@` (subjects `Mail 1`...), a
 * user mailbox `folder`, and a session with INBOX selected.
 */
const inbox = async (count: number): Promise<Box> => {
  const user = await createUser(server, `copy${++userCount}`);
  const mails: DeliveredMail[] = [];
  for (let i = 0; i < count; i++) {
    mails.push(
      await deliver(server, {
        to: `hello@${mailDomain(server, user.username)}`,
        cc: "Dave <dave@remote.example>",
        subject: `Mail ${i + 1}`,
      })
    );
  }
  const imap = await openSession(server, user.username, user.password, "INBOX");
  await imap.command("CREATE folder");
  return { user, imap, mails, validity: await storedUidValidity(server, user.username) };
};

/** `seq/uid subject (flags)` for every message of `mailbox`, which is left selected. */
const listing = async (imap: ImapClient, mailbox: string): Promise<string[]> => {
  const selected = await imap.command(`SELECT ${mailbox}`);
  if (selected.status !== "OK") throw new Error(`SELECT ${mailbox}: ${selected.tagged}`);
  const result = await imap.command("FETCH 1:* (UID FLAGS BODY.PEEK[HEADER.FIELDS (SUBJECT)])");
  return result.untagged.map((line) => {
    const match = /^\* (\d+) FETCH \(UID (\d+) FLAGS \(([^)]*)\) .*Subject: ([^\r]*)\r\n/s.exec(line)!;
    return `${match[1]}/${match[2]} ${match[4]} (${match[3]})`;
  });
};

const tail = (line: string | undefined) => line?.replace(/^A\d+ /, "");

beforeAll(async () => {
  server = await startImapServer();
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

describe("COPY", () => {
  test("copies a message into a user mailbox and reports COPYUID; the source is untouched", async () => {
    const { imap, validity } = await inbox(2);
    await imap.command("STORE 2 +FLAGS.SILENT (\\Seen \\Answered)");
    const result = await imap.command("COPY 2 folder");
    expect(result.lines).toEqual([`${result.tag} OK [COPYUID ${validity} 2 1] COPY completed`]);
    // The copy carries the flags of the original.
    expect(await listing(imap, "folder")).toEqual(["1/1 Mail 2 (\\Seen \\Answered)"]);
    expect((await imap.command("STATUS folder (MESSAGES UIDNEXT UNSEEN)")).untagged).toEqual([
      '* STATUS "folder" (MESSAGES 1 UIDNEXT 2 UNSEEN 0)',
    ]);
    imap.close();
  });

  test("the copy is a new row with a derived Message-ID, filed under the destination", async () => {
    const { imap, mails, user } = await inbox(1);
    await imap.command("COPY 1 folder");
    const rows = await server.db.query(
      "SELECT message_id, subject, sent, read, expunged, uid_domain, text, html FROM mails WHERE user_id = $1 ORDER BY uid_domain",
      [user.id]
    );
    expect(rows).toHaveLength(2);
    const [source, copy] = rows;
    expect(source!.message_id).toBe(mails[0]!.message_id);
    expect(copy!.message_id).toMatch(/^[0-9a-f]{16}\.copy@server$/);
    expect<Record<string, unknown>>({ ...copy, message_id: "" }).toEqual({ ...source, message_id: "", uid_domain: 2 });
    const filed = await server.db.query(
      "SELECT x.mailbox, x.uid FROM mail_mailbox_uid x JOIN mails m USING (mail_id) WHERE m.user_id = $1 AND m.uid_domain = 2",
      [user.id]
    );
    expect(filed).toEqual([{ mailbox: "folder", uid: "1" }]);
    imap.close();
  });

  test("the copy is not the same message on the wire: its envelope names the mailbox, drops Cc and has the derived id", async () => {
    // NOTE: looks like a bug. RFC 3501 6.4.7 copies the message; here the
    // copy's To address becomes <mailbox name>@<account domain>, its Cc
    // addresses are removed from the envelope and its Message-ID is replaced,
    // so a client cannot match the copy to the original by any of them. The
    // header *text* (BODY[HEADER]) keeps the original To and Cc lines.
    const { imap, user } = await inbox(1);
    const domain = mailDomain(server, user.username);
    const original = await imap.command("FETCH 1 (ENVELOPE)");
    expect(original.untagged[0]).toContain(
      `((NIL NIL "hello" "${domain}")) (("Dave" NIL "dave" "remote.example")) NIL NIL "<fixture-`
    );
    await imap.command("COPY 1 folder");
    await imap.command("SELECT folder");
    const copied = await imap.command("FETCH 1 (ENVELOPE BODY.PEEK[HEADER.FIELDS (TO CC)])");
    expect(copied.untagged[0]).toMatch(
      new RegExp(
        `^\\* 1 FETCH \\(ENVELOPE \\("[^"]+" "Mail 1" \\(\\("Sender" NIL "sender" "remote\\.example"\\)\\) ` +
          `\\(\\("Sender" NIL "sender" "remote\\.example"\\)\\) NIL \\(\\(NIL NIL "folder" "${domain.replace(/\./g, "\\.")}"\\)\\) ` +
          `NIL NIL NIL "[0-9a-f]{16}\\.copy@server"\\) BODY\\[HEADER\\.FIELDS \\(TO CC\\)\\] \\{\\d+\\}\\r\\n` +
          `To: hello@${domain.replace(/\./g, "\\.")}\\r\\nCc: "Dave" <dave@remote\\.example>\\r\\n\\r\\n\\)$`
      )
    );
    imap.close();
  });

  test("copying the same message to the same mailbox again answers with the first copy's UID and adds nothing", async () => {
    const { imap, validity } = await inbox(2);
    expect(tail((await imap.command("COPY 1 folder")).tagged)).toBe(`OK [COPYUID ${validity} 1 1] COPY completed`);
    expect(tail((await imap.command("COPY 1 folder")).tagged)).toBe(`OK [COPYUID ${validity} 1 1] COPY completed`);
    expect((await imap.command("STATUS folder (MESSAGES)")).untagged).toEqual(['* STATUS "folder" (MESSAGES 1)']);
    // The repeated copy still used up a UID of the destination.
    expect(tail((await imap.command("COPY 2 folder")).tagged)).toBe(`OK [COPYUID ${validity} 2 3] COPY completed`);
    expect(await listing(imap, "folder")).toEqual(["1/1 Mail 1 ()", "2/3 Mail 2 ()"]);
    imap.close();
  });

  test("a set is copied in ascending order, and COPYUID lists both sides as compact sets", async () => {
    const { imap, validity } = await inbox(5);
    expect(tail((await imap.command("COPY 2:3 folder")).tagged)).toBe(`OK [COPYUID ${validity} 2:3 1:2] COPY completed`);
    expect(tail((await imap.command("COPY 5,1 folder")).tagged)).toBe(`OK [COPYUID ${validity} 1,5 3:4] COPY completed`);
    // Message 5 was copied above (to UID 4), so only message 4 is new (UID 5).
    // Both sides are sorted on their own, so here the sets no longer pair up
    // position by position: 4 went to 5 and 5 to 4.
    expect(tail((await imap.command("COPY 4:* folder")).tagged)).toBe(
      `OK [COPYUID ${validity} 4:5 4:5] COPY completed`
    );
    imap.close();
  });

  test("numbers past the end copy nothing: OK without COPYUID", async () => {
    const { imap, validity } = await inbox(2);
    expect((await imap.command("COPY 9 folder")).lines).toEqual(["A4 OK COPY completed"]);
    expect((await imap.command("COPY 5:9 folder")).lines).toEqual(["A5 OK COPY completed"]);
    expect((await imap.command("COPY 0 folder")).lines).toEqual(["A6 OK COPY completed"]);
    // A range that starts inside the mailbox is cut at its end.
    expect((await imap.command("COPY 2:9 folder")).lines).toEqual([`A7 OK [COPYUID ${validity} 2 1] COPY completed`]);
    imap.close();
  });

  test("UID COPY takes UIDs; unknown UIDs copy nothing", async () => {
    const { imap, validity } = await inbox(3);
    await imap.command("STORE 1 +FLAGS.SILENT (\\Deleted)");
    await imap.command("EXPUNGE");
    expect((await imap.command("UID COPY 3 folder")).lines).toEqual([`A6 OK [COPYUID ${validity} 3 1] COPY completed`]);
    expect((await imap.command("UID COPY 1 folder")).lines).toEqual(["A7 OK COPY completed"]);
    expect((await imap.command("UID COPY 99 folder")).lines).toEqual(["A8 OK COPY completed"]);
    expect((await imap.command("UID COPY 2:* folder")).lines).toEqual([
      `A9 OK [COPYUID ${validity} 2:3 1:2] COPY completed`,
    ]);
    imap.close();
  });

  test("a destination that does not exist is refused with TRYCREATE, and is not created", async () => {
    const { imap } = await inbox(1);
    for (const name of ["no-such-box", "Folder", '"INBOX/accounts/nobody"', '"Sent Messages"', '""']) {
      const result = await imap.command(`COPY 1 ${name}`);
      expect({ name, lines: result.lines }).toEqual({
        name,
        lines: [`${result.tag} NO [TRYCREATE] Mailbox does not exist`],
      });
    }
    expect((await imap.command('LIST "" "no-such-box"')).untagged).toEqual([]);
    imap.close();
  });

  test("with no mailbox selected COPY and MOVE are refused", async () => {
    const { imap } = await inbox(1);
    await imap.command("CLOSE");
    for (const command of ["COPY 1 folder", "UID COPY 1 folder", "MOVE 1 folder", "UID MOVE 1 folder"]) {
      const result = await imap.command(command);
      expect(result.lines).toEqual([`${result.tag} NO Not authenticated or no mailbox selected.`]);
    }
    imap.close();
  });

  test("the session's own view does not change: nothing untagged is sent", async () => {
    const { imap } = await inbox(2);
    const result = await imap.command("COPY 1:2 folder");
    expect(result.untagged).toEqual([]);
    expect((await imap.command("NOOP")).lines).toEqual(["A5 OK NOOP completed"]);
    expect((await imap.command("FETCH 1:* UID")).untagged).toEqual(["* 1 FETCH (UID 1)", "* 2 FETCH (UID 2)"]);
    imap.close();
  });
});

describe("COPY: where each kind of destination puts the copy", () => {
  test("INBOX shows every copy made into a mailbox of received mail", async () => {
    // NOTE: INBOX is the view of all received mail of the account, so a copy
    // filed into a user mailbox (or a per-address one, Trash or Starred) is
    // also a new message in INBOX, with the next INBOX UID.
    const { imap, validity } = await inbox(1);
    expect(tail((await imap.command("COPY 1 folder")).tagged)).toBe(`OK [COPYUID ${validity} 1 1] COPY completed`);
    expect(await listing(imap, "INBOX")).toEqual(["1/1 Mail 1 ()", "2/2 Mail 1 ()"]);
    expect((await imap.command("STATUS INBOX (MESSAGES UIDNEXT)")).untagged).toEqual([
      '* STATUS "INBOX" (MESSAGES 2 UIDNEXT 3)',
    ]);
    imap.close();
  });

  test("a copy into a user mailbox makes a per-address mailbox of the same name appear", async () => {
    // NOTE: looks like a bug. The copy is re-addressed to <mailbox>@<domain>,
    // and LIST shows one INBOX/accounts/<local part> for every address that
    // has received mail, so copying into "folder" invents the mailbox
    // INBOX/accounts/folder. It is selectable and empty: the copy is filed
    // under "folder" only.
    const { imap } = await inbox(1);
    expect((await imap.command('LIST "" "INBOX/accounts/%"')).untagged).toEqual([
      '* LIST (\\HasNoChildren) "/" "INBOX/accounts/hello"',
    ]);
    await imap.command("COPY 1 folder");
    // Sorted: both addresses' latest mail carries the same Date, which is what LIST orders by.
    expect((await imap.command('LIST "" "INBOX/accounts/%"')).untagged.sort()).toEqual([
      '* LIST (\\HasNoChildren) "/" "INBOX/accounts/folder"',
      '* LIST (\\HasNoChildren) "/" "INBOX/accounts/hello"',
    ]);
    expect((await imap.command('STATUS "INBOX/accounts/folder" (MESSAGES)')).untagged).toEqual([
      '* STATUS "INBOX/accounts/folder" (MESSAGES 0)',
    ]);
    imap.close();
  });

  test("into INBOX itself, in any letter case: a second message with the next UID", async () => {
    const { imap, validity } = await inbox(1);
    expect(tail((await imap.command("COPY 1 INBOX")).tagged)).toBe(`OK [COPYUID ${validity} 1 2] COPY completed`);
    // The same message copied to the same mailbox again: the same answer, nothing added.
    expect(tail((await imap.command("COPY 1 inbox")).tagged)).toBe(`OK [COPYUID ${validity} 1 2] COPY completed`);
    expect(await listing(imap, "INBOX")).toEqual(["1/1 Mail 1 ()", "2/2 Mail 1 ()"]);
    imap.close();
  });

  test("into a per-address mailbox: the copy is addressed to that address", async () => {
    const { imap, user, validity } = await inbox(1);
    const domain = mailDomain(server, user.username);
    await deliver(server, { to: `work@${domain}`, subject: "For work" });
    expect(tail((await imap.command('COPY 1 "INBOX/accounts/work"')).tagged)).toBe(
      `OK [COPYUID ${validity} 1 2] COPY completed`
    );
    expect(await listing(imap, '"INBOX/accounts/work"')).toEqual(["1/1 For work ()", "2/2 Mail 1 ()"]);
    const copied = await imap.command("FETCH 2 ENVELOPE");
    expect(copied.untagged[0]).toContain(`NIL ((NIL NIL "work" "${domain}")) NIL NIL NIL "`);
    // The source's own per-address mailbox is unchanged.
    expect(await listing(imap, '"INBOX/accounts/hello"')).toEqual(["1/1 Mail 1 ()"]);
    imap.close();
  });

  test("into Trash sets \\Deleted on the copy, into Starred \\Flagged; each has its own UIDs", async () => {
    const { imap, validity } = await inbox(1);
    expect(tail((await imap.command("COPY 1 Trash")).tagged)).toBe(`OK [COPYUID ${validity} 1 1] COPY completed`);
    expect(tail((await imap.command("COPY 1 starred")).tagged)).toBe(`OK [COPYUID ${validity} 1 1] COPY completed`);
    expect(await listing(imap, "Trash")).toEqual(["1/1 Mail 1 (\\Deleted)"]);
    expect(await listing(imap, "Starred")).toEqual(["1/1 Mail 1 (\\Flagged)"]);
    // The original has neither flag.
    expect(await listing(imap, "INBOX")).toEqual(["1/1 Mail 1 ()", "2/2 Mail 1 (\\Deleted)", "3/3 Mail 1 (\\Flagged)"]);
    imap.close();
  });

  test("into Drafts or Junk: the copy takes the next INBOX UID but is not shown in INBOX", async () => {
    const { imap, user, validity } = await inbox(1);
    expect(tail((await imap.command("COPY 1 Drafts")).tagged)).toBe(`OK [COPYUID ${validity} 1 2] COPY completed`);
    expect(tail((await imap.command("COPY 1 Junk")).tagged)).toBe(`OK [COPYUID ${validity} 1 3] COPY completed`);
    expect(await listing(imap, "Drafts")).toEqual(["1/2 Mail 1 (\\Draft)"]);
    expect(await listing(imap, "Junk")).toEqual(["1/3 Mail 1 ()"]);
    expect(await listing(imap, "INBOX")).toEqual(["1/1 Mail 1 ()"]);
    const rows = await server.db.query(
      "SELECT uid_domain, draft, is_spam FROM mails WHERE user_id = $1 ORDER BY uid_domain",
      [user.id]
    );
    expect(rows).toEqual([
      { uid_domain: 1, draft: false, is_spam: false },
      { uid_domain: 2, draft: true, is_spam: false },
      { uid_domain: 3, draft: false, is_spam: true },
    ]);
    imap.close();
  });

  test("into the sent mailboxes: the copy is stored as sent mail, with UIDs of the sent side", async () => {
    const { imap, user, validity } = await inbox(1);
    await sendOverHttp(server, user, { sender: "me", to: "x@remote.example", subject: "Sent one", html: "<p>s</p>" });
    expect(tail((await imap.command('COPY 1 "Sent Messages"')).tagged)).toBe(
      `OK [COPYUID ${validity} 1 2] COPY completed`
    );
    expect(tail((await imap.command('COPY 1 "Sent Messages/accounts/me"')).tagged)).toBe(
      `OK [COPYUID ${validity} 1 2] COPY completed`
    );
    expect(await listing(imap, '"Sent Messages"')).toEqual(["1/1 Sent one (\\Seen)", "2/2 Mail 1 ()", "3/3 Mail 1 ()"]);
    expect(await listing(imap, '"Sent Messages/accounts/me"')).toEqual(["1/1 Sent one (\\Seen)", "2/2 Mail 1 ()"]);
    // Sent copies are not received mail: INBOX is unchanged.
    expect(await listing(imap, "INBOX")).toEqual(["1/1 Mail 1 ()"]);
    imap.close();
  });

  test("out of a sent mailbox into INBOX", async () => {
    const { imap, user, validity } = await inbox(1);
    await sendOverHttp(server, user, { sender: "me", to: "x@remote.example", subject: "Sent one", html: "<p>s</p>" });
    await imap.command('SELECT "Sent Messages"');
    expect(tail((await imap.command("COPY 1 INBOX")).tagged)).toBe(`OK [COPYUID ${validity} 1 2] COPY completed`);
    expect(await listing(imap, "INBOX")).toEqual(["1/1 Mail 1 ()", "2/2 Sent one (\\Seen)"]);
    expect(await listing(imap, '"Sent Messages"')).toEqual(["1/1 Sent one (\\Seen)"]);
    imap.close();
  });

  test("into the \\Noselect accounts parent succeeds", async () => {
    // NOTE: looks like a bug. INBOX/accounts cannot be selected, yet COPY
    // files a copy under it (addressed to "INBOX/accounts@<domain>"), where
    // only INBOX will ever show it.
    const { imap, validity } = await inbox(1);
    expect(tail((await imap.command('COPY 1 "INBOX/accounts"')).tagged)).toBe(
      `OK [COPYUID ${validity} 1 1] COPY completed`
    );
    expect((await imap.command('STATUS "INBOX/accounts" (MESSAGES)')).untagged).toEqual([
      '* STATUS "INBOX/accounts" (MESSAGES 1)',
    ]);
    expect(await listing(imap, "INBOX")).toEqual(["1/1 Mail 1 ()", "2/2 Mail 1 ()"]);
    imap.close();
  });
});

describe("MOVE", () => {
  /** Two mails copied into `folder`, which is then selected; `other` exists and is empty. */
  const folderWithTwo = async () => {
    const box = await inbox(2);
    await box.imap.command("CREATE other");
    await box.imap.command("COPY 1:2 folder");
    await box.imap.command("SELECT folder");
    return box;
  };

  test("announces the EXPUNGE, then completes with COPYUID", async () => {
    const { imap, validity } = await folderWithTwo();
    await imap.command("STORE 1 +FLAGS.SILENT (\\Seen)");
    const result = await imap.command("MOVE 1 other");
    expect(result.lines).toEqual(["* 1 EXPUNGE", `${result.tag} OK [COPYUID ${validity} 1 1] MOVE completed`]);
    // The session's view is renumbered: what was message 2 is now message 1.
    expect((await imap.command("FETCH 1:* UID")).untagged).toEqual(["* 1 FETCH (UID 2)"]);
    expect(await listing(imap, "other")).toEqual(["1/1 Mail 1 (\\Seen)"]);
    expect(await listing(imap, "folder")).toEqual(["1/2 Mail 2 ()"]);
    imap.close();
  });

  test("the source row is marked expunged, not deleted, and keeps its \\Deleted flag clear", async () => {
    const { imap, user } = await folderWithTwo();
    await imap.command("MOVE 1 other");
    const rows = await server.db.query(
      `SELECT m.subject, m.deleted, m.expunged, x.mailbox FROM mails m JOIN mail_mailbox_uid x USING (mail_id)
       WHERE m.user_id = $1 AND x.mailbox IN ('folder', 'other') ORDER BY x.mailbox, x.uid`,
      [user.id]
    );
    expect(rows).toEqual([
      { subject: "Mail 1", deleted: false, expunged: true, mailbox: "folder" },
      { subject: "Mail 2", deleted: false, expunged: false, mailbox: "folder" },
      { subject: "Mail 1", deleted: false, expunged: false, mailbox: "other" },
    ]);
    imap.close();
  });

  test("several messages are expunged from the highest number down; COPYUID lists them ascending", async () => {
    const box = await inbox(4);
    const { imap, validity } = box;
    await imap.command("CREATE other");
    await imap.command("COPY 1:4 folder");
    await imap.command("SELECT folder");
    const result = await imap.command("MOVE 2:4 other");
    expect(result.lines).toEqual([
      "* 4 EXPUNGE",
      "* 3 EXPUNGE",
      "* 2 EXPUNGE",
      `${result.tag} OK [COPYUID ${validity} 2:4 1:3] MOVE completed`,
    ]);
    expect(await listing(imap, "other")).toEqual(["1/1 Mail 2 ()", "2/2 Mail 3 ()", "3/3 Mail 4 ()"]);
    imap.close();
  });

  test("a set with gaps expunges exactly its messages", async () => {
    const box = await inbox(4);
    const { imap, validity } = box;
    await imap.command("CREATE other");
    await imap.command("COPY 1:4 folder");
    await imap.command("SELECT folder");
    const result = await imap.command("MOVE 4,1 other");
    expect(result.lines).toEqual(["* 4 EXPUNGE", "* 1 EXPUNGE", `${result.tag} OK [COPYUID ${validity} 1,4 1:2] MOVE completed`]);
    expect((await imap.command("FETCH 1:* UID")).untagged).toEqual(["* 1 FETCH (UID 2)", "* 2 FETCH (UID 3)"]);
    imap.close();
  });

  test("UID MOVE takes UIDs", async () => {
    const { imap, validity } = await folderWithTwo();
    const result = await imap.command("UID MOVE 2 other");
    expect(result.lines).toEqual(["* 2 EXPUNGE", `${result.tag} OK [COPYUID ${validity} 2 1] MOVE completed`]);
    expect((await imap.command("UID MOVE 2 other")).lines).toEqual(["A8 OK MOVE completed"]);
    expect((await imap.command("UID MOVE 99 other")).lines).toEqual(["A9 OK MOVE completed"]);
    imap.close();
  });

  test("does not touch other messages flagged \\Deleted", async () => {
    const { imap } = await folderWithTwo();
    await imap.command("STORE 2 +FLAGS.SILENT (\\Deleted)");
    const result = await imap.command("MOVE 1 other");
    expect(result.untagged).toEqual(["* 1 EXPUNGE"]);
    expect((await imap.command("FETCH 1:* (UID FLAGS)")).untagged).toEqual(["* 1 FETCH (UID 2 FLAGS (\\Deleted))"]);
    imap.close();
  });

  test("to the selected mailbox is a no-op answered OK", async () => {
    const { imap } = await folderWithTwo();
    expect((await imap.command("MOVE 1:2 folder")).lines).toEqual(["A7 OK MOVE completed"]);
    expect((await imap.command("FETCH 1:* UID")).untagged).toEqual(["* 1 FETCH (UID 1)", "* 2 FETCH (UID 2)"]);
    imap.close();
  });

  test("numbers past the end move nothing: OK without COPYUID", async () => {
    const { imap } = await folderWithTwo();
    expect((await imap.command("MOVE 9 other")).lines).toEqual(["A7 OK MOVE completed"]);
    expect((await imap.command("MOVE 3:9 other")).lines).toEqual(["A8 OK MOVE completed"]);
    imap.close();
  });

  test("a destination that does not exist is refused with TRYCREATE and nothing is expunged", async () => {
    const { imap } = await folderWithTwo();
    expect((await imap.command("MOVE 1 no-such-box")).lines).toEqual(["A7 NO [TRYCREATE] Mailbox does not exist"]);
    expect((await imap.command("FETCH 1:* UID")).untagged).toEqual(["* 1 FETCH (UID 1)", "* 2 FETCH (UID 2)"]);
    imap.close();
  });

  test("is refused on an examined mailbox", async () => {
    const { imap } = await folderWithTwo();
    await imap.command("EXAMINE folder");
    expect((await imap.command("MOVE 1 other")).lines).toEqual(["A8 NO [READ-ONLY] Mailbox is read-only"]);
    expect((await imap.command("UID MOVE 1 other")).lines).toEqual(["A9 NO [READ-ONLY] Mailbox is read-only"]);
    // Even the no-op form and an unknown destination.
    expect((await imap.command("MOVE 1 folder")).lines).toEqual(["A10 NO [READ-ONLY] Mailbox is read-only"]);
    expect((await imap.command("MOVE 1 no-such-box")).lines).toEqual(["A11 NO [READ-ONLY] Mailbox is read-only"]);
    imap.close();
  });

  test("into Trash sets \\Deleted on the moved copy", async () => {
    const { imap, validity } = await folderWithTwo();
    const result = await imap.command("MOVE 1 Trash");
    expect(result.lines).toEqual(["* 1 EXPUNGE", `${result.tag} OK [COPYUID ${validity} 1 1] MOVE completed`]);
    expect(await listing(imap, "Trash")).toEqual(["1/1 Mail 1 (\\Deleted)"]);
    imap.close();
  });

  test("out of INBOX: the message leaves under its UID and comes back as the moved copy with a new one", async () => {
    // NOTE: follows from INBOX showing every received mail: a message moved
    // from INBOX into a folder is expunged there, and its copy in the folder
    // is at once a new INBOX message too. A client can never empty INBOX by
    // filing mail into folders.
    const { imap, validity } = await inbox(2);
    const result = await imap.command("MOVE 1 folder");
    expect(result.lines).toEqual(["* 1 EXPUNGE", `${result.tag} OK [COPYUID ${validity} 1 1] MOVE completed`]);
    // The session already counts the copy as message 2, though no EXISTS
    // announced it: the client was told of one message leaving, nothing more.
    expect((await imap.command("FETCH 1:* UID")).untagged).toEqual(["* 1 FETCH (UID 2)", "* 2 FETCH (UID 3)"]);
    expect(await listing(imap, "folder")).toEqual(["1/1 Mail 1 ()"]);
    expect(await listing(imap, "INBOX")).toEqual(["1/2 Mail 2 ()", "2/3 Mail 1 ()"]);
    imap.close();
  });

  test("out of INBOX into Drafts takes the message out of INBOX for good", async () => {
    const { imap, validity } = await inbox(2);
    const result = await imap.command("MOVE 1 Drafts");
    expect(result.lines).toEqual(["* 1 EXPUNGE", `${result.tag} OK [COPYUID ${validity} 1 3] MOVE completed`]);
    expect(await listing(imap, "Drafts")).toEqual(["1/3 Mail 1 (\\Draft)"]);
    expect(await listing(imap, "INBOX")).toEqual(["1/2 Mail 2 ()"]);
    imap.close();
  });
});
