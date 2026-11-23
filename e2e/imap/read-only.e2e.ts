import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { ADMIN_PASSWORD, type TestServer } from "../harness";
import { ImapClient, lit, openSession, saslPlain } from "../harness/imap";
import {
  CAPABILITIES,
  deliver,
  FLAGS_LINE,
  PERMANENTFLAGS_LINE,
  raws,
  startImapServer,
  storedUidValidity,
  type DeliveredMail,
} from "./support";

// `admin-ro` signs in with its own password and sees the admin's mail, but may
// change nothing. The admin holds three mails for hello@mail.test, a user
// mailbox `folder` with a copy of the first, and one \Deleted message.
const RO_PASSWORD = "read-only-password-e2e";

let server: TestServer;
let mails: DeliveredMail[];

const readOnly = (mailbox?: string): Promise<ImapClient> => openSession(server, "admin-ro", RO_PASSWORD, mailbox);
const refusal = (command: string) => `NO [READ-ONLY] ${command} not permitted for read-only user (admin-ro).`;

/** The rows of the admin's mails, flags and filing included, to prove nothing changed. */
const snapshot = async () => ({
  mails: await server.db.query(
    `SELECT m.uid_domain, m.read, m.saved, m.deleted, m.draft, m.answered, m.expunged, m.modseq
     FROM mails m JOIN users u USING (user_id) WHERE u.username = 'admin' ORDER BY m.sent, m.uid_domain`
  ),
  filed: await server.db.query(
    `SELECT x.mailbox, x.uid FROM mail_mailbox_uid x JOIN users u USING (user_id)
     WHERE u.username = 'admin' ORDER BY x.mailbox, x.uid`
  ),
  mailboxes: await server.db.query(
    `SELECT b.name, b.subscribed FROM mailboxes b JOIN users u USING (user_id)
     WHERE u.username = 'admin' ORDER BY b.name`
  ),
});

beforeAll(async () => {
  server = await startImapServer({ env: { ADMIN_RO_PASSWORD: RO_PASSWORD } });
  mails = [];
  for (let i = 1; i <= 3; i++) mails.push(await deliver(server, { to: "hello@mail.test", subject: `Mail ${i}` }));
  const admin = await openSession(server, "admin", ADMIN_PASSWORD, "INBOX");
  await admin.command("CREATE folder");
  await admin.command("COPY 1 folder");
  await admin.command("STORE 3 +FLAGS.SILENT (\\Deleted)");
  await admin.logout();
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

describe("the read-only account: signing in", () => {
  test("LOGIN with its own password completes like any other", async () => {
    const imap = await ImapClient.connect(server);
    const result = await imap.command(`LOGIN admin-ro ${RO_PASSWORD}`);
    expect(result.lines).toEqual([`A1 OK [CAPABILITY ${CAPABILITIES}] LOGIN completed`]);
    imap.close();
  });

  test("AUTHENTICATE PLAIN works too", async () => {
    const imap = await ImapClient.connect(server);
    const result = await imap.command(`AUTHENTICATE PLAIN ${saslPlain("admin-ro", RO_PASSWORD)}`);
    expect(result.lines).toEqual([`A1 OK [CAPABILITY ${CAPABILITIES}] AUTHENTICATE completed`]);
    expect((await imap.command("CREATE x")).lines).toEqual([`A2 ${refusal("CREATE")}`]);
    imap.close();
  });

  test("the admin's password does not sign admin-ro in, nor the other way round", async () => {
    // Two failed sign-ins from this address; the file makes no others.
    const imap = await ImapClient.connect(server);
    expect((await imap.command(`LOGIN admin-ro ${ADMIN_PASSWORD}`)).text).toBe(
      "[AUTHENTICATIONFAILED] Invalid credentials."
    );
    expect((await imap.command(`LOGIN admin ${RO_PASSWORD}`)).text).toBe("[AUTHENTICATIONFAILED] Invalid credentials.");
    imap.close();
  });
});

describe("the read-only account: what it may do", () => {
  test("LIST and LSUB show the admin's mailboxes", async () => {
    const imap = await readOnly();
    expect((await imap.command('LIST "" "*"')).untagged).toEqual([
      '* LIST (\\HasChildren) "/" "INBOX"',
      '* LIST (\\Drafts \\HasNoChildren) "/" "Drafts"',
      '* LIST (\\Junk \\HasNoChildren) "/" "Junk"',
      '* LIST (\\Flagged \\HasNoChildren) "/" "Starred"',
      '* LIST (\\Trash \\HasNoChildren) "/" "Trash"',
      '* LIST (\\HasChildren \\Noselect) "/" "INBOX/accounts"',
      '* LIST (\\HasNoChildren) "/" "INBOX/accounts/hello"',
      // The copy filed into `folder` is addressed to folder@<domain>, which lists as an address of its own.
      '* LIST (\\HasNoChildren) "/" "INBOX/accounts/folder"',
      '* LIST (\\HasNoChildren) "/" "folder"',
    ]);
    expect((await imap.command('LSUB "" "folder"')).untagged).toEqual(['* LSUB (\\HasNoChildren) "/" "folder"']);
    imap.close();
  });

  test("STATUS, NAMESPACE, ENABLE, ID, CHECK, NOOP and CAPABILITY work", async () => {
    const imap = await readOnly();
    expect((await imap.command("STATUS INBOX (MESSAGES UNSEEN UIDNEXT)")).untagged).toEqual([
      '* STATUS "INBOX" (MESSAGES 4 UNSEEN 4 UIDNEXT 5)',
    ]);
    expect((await imap.command("NAMESPACE")).status).toBe("OK");
    expect((await imap.command("ENABLE CONDSTORE")).untagged).toEqual(["* ENABLED CONDSTORE"]);
    expect((await imap.command("ID NIL")).untagged).toEqual(["* ID NIL"]);
    expect((await imap.command("CHECK")).status).toBe("OK");
    expect((await imap.command("NOOP")).status).toBe("OK");
    expect((await imap.command("CAPABILITY")).untagged).toEqual([`* CAPABILITY ${CAPABILITIES}`]);
    imap.close();
  });

  test("SELECT is answered READ-ONLY, under the name SELECT", async () => {
    const imap = await readOnly();
    const result = await imap.command("SELECT INBOX");
    const validity = await storedUidValidity(server, "admin");
    expect(result.lines).toEqual([
      "* 4 EXISTS",
      "* 0 RECENT",
      "* OK [UNSEEN 1] Message 1 is first unseen",
      `* OK [UIDVALIDITY ${validity}] UIDs valid`,
      "* OK [UIDNEXT 5] Predicted next UID",
      expect.stringMatching(/^\* OK \[HIGHESTMODSEQ \d+\] Highest mod-sequence$/),
      FLAGS_LINE,
      PERMANENTFLAGS_LINE,
      "A2 OK [READ-ONLY] SELECT completed",
    ]);
    expect((await imap.command("EXAMINE folder")).tagged).toBe("A3 OK [READ-ONLY] EXAMINE completed");
    imap.close();
  });

  test("FETCH and SEARCH work, and a body fetch does not mark the message read", async () => {
    const imap = await readOnly("INBOX");
    const before = await snapshot();
    expect((await imap.command("FETCH 1:* (UID FLAGS)")).untagged).toEqual([
      "* 1 FETCH (UID 1 FLAGS ())",
      "* 2 FETCH (UID 2 FLAGS ())",
      "* 3 FETCH (UID 3 FLAGS (\\Deleted))",
      "* 4 FETCH (UID 4 FLAGS ())",
    ]);
    expect((await imap.command("SEARCH SUBJECT Mail")).untagged).toEqual(["* SEARCH 1 2 3 4"]);
    expect((await imap.command("UID SEARCH DELETED")).untagged).toEqual(["* SEARCH 3"]);
    for (const item of ["BODY[]", "BODY[TEXT]", "BODY[HEADER]", "BODY[1]", "RFC822", "RFC822.TEXT"]) {
      const result = await imap.command(`FETCH 1 (${item})`);
      expect({ item, status: result.status, literals: result.responses[0]!.literals.length }).toEqual({
        item,
        status: "OK",
        literals: 1,
      });
    }
    expect((await imap.command("UID FETCH 1 (FLAGS)")).untagged).toEqual(["* 1 FETCH (UID 1 FLAGS ())"]);
    expect(await snapshot()).toEqual(before);
    imap.close();
  });

  test("IDLE works and hears of new mail", async () => {
    const imap = await readOnly("INBOX");
    const { tag } = await imap.idle();
    const mail = await deliver(server, { to: "hello@mail.test", subject: "While idling" });
    const seen = await imap.waitFor(/EXISTS$/);
    expect(raws(seen)).toEqual(["* 5 EXISTS"]);
    expect((await imap.next()).raw).toBe("* 0 RECENT");
    expect((await imap.done(tag)).lines).toEqual([`${tag} OK IDLE terminated`]);
    imap.close();
    // Put the mailbox back the way the other tests expect it.
    await server.db.query("DELETE FROM mail_mailbox_uid WHERE mail_id = $1", [mail.mail_id]);
    await server.db.query("DELETE FROM mails WHERE mail_id = $1", [mail.mail_id]);
  });

  test("CLOSE, UNSELECT and LOGOUT work", async () => {
    const imap = await readOnly("INBOX");
    expect((await imap.command("CLOSE")).lines).toEqual(["A3 OK CLOSE completed"]);
    await imap.command("SELECT INBOX");
    expect((await imap.command("UNSELECT")).lines).toEqual(["A5 OK UNSELECT completed"]);
    expect((await imap.command("LOGOUT")).lines).toEqual([
      "* BYE IMAP4rev1 Server logging out",
      "A6 OK LOGOUT completed",
    ]);
    await imap.waitForClose();
  });
});

describe("the read-only account: what it is refused", () => {
  test("every mailbox-changing command, naming the command and the account", async () => {
    const imap = await readOnly();
    const before = await snapshot();
    for (const [command, name] of [
      ["CREATE another", "CREATE"],
      ["CREATE INBOX", "CREATE"],
      ["DELETE folder", "DELETE"],
      ["DELETE INBOX", "DELETE"],
      ["RENAME folder renamed", "RENAME"],
      ["SUBSCRIBE folder", "SUBSCRIBE"],
      ["UNSUBSCRIBE folder", "UNSUBSCRIBE"],
    ] as const) {
      const result = await imap.command(command);
      expect({ command, lines: result.lines }).toEqual({ command, lines: [`${result.tag} ${refusal(name)}`] });
    }
    expect(await snapshot()).toEqual(before);
    imap.close();
  });

  test("every message-changing command on a selected mailbox", async () => {
    const imap = await readOnly("INBOX");
    const before = await snapshot();
    for (const [command, name] of [
      ["STORE 1 +FLAGS (\\Seen)", "STORE"],
      ["STORE 1 -FLAGS.SILENT (\\Deleted)", "STORE"],
      ["STORE 99 FLAGS ()", "STORE"],
      ["UID STORE 1 +FLAGS (\\Flagged)", "UID STORE"],
      ["COPY 1 folder", "COPY"],
      ["COPY 1 no-such-box", "COPY"],
      ["UID COPY 1 folder", "UID COPY"],
      ["MOVE 1 folder", "MOVE"],
      ["MOVE 1 INBOX", "MOVE"],
      ["UID MOVE 1 folder", "UID MOVE"],
      ["EXPUNGE", "EXPUNGE"],
    ] as const) {
      const result = await imap.command(command);
      expect({ command, lines: result.lines }).toEqual({ command, lines: [`${result.tag} ${refusal(name)}`] });
    }
    expect(await snapshot()).toEqual(before);
    imap.close();
  });

  test("APPEND, after its literal has been taken", async () => {
    const imap = await readOnly();
    const before = await snapshot();
    const sync = await imap.command(["APPEND INBOX ", lit("Subject: x\r\n\r\nbody\r\n")]);
    expect(sync.lines).toEqual(["+ go ahead", `A2 ${refusal("APPEND")}`]);
    const nonSync = await imap.command(["APPEND no-such-box (\\Seen) ", lit("Subject: x\r\n\r\nbody\r\n", false)]);
    expect(nonSync.lines).toEqual([`A3 ${refusal("APPEND")}`]);
    expect(await snapshot()).toEqual(before);
    imap.close();
  });

  test("with no mailbox selected, the missing-mailbox answer comes first", async () => {
    const imap = await readOnly();
    expect((await imap.command("STORE 1 +FLAGS (\\Seen)")).lines).toEqual(["A2 BAD No mailbox selected"]);
    expect((await imap.command("EXPUNGE")).lines).toEqual(["A3 BAD No mailbox selected"]);
    expect((await imap.command("COPY 1 folder")).lines).toEqual(["A4 NO Not authenticated or no mailbox selected."]);
    expect((await imap.command("MOVE 1 folder")).lines).toEqual(["A5 NO Not authenticated or no mailbox selected."]);
    imap.close();
  });

  test("a syntax error is still reported as one", async () => {
    const imap = await readOnly("INBOX");
    expect((await imap.command("STORE 1 BOGUS (\\Seen)")).lines).toEqual(["A3 BAD Invalid store operation: BOGUS"]);
    expect((await imap.command("CREATE")).lines).toEqual(["A4 BAD Invalid mailbox name in CREATE"]);
    imap.close();
  });

  test("the admin itself is not restricted on the same server", async () => {
    const imap = await openSession(server, "admin", ADMIN_PASSWORD);
    expect((await imap.command("SELECT INBOX")).tagged).toBe("A2 OK [READ-WRITE] SELECT completed");
    expect((await imap.command("CREATE scratch")).lines).toEqual(["A3 OK CREATE completed"]);
    expect((await imap.command("DELETE scratch")).lines).toEqual(["A4 OK DELETE completed"]);
    imap.close();
  });
});

describe("without ADMIN_RO_PASSWORD", () => {
  test("admin-ro cannot sign in", async () => {
    const plain = await startImapServer();
    try {
      const imap = await ImapClient.connect(plain);
      expect((await imap.command(`LOGIN admin-ro ${RO_PASSWORD}`)).lines).toEqual([
        "A1 NO [AUTHENTICATIONFAILED] Invalid credentials.",
      ]);
      imap.close();
    } finally {
      await plain.stop();
    }
  });
});
