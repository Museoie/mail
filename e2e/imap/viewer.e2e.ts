import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { ADMIN_PASSWORD, adminUser, createUser, type TestServer, type TestUser } from "../harness";
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

// `vera` holds the viewer role on `mail.test`, which admin owns. Signing in as
// `vera@mail.test` opens admin's mail of that domain, read-only. The admin
// holds three mails for hello@mail.test, a user mailbox `folder` with a copy
// of the first, and one \Deleted message.
const DOMAIN = "mail.test";

let server: TestServer;
let vera: TestUser;
let mails: DeliveredMail[];

const login = () => `${vera.username}@${DOMAIN}`;
const viewer = (mailbox?: string): Promise<ImapClient> => openSession(server, login(), vera.password, mailbox);
const refusal = (command: string) => `NO [READ-ONLY] ${command} not permitted: ${vera.username} can only read ${DOMAIN}.`;
const notScoped = (command: string) => `NO [CANNOT] ${command} is not available to a login scoped to one domain.`;

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

const share = async () => {
  const admin = await adminUser(server);
  await server.db.query(
    `INSERT INTO domain_roles (domain_id, user_id, role)
     SELECT domain_id, $2, 'viewer' FROM domains WHERE name = $1 AND user_id = $3
     ON CONFLICT DO NOTHING`,
    [DOMAIN, vera.id, admin.id]
  );
};

beforeAll(async () => {
  server = await startImapServer();
  vera = await createUser(server, "vera");
  await share();
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

describe("a viewer: signing in", () => {
  test("LOGIN as <username>@<domain> with its own password completes like any other", async () => {
    const imap = await ImapClient.connect(server);
    const result = await imap.command(`LOGIN ${login()} ${vera.password}`);
    expect(result.lines).toEqual([`A1 OK [CAPABILITY ${CAPABILITIES}] LOGIN completed`]);
    imap.close();
  });

  test("AUTHENTICATE PLAIN works too", async () => {
    const imap = await ImapClient.connect(server);
    const result = await imap.command(`AUTHENTICATE PLAIN ${saslPlain(login(), vera.password)}`);
    expect(result.lines).toEqual([`A1 OK [CAPABILITY ${CAPABILITIES}] AUTHENTICATE completed`]);
    expect((await imap.command("SELECT INBOX")).tagged).toBe("A2 OK [READ-ONLY] SELECT completed");
    imap.close();
  });

  test("the plain login opens the viewer's own store, not the domain", async () => {
    const imap = await openSession(server, vera.username, vera.password);
    expect((await imap.command("STATUS INBOX (MESSAGES)")).untagged).toEqual(['* STATUS "INBOX" (MESSAGES 0)']);
    expect((await imap.command("SELECT INBOX")).tagged).toBe("A3 OK [READ-WRITE] SELECT completed");
    imap.close();
  });

  test("a domain the account holds no role on is refused like a wrong password", async () => {
    // Two failed sign-ins from this address; the file makes no others.
    const stranger = await createUser(server, "stranger");
    const imap = await ImapClient.connect(server);
    expect((await imap.command(`LOGIN stranger@${DOMAIN} ${stranger.password}`)).text).toBe(
      "[AUTHENTICATIONFAILED] Invalid credentials."
    );
    expect((await imap.command(`LOGIN ${login()} ${ADMIN_PASSWORD}`)).text).toBe(
      "[AUTHENTICATIONFAILED] Invalid credentials."
    );
    imap.close();
  });
});

describe("a viewer: what it may do", () => {
  test("LIST shows the domain's views, not the owner's own mailboxes", async () => {
    const imap = await viewer();
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
    ]);
    imap.close();
  });

  test("STATUS, NAMESPACE, ENABLE, ID, CHECK, NOOP and CAPABILITY work", async () => {
    const imap = await viewer();
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
    const imap = await viewer();
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
    imap.close();
  });

  test("FETCH and SEARCH work, and a body fetch does not mark the message read", async () => {
    const imap = await viewer("INBOX");
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
    expect(await snapshot()).toEqual(before);
    imap.close();
  });

  test("IDLE works and hears of new mail to the domain", async () => {
    const imap = await viewer("INBOX");
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
    const imap = await viewer("INBOX");
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

describe("a viewer: what it is refused", () => {
  test("every mailbox-changing command, naming the command, the account and the domain", async () => {
    const imap = await viewer();
    const before = await snapshot();
    for (const [command, name] of [
      ["CREATE another", "CREATE"],
      ["DELETE folder", "DELETE"],
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
    const imap = await viewer("INBOX");
    const before = await snapshot();
    for (const [command, name] of [
      ["STORE 1 +FLAGS (\\Seen)", "STORE"],
      ["STORE 1 -FLAGS.SILENT (\\Deleted)", "STORE"],
      ["UID STORE 1 +FLAGS (\\Flagged)", "UID STORE"],
      ["COPY 1 folder", "COPY"],
      ["UID COPY 1 folder", "UID COPY"],
      ["MOVE 1 folder", "MOVE"],
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
    const imap = await viewer();
    const before = await snapshot();
    const sync = await imap.command(["APPEND INBOX ", lit("Subject: x\r\n\r\nbody\r\n")]);
    expect(sync.lines).toEqual(["+ go ahead", `A2 ${refusal("APPEND")}`]);
    expect(await snapshot()).toEqual(before);
    imap.close();
  });

  test("the owner's scoped login reads and writes the domain, but manages no mailbox", async () => {
    const imap = await openSession(server, `admin@${DOMAIN}`, ADMIN_PASSWORD);
    expect((await imap.command("SELECT INBOX")).tagged).toBe("A2 OK [READ-WRITE] SELECT completed");
    expect((await imap.command("CREATE scratch")).lines).toEqual([`A3 ${notScoped("CREATE")}`]);
    imap.close();
  });

  test("the owner's plain login is not restricted", async () => {
    const imap = await openSession(server, "admin", ADMIN_PASSWORD);
    expect((await imap.command("SELECT INBOX")).tagged).toBe("A2 OK [READ-WRITE] SELECT completed");
    expect((await imap.command("CREATE scratch")).lines).toEqual(["A3 OK CREATE completed"]);
    expect((await imap.command("DELETE scratch")).lines).toEqual(["A4 OK DELETE completed"]);
    imap.close();
  });
});

describe("when the owner stops sharing", () => {
  test("an open session loses the domain at its next command, and cannot sign in again", async () => {
    const imap = await viewer("INBOX");
    expect((await imap.command("NOOP")).status).toBe("OK");
    await server.db.query("DELETE FROM domain_roles WHERE user_id = $1", [vera.id]);
    const next = await imap.command("FETCH 1 (FLAGS)");
    expect(next.tagged).toMatch(/^A\d+ NO \[AUTHENTICATIONFAILED\]/);
    imap.close();
    const again = await ImapClient.connect(server);
    expect((await again.command(`LOGIN ${login()} ${vera.password}`)).text).toBe(
      "[AUTHENTICATIONFAILED] Invalid credentials."
    );
    again.close();
    await share();
  });
});
