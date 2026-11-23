import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createUser, type TestServer, type TestUser } from "../harness";
import { ImapClient, lit, openSession } from "../harness/imap";
import { deliver, mailDomain, startImapServer, type DeliveredMail } from "./support";

let server: TestServer;
let userCount = 0;

interface Box {
  user: TestUser;
  imap: ImapClient;
  mails: DeliveredMail[];
  domain: string;
}

/**
 * A fresh account with `count` mails and INBOX selected. A new account's
 * mod-sequences start at 1 and each delivery takes the next, so message n
 * starts with MODSEQ n and the first change after them gets `count + 1`.
 */
const inbox = async (count: number): Promise<Box> => {
  const user = await createUser(server, `cond${++userCount}`);
  const domain = mailDomain(server, user.username);
  const mails: DeliveredMail[] = [];
  for (let i = 1; i <= count; i++) mails.push(await deliver(server, { to: `hello@${domain}`, subject: `Mail ${i}` }));
  const imap = await openSession(server, user.username, user.password, "INBOX");
  return { user, imap, mails, domain };
};

const lines = async (imap: ImapClient, command: string) => {
  const result = await imap.command(command);
  return [...result.untagged.sort(), result.tagged!.replace(/^A\d+ /, "")];
};

const modseqCounter = async (user: TestUser) => {
  const [row] = await server.db.query(
    "SELECT last_uid FROM mail_uid_counters WHERE user_id = $1 AND uid_kind = 'modseq'",
    [user.id]
  );
  return Number(row!.last_uid);
};

beforeAll(async () => {
  server = await startImapServer();
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

describe("CONDSTORE: mod-sequences", () => {
  test("each delivered message takes the account's next mod-sequence, stored on its row", async () => {
    const { imap, user, mails } = await inbox(3);
    expect(mails.map((mail) => Number(mail.modseq))).toEqual([1, 2, 3]);
    expect(await modseqCounter(user)).toBe(3);
    expect(await lines(imap, "FETCH 1:* (UID MODSEQ)")).toEqual([
      "* 1 FETCH (UID 1 MODSEQ (1))",
      "* 2 FETCH (UID 2 MODSEQ (2))",
      "* 3 FETCH (UID 3 MODSEQ (3))",
      "OK FETCH completed",
    ]);
    imap.close();
  });

  test("SELECT, EXAMINE and STATUS report the highest mod-sequence, and it grows with each change", async () => {
    const { imap } = await inbox(2);
    const highest = async () => {
      const selected = await imap.command("SELECT INBOX");
      const status = await imap.command("STATUS INBOX (HIGHESTMODSEQ)");
      return [selected.untagged.find((line) => line.includes("HIGHESTMODSEQ")), status.untagged[0]];
    };
    expect(await highest()).toEqual([
      "* OK [HIGHESTMODSEQ 2] Highest mod-sequence",
      '* STATUS "INBOX" (HIGHESTMODSEQ 2)',
    ]);
    await imap.command("STORE 1 +FLAGS.SILENT (\\Seen)");
    expect(await highest()).toEqual([
      "* OK [HIGHESTMODSEQ 3] Highest mod-sequence",
      '* STATUS "INBOX" (HIGHESTMODSEQ 3)',
    ]);
    const examined = await imap.command("EXAMINE INBOX");
    expect(examined.untagged).toContain("* OK [HIGHESTMODSEQ 3] Highest mod-sequence");
    imap.close();
  });

  test("an APPEND, a COPY and an EXPUNGE each take a mod-sequence", async () => {
    const { imap, user } = await inbox(1);
    await imap.command(["APPEND INBOX ", lit("Subject: appended\r\n\r\nx\r\n")]);
    expect(await modseqCounter(user)).toBe(2);
    await imap.command("CREATE folder");
    await imap.command("SELECT INBOX");
    await imap.command("COPY 1 folder");
    expect(await modseqCounter(user)).toBe(3);
    // The STORE takes one, the EXPUNGE another.
    await imap.command("STORE 1 +FLAGS.SILENT (\\Deleted)");
    await imap.command("EXPUNGE");
    expect(await modseqCounter(user)).toBe(5);
    expect((await imap.command("FETCH 1:* (UID MODSEQ)")).untagged).toEqual([
      "* 1 FETCH (UID 2 MODSEQ (2))",
      "* 2 FETCH (UID 3 MODSEQ (3))",
    ]);
    imap.close();
  });

  test("the implicit \\Seen of a body fetch takes one too", async () => {
    const { imap, mails } = await inbox(2);
    await imap.command("FETCH 1 (BODY[TEXT])");
    expect((await imap.command("FETCH 1:2 (MODSEQ)")).untagged).toEqual(["* 1 FETCH (MODSEQ (3))", "* 2 FETCH (MODSEQ (2))"]);
    const [row] = await server.db.query("SELECT read, modseq FROM mails WHERE mail_id = $1", [mails[0]!.mail_id]);
    expect(row).toEqual({ read: true, modseq: "3" });
    imap.close();
  });
});

describe("CONDSTORE: MODSEQ in FETCH and STORE answers", () => {
  test("without CONDSTORE enabled, MODSEQ appears only when asked for", async () => {
    const { imap } = await inbox(1);
    expect((await imap.command("FETCH 1 (FLAGS)")).untagged).toEqual(["* 1 FETCH (FLAGS ())"]);
    expect((await imap.command("FETCH 1 (FLAGS MODSEQ)")).untagged).toEqual(["* 1 FETCH (FLAGS () MODSEQ (1))"]);
    expect((await imap.command("STORE 1 +FLAGS (\\Seen)")).untagged).toEqual(["* 1 FETCH (FLAGS (\\Seen))"]);
    expect((await imap.command("FETCH 1 (FLAGS)")).untagged).toEqual(["* 1 FETCH (FLAGS (\\Seen))"]);
    imap.close();
  });

  test("ENABLE CONDSTORE adds MODSEQ to every FETCH answer and to every STORE answer", async () => {
    const { imap } = await inbox(2);
    expect((await imap.command("ENABLE CONDSTORE")).lines).toEqual(["* ENABLED CONDSTORE", "A3 OK ENABLE completed"]);
    expect((await imap.command("FETCH 1:2 (FLAGS)")).untagged).toEqual([
      "* 1 FETCH (FLAGS () MODSEQ (1))",
      "* 2 FETCH (FLAGS () MODSEQ (2))",
    ]);
    expect((await imap.command("UID FETCH 2 (FLAGS)")).untagged).toEqual(["* 2 FETCH (UID 2 FLAGS () MODSEQ (2))"]);
    expect((await imap.command("FETCH 1 (UID MODSEQ FLAGS)")).untagged).toEqual(["* 1 FETCH (UID 1 FLAGS () MODSEQ (1))"]);
    expect((await imap.command("STORE 1 +FLAGS (\\Seen)")).untagged).toEqual(["* 1 FETCH (FLAGS (\\Seen) MODSEQ (3))"]);
    expect((await imap.command("UID STORE 2 +FLAGS (\\Flagged)")).untagged).toEqual([
      "* 2 FETCH (UID 2 FLAGS (\\Flagged) MODSEQ (4))",
    ]);
    imap.close();
  });

  test("with CONDSTORE enabled a .SILENT store still reports the new MODSEQ, without FLAGS", async () => {
    const { imap } = await inbox(1);
    await imap.command("ENABLE CONDSTORE");
    expect((await imap.command("STORE 1 +FLAGS.SILENT (\\Seen)")).lines).toEqual([
      "* 1 FETCH (MODSEQ (2))",
      "A4 OK STORE completed",
    ]);
    expect((await imap.command("UID STORE 1 FLAGS.SILENT ()")).lines).toEqual([
      "* 1 FETCH (UID 1 MODSEQ (3))",
      "A5 OK STORE completed",
    ]);
    imap.close();
  });

  test("it is enabled for the connection only, and survives selecting another mailbox", async () => {
    const { imap, user } = await inbox(1);
    const other = await openSession(server, user.username, user.password, "INBOX");
    await imap.command("ENABLE CONDSTORE");
    expect((await other.command("FETCH 1 (FLAGS)")).untagged).toEqual(["* 1 FETCH (FLAGS ())"]);
    await imap.command('SELECT "INBOX/accounts/hello"');
    expect((await imap.command("FETCH 1 (FLAGS)")).untagged).toEqual(["* 1 FETCH (FLAGS () MODSEQ (1))"]);
    other.close();
    imap.close();
  });
});

describe("CONDSTORE: FETCH (CHANGEDSINCE)", () => {
  test("returns only messages changed after the given mod-sequence, MODSEQ included", async () => {
    const { imap } = await inbox(4);
    await imap.command("STORE 2 +FLAGS.SILENT (\\Seen)"); // 5
    await imap.command("STORE 4 +FLAGS.SILENT (\\Flagged)"); // 6
    expect(await lines(imap, "FETCH 1:* (FLAGS) (CHANGEDSINCE 4)")).toEqual([
      "* 2 FETCH (FLAGS (\\Seen) MODSEQ (5))",
      "* 4 FETCH (FLAGS (\\Flagged) MODSEQ (6))",
      "OK FETCH completed",
    ]);
    expect(await lines(imap, "FETCH 1:* (FLAGS) (CHANGEDSINCE 5)")).toEqual([
      "* 4 FETCH (FLAGS (\\Flagged) MODSEQ (6))",
      "OK FETCH completed",
    ]);
    expect(await lines(imap, "FETCH 1:* (FLAGS) (CHANGEDSINCE 6)")).toEqual(["OK FETCH completed"]);
    expect(await lines(imap, "FETCH 1:* (FLAGS) (CHANGEDSINCE 999999)")).toEqual(["OK FETCH completed"]);
    imap.close();
  });

  test("CHANGEDSINCE 0 returns every message", async () => {
    const { imap } = await inbox(2);
    expect(await lines(imap, "FETCH 1:* (UID) (CHANGEDSINCE 0)")).toEqual([
      "* 1 FETCH (UID 1 MODSEQ (1))",
      "* 2 FETCH (UID 2 MODSEQ (2))",
      "OK FETCH completed",
    ]);
    imap.close();
  });

  test("works with UID FETCH, a single item without parentheses, and in lower case", async () => {
    const { imap } = await inbox(3);
    await imap.command("STORE 3 +FLAGS.SILENT (\\Answered)"); // 4
    expect(await lines(imap, "UID FETCH 1:* (FLAGS) (CHANGEDSINCE 3)")).toEqual([
      "* 3 FETCH (UID 3 FLAGS (\\Answered) MODSEQ (4))",
      "OK FETCH completed",
    ]);
    expect(await lines(imap, "FETCH 1:* FLAGS (CHANGEDSINCE 3)")).toEqual([
      "* 3 FETCH (FLAGS (\\Answered) MODSEQ (4))",
      "OK FETCH completed",
    ]);
    expect(await lines(imap, "fetch 1:* (flags modseq) (changedsince 3)")).toEqual([
      "* 3 FETCH (FLAGS (\\Answered) MODSEQ (4))",
      "OK FETCH completed",
    ]);
    imap.close();
  });

  test("using it enables CONDSTORE for the rest of the connection", async () => {
    const { imap } = await inbox(2);
    expect((await imap.command("FETCH 1 (FLAGS)")).untagged).toEqual(["* 1 FETCH (FLAGS ())"]);
    await imap.command("FETCH 1:* (UID) (CHANGEDSINCE 2)");
    expect((await imap.command("FETCH 1 (FLAGS)")).untagged).toEqual(["* 1 FETCH (FLAGS () MODSEQ (1))"]);
    expect((await imap.command("STORE 1 +FLAGS.SILENT (\\Seen)")).untagged).toEqual(["* 1 FETCH (MODSEQ (3))"]);
    // ENABLE now has nothing left to turn on.
    expect((await imap.command("ENABLE CONDSTORE")).untagged).toEqual(["* ENABLED"]);
    imap.close();
  });

  test("an empty modifier group changes nothing; a malformed or unknown one is BAD", async () => {
    const { imap } = await inbox(1);
    expect(await lines(imap, "FETCH 1 (UID) ()")).toEqual(["* 1 FETCH (UID 1)", "OK FETCH completed"]);
    expect(await lines(imap, "FETCH 1 (UID) (CHANGEDSINCE)")).toEqual([
      "BAD CHANGEDSINCE requires a mod-sequence value",
    ]);
    expect(await lines(imap, "FETCH 1 (UID) (CHANGEDSINCE abc)")).toEqual([
      "BAD CHANGEDSINCE requires a mod-sequence value",
    ]);
    expect(await lines(imap, "FETCH 1 (UID) (BOGUS 5)")).toEqual(["BAD Unknown FETCH modifier: BOGUS"]);
    expect(await lines(imap, "FETCH 1 (UID) (CHANGEDSINCE 5 VANISHED)")).toEqual([
      "BAD Unknown FETCH modifier: VANISHED",
    ]);
    expect(await lines(imap, "FETCH 1 (UID) (CHANGEDSINCE 5")).toEqual(["BAD Unterminated FETCH modifier group"]);
    imap.close();
  });
});

describe("CONDSTORE: STORE (UNCHANGEDSINCE)", () => {
  test("stores when the message has not changed since, and answers with MODSEQ", async () => {
    const { imap, mails } = await inbox(1);
    expect((await imap.command("STORE 1 (UNCHANGEDSINCE 1) +FLAGS (\\Seen)")).lines).toEqual([
      "* 1 FETCH (FLAGS (\\Seen) MODSEQ (2))",
      "A3 OK STORE completed",
    ]);
    const [row] = await server.db.query("SELECT read, modseq FROM mails WHERE mail_id = $1", [mails[0]!.mail_id]);
    expect(row).toEqual({ read: true, modseq: "2" });
    imap.close();
  });

  test("refuses a message changed since, naming it in MODIFIED on an OK completion, and leaves it alone", async () => {
    const { imap, mails } = await inbox(1);
    await imap.command("STORE 1 +FLAGS.SILENT (\\Seen)"); // 2
    expect((await imap.command("STORE 1 (UNCHANGEDSINCE 1) +FLAGS (\\Flagged)")).lines).toEqual([
      "A4 OK [MODIFIED 1] Conditional STORE failed",
    ]);
    const [row] = await server.db.query("SELECT saved, modseq FROM mails WHERE mail_id = $1", [mails[0]!.mail_id]);
    expect(row).toEqual({ saved: false, modseq: "2" });
    imap.close();
  });

  test("over a range, stores the unchanged ones and lists the others as a compact set", async () => {
    const { imap } = await inbox(4);
    await imap.command("STORE 2:3 +FLAGS.SILENT (\\Seen)"); // 5
    const result = await imap.command("STORE 1:4 (UNCHANGEDSINCE 4) +FLAGS (\\Deleted)");
    expect([...result.untagged].sort()).toEqual([
      "* 1 FETCH (FLAGS (\\Deleted) MODSEQ (6))",
      "* 4 FETCH (FLAGS (\\Deleted) MODSEQ (6))",
    ]);
    expect(result.tagged).toBe("A4 OK [MODIFIED 2:3] Conditional STORE failed");
    expect(await lines(imap, "FETCH 1:* (FLAGS)")).toEqual([
      "* 1 FETCH (FLAGS (\\Deleted) MODSEQ (6))",
      "* 2 FETCH (FLAGS (\\Seen) MODSEQ (5))",
      "* 3 FETCH (FLAGS (\\Seen) MODSEQ (5))",
      "* 4 FETCH (FLAGS (\\Deleted) MODSEQ (6))",
      "OK FETCH completed",
    ]);
    imap.close();
  });

  test("UID STORE lists the refused messages by UID", async () => {
    const { imap } = await inbox(3);
    await imap.command("STORE 1 +FLAGS.SILENT (\\Deleted)");
    await imap.command("EXPUNGE"); // UIDs 2 and 3 remain at 1 and 2
    await imap.command("STORE 2 +FLAGS.SILENT (\\Seen)"); // UID 3 changes
    const highest = Number(
      /HIGHESTMODSEQ (\d+)/.exec((await imap.command("STATUS INBOX (HIGHESTMODSEQ)")).untagged[0]!)![1]
    );
    const result = await imap.command(`UID STORE 2:3 (UNCHANGEDSINCE ${highest - 1}) +FLAGS (\\Flagged)`);
    expect(result.lines).toEqual([
      `* 1 FETCH (UID 2 FLAGS (\\Flagged) MODSEQ (${highest + 1}))`,
      `${result.tag} OK [MODIFIED 3] Conditional STORE failed`,
    ]);
    // The plain form names the same message by its sequence number.
    const plain = await imap.command(`STORE 1:2 (UNCHANGEDSINCE ${highest - 1}) +FLAGS (\\Answered)`);
    expect(plain.tagged).toBe(`${plain.tag} OK [MODIFIED 1:2] Conditional STORE failed`);
    imap.close();
  });

  test("a .SILENT conditional store reports MODSEQ for what it changed", async () => {
    const { imap } = await inbox(2);
    expect((await imap.command("STORE 1 (UNCHANGEDSINCE 999999) +FLAGS.SILENT (\\Flagged)")).lines).toEqual([
      "* 1 FETCH (MODSEQ (3))",
      "A3 OK STORE completed",
    ]);
    expect((await imap.command("UID STORE 2 (UNCHANGEDSINCE 2) -FLAGS.SILENT (\\Seen)")).lines).toEqual([
      "* 2 FETCH (UID 2 MODSEQ (4))",
      "A4 OK STORE completed",
    ]);
    imap.close();
  });

  test("UNCHANGEDSINCE 0 refuses every message", async () => {
    const { imap } = await inbox(2);
    expect((await imap.command("STORE 1:2 (UNCHANGEDSINCE 0) +FLAGS (\\Seen)")).lines).toEqual([
      "A3 OK [MODIFIED 1:2] Conditional STORE failed",
    ]);
    imap.close();
  });

  test("a refused conditional STORE still uses up a mod-sequence", async () => {
    const { imap, user } = await inbox(1);
    await imap.command("STORE 1 +FLAGS.SILENT (\\Seen)");
    expect(await modseqCounter(user)).toBe(2);
    await imap.command("STORE 1 (UNCHANGEDSINCE 1) +FLAGS (\\Flagged)");
    expect(await modseqCounter(user)).toBe(3);
    expect((await imap.command("FETCH 1 (MODSEQ)")).untagged).toEqual(["* 1 FETCH (MODSEQ (2))"]);
    imap.close();
  });

  test("using it enables CONDSTORE for the rest of the connection", async () => {
    const { imap } = await inbox(1);
    await imap.command("STORE 1 (UNCHANGEDSINCE 1) +FLAGS.SILENT (\\Seen)");
    expect((await imap.command("FETCH 1 (FLAGS)")).untagged).toEqual(["* 1 FETCH (FLAGS (\\Seen) MODSEQ (2))"]);
    imap.close();
  });

  test("a set that matches no message completes OK", async () => {
    const { imap } = await inbox(1);
    expect((await imap.command("STORE 9 (UNCHANGEDSINCE 1) +FLAGS (\\Seen)")).lines).toEqual(["A3 OK STORE completed"]);
    expect((await imap.command("UID STORE 9 (UNCHANGEDSINCE 1) +FLAGS (\\Seen)")).lines).toEqual([
      "A4 OK STORE completed",
    ]);
    imap.close();
  });

  test("a message named twice in the set is stored once and not reported as modified", async () => {
    const { imap } = await inbox(2);
    const result = await imap.command("STORE 1,1 (UNCHANGEDSINCE 2) +FLAGS (\\Seen)");
    expect(result.lines).toEqual(["* 1 FETCH (FLAGS (\\Seen) MODSEQ (3))", "A3 OK STORE completed"]);
    imap.close();
  });

  test("a malformed or unknown modifier is BAD", async () => {
    const { imap } = await inbox(1);
    const cases: [string, string][] = [
      ["STORE 1 (UNCHANGEDSINCE) +FLAGS (\\Seen)", "BAD UNCHANGEDSINCE requires a mod-sequence value"],
      ["STORE 1 (BOGUS 1) +FLAGS (\\Seen)", "BAD Unknown STORE modifier: BOGUS"],
      ["STORE 1 (CHANGEDSINCE 1) +FLAGS (\\Seen)", "BAD Unknown STORE modifier: CHANGEDSINCE"],
      ["STORE 1 (UNCHANGEDSINCE 1 +FLAGS (\\Seen)", "BAD Unknown STORE modifier: +FLAGS"],
    ];
    for (const [command, expected] of cases) {
      const result = await imap.command(command);
      expect({ command, lines: result.lines }).toEqual({ command, lines: [`${result.tag} ${expected}`] });
    }
    expect((await imap.command("FETCH 1 (FLAGS)")).untagged).toEqual(["* 1 FETCH (FLAGS ())"]);
    imap.close();
  });
});
