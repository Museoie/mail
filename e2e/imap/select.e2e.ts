import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createUser, type TestServer, type TestUser } from "../harness";
import { ImapClient, lit, openSession, type CommandResult } from "../harness/imap";
import {
  deliver,
  FLAGS_LINE,
  PERMANENTFLAGS_LINE,
  sendOverHttp,
  startImapServer,
  storedUidValidity,
} from "./support";

let server: TestServer;
let userCount = 0;

const newUser = (): Promise<TestUser> => createUser(server, `sel${++userCount}`);
const session = (user: TestUser, mailbox?: string): Promise<ImapClient> =>
  openSession(server, user.username, user.password, mailbox);

/** A user with `count` unread mails for `hello@`, delivered in order. */
const userWithMail = async (count: number): Promise<TestUser> => {
  const user = await newUser();
  for (let i = 0; i < count; i++) await deliver(server, { to: `hello@${user.username}.mail.test` });
  return user;
};

/** The untagged block of a SELECT / EXAMINE, as the server writes it. */
const block = (options: {
  exists: number;
  unseen?: number;
  validity: number;
  uidNext: number;
  highestModseq: number;
}): string[] => [
  `* ${options.exists} EXISTS`,
  "* 0 RECENT",
  ...(options.unseen === undefined
    ? []
    : [`* OK [UNSEEN ${options.unseen}] Message ${options.unseen} is first unseen`]),
  `* OK [UIDVALIDITY ${options.validity}] UIDs valid`,
  `* OK [UIDNEXT ${options.uidNext}] Predicted next UID`,
  `* OK [HIGHESTMODSEQ ${options.highestModseq}] Highest mod-sequence`,
  FLAGS_LINE,
  PERMANENTFLAGS_LINE,
];

const untaggedOf = (result: CommandResult) => result.untagged;

beforeAll(async () => {
  server = await startImapServer();
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

describe("signed in, with no mailbox selected", () => {
  const refusals: [string, string][] = [
    ["FETCH 1 FLAGS", "BAD No mailbox selected"],
    ["UID FETCH 1 FLAGS", "BAD No mailbox selected"],
    ["SEARCH ALL", "BAD No mailbox selected"],
    ["UID SEARCH ALL", "BAD No mailbox selected"],
    ["STORE 1 +FLAGS (\\Seen)", "BAD No mailbox selected"],
    ["UID STORE 1 +FLAGS (\\Seen)", "BAD No mailbox selected"],
    ["EXPUNGE", "BAD No mailbox selected"],
    ["CLOSE", "BAD No mailbox selected"],
    ["UNSELECT", "BAD No mailbox selected"],
    // COPY, MOVE and IDLE word it differently, and as NO.
    ["COPY 1 INBOX", "NO Not authenticated or no mailbox selected."],
    ["UID COPY 1 INBOX", "NO Not authenticated or no mailbox selected."],
    ["MOVE 1 INBOX", "NO Not authenticated or no mailbox selected."],
    ["UID MOVE 1 INBOX", "NO Not authenticated or no mailbox selected."],
    ["IDLE", "NO Not authenticated or no mailbox selected"],
    ["UID EXPUNGE 1", "BAD UID EXPUNGE not supported"],
  ];

  test("every command that needs a mailbox is refused with its own text", async () => {
    const imap = await session(await userWithMail(1));
    for (const [command, expected] of refusals) {
      const result = await imap.command(command);
      expect({ command, lines: result.lines }).toEqual({ command, lines: [`${result.tag} ${expected}`] });
    }
    imap.close();
  });

  test("the commands that need none still work", async () => {
    const imap = await session(await userWithMail(1));
    for (const command of ["NOOP", "CHECK", "CAPABILITY", "NAMESPACE", "ID NIL", "ENABLE CONDSTORE", 'LIST "" "*"', 'LSUB "" "*"', "STATUS INBOX (MESSAGES)", "CREATE box", "SUBSCRIBE box", "UNSUBSCRIBE box", "RENAME box box2", "DELETE box2"]) {
      const result = await imap.command(command);
      expect({ command, status: result.status }).toEqual({ command, status: "OK" });
    }
    imap.close();
  });
});

describe("SELECT", () => {
  test("announces EXISTS, RECENT, UNSEEN, UIDVALIDITY, UIDNEXT, HIGHESTMODSEQ, FLAGS and PERMANENTFLAGS, then READ-WRITE", async () => {
    const user = await userWithMail(3);
    const imap = await session(user);
    const result = await imap.command("SELECT INBOX");
    const validity = await storedUidValidity(server, user.username);
    expect(result.lines).toEqual([
      ...block({ exists: 3, unseen: 1, validity, uidNext: 4, highestModseq: 3 }),
      "A2 OK [READ-WRITE] SELECT completed",
    ]);
    imap.close();
  });

  test("an empty mailbox has no UNSEEN line, UIDNEXT 1 and HIGHESTMODSEQ 1", async () => {
    const user = await newUser();
    const imap = await session(user);
    const result = await imap.command("SELECT INBOX");
    const validity = await storedUidValidity(server, user.username);
    expect(result.lines).toEqual([
      ...block({ exists: 0, validity, uidNext: 1, highestModseq: 1 }),
      "A2 OK [READ-WRITE] SELECT completed",
    ]);
    imap.close();
  });

  test("UNSEEN is the sequence number of the first unread message, and is left out when all are read", async () => {
    const user = await userWithMail(3);
    const imap = await session(user, "INBOX");
    await imap.command("STORE 1:2 +FLAGS.SILENT (\\Seen)");
    const partly = await imap.command("SELECT INBOX");
    expect(partly.untagged[2]).toBe("* OK [UNSEEN 3] Message 3 is first unseen");
    await imap.command("STORE 3 +FLAGS.SILENT (\\Seen)");
    const allRead = await imap.command("SELECT INBOX");
    expect(allRead.untagged.filter((line) => line.includes("UNSEEN"))).toEqual([]);
    expect(allRead.untagged).toHaveLength(7);
    imap.close();
  });

  test("UIDNEXT does not go down when the newest message is expunged", async () => {
    const user = await userWithMail(2);
    const imap = await session(user, "INBOX");
    await imap.command("STORE 2 +FLAGS.SILENT (\\Deleted)");
    await imap.command("EXPUNGE");
    const result = await imap.command("SELECT INBOX");
    expect(result.untagged[0]).toBe("* 1 EXISTS");
    expect(result.untagged).toContain("* OK [UIDNEXT 3] Predicted next UID");
    // The next delivery takes the promised UID.
    await deliver(server, { to: `hello@${user.username}.mail.test` });
    await imap.command("SELECT INBOX");
    expect((await imap.command("FETCH 2 UID")).untagged).toEqual(["* 2 FETCH (UID 3)"]);
    imap.close();
  });

  test("accepts INBOX in any letter case, quoted, as a literal, and ignores trailing arguments", async () => {
    const user = await userWithMail(1);
    const imap = await session(user);
    const validity = await storedUidValidity(server, user.username).catch(() => 0);
    const expected = (v: number) => block({ exists: 1, unseen: 1, validity: v, uidNext: 2, highestModseq: 1 });

    const first = await imap.command("select InBoX");
    const stored = validity || (await storedUidValidity(server, user.username));
    expect(first.lines).toEqual([...expected(stored), "A2 OK [READ-WRITE] SELECT completed"]);
    expect(untaggedOf(await imap.command('SELECT "inbox"'))).toEqual(expected(stored));
    const literal = await imap.command(["SELECT ", lit("INBOX")]);
    expect(literal.lines).toEqual(["+ go ahead", ...expected(stored), "A4 OK [READ-WRITE] SELECT completed"]);
    expect(untaggedOf(await imap.command(["SELECT ", lit("INBOX", false)]))).toEqual(expected(stored));
    expect((await imap.command("SELECT INBOX extra words")).tagged).toBe("A6 OK [READ-WRITE] SELECT completed");
    // A CONDSTORE select parameter is accepted and changes nothing.
    expect((await imap.command("SELECT INBOX (CONDSTORE)")).lines).toEqual([
      ...expected(stored),
      "A7 OK [READ-WRITE] SELECT completed",
    ]);
    imap.close();
  });

  test("selects each kind of mailbox, each with its own count and UIDNEXT", async () => {
    const user = await newUser();
    await deliver(server, { to: `hello@${user.username}.mail.test` });
    await deliver(server, { to: `hello@${user.username}.mail.test` });
    await deliver(server, { to: `work@${user.username}.mail.test` });
    await sendOverHttp(server, user, { sender: "me", to: "x@remote.example", subject: "s", html: "<p>s</p>" });
    const imap = await session(user);
    await imap.command("CREATE custom");

    const summary = async (name: string) => {
      const result = await imap.command(`SELECT ${name}`);
      return [
        result.untagged[0],
        result.untagged.find((line) => line.includes("UNSEEN")) ?? "(no UNSEEN)",
        result.untagged.find((line) => line.includes("UIDNEXT")),
        result.tagged!.replace(/^A\d+ /, ""),
      ];
    };
    const readWrite = "OK [READ-WRITE] SELECT completed";
    expect(await summary("INBOX")).toEqual([
      "* 3 EXISTS",
      "* OK [UNSEEN 1] Message 1 is first unseen",
      "* OK [UIDNEXT 4] Predicted next UID",
      readWrite,
    ]);
    expect(await summary('"INBOX/accounts/hello"')).toEqual([
      "* 2 EXISTS",
      "* OK [UNSEEN 1] Message 1 is first unseen",
      "* OK [UIDNEXT 3] Predicted next UID",
      readWrite,
    ]);
    expect(await summary('"INBOX/accounts/work"')).toEqual([
      "* 1 EXISTS",
      "* OK [UNSEEN 1] Message 1 is first unseen",
      "* OK [UIDNEXT 2] Predicted next UID",
      readWrite,
    ]);
    expect(await summary('"Sent Messages"')).toEqual([
      "* 1 EXISTS",
      "(no UNSEEN)",
      "* OK [UIDNEXT 2] Predicted next UID",
      readWrite,
    ]);
    expect(await summary('"Sent Messages/accounts/me"')).toEqual([
      "* 1 EXISTS",
      "(no UNSEEN)",
      "* OK [UIDNEXT 2] Predicted next UID",
      readWrite,
    ]);
    expect(await summary("custom")).toEqual(["* 0 EXISTS", "(no UNSEEN)", "* OK [UIDNEXT 1] Predicted next UID", readWrite]);
    // Drafts and Junk share INBOX's UID counter; Starred and Trash have their own.
    expect(await summary("Drafts")).toEqual(["* 0 EXISTS", "(no UNSEEN)", "* OK [UIDNEXT 4] Predicted next UID", readWrite]);
    expect(await summary("junk")).toEqual(["* 0 EXISTS", "(no UNSEEN)", "* OK [UIDNEXT 4] Predicted next UID", readWrite]);
    expect(await summary("Starred")).toEqual(["* 0 EXISTS", "(no UNSEEN)", "* OK [UIDNEXT 1] Predicted next UID", readWrite]);
    expect(await summary("TRASH")).toEqual(["* 0 EXISTS", "(no UNSEEN)", "* OK [UIDNEXT 1] Predicted next UID", readWrite]);
    imap.close();
  });

  test("the same UIDVALIDITY is announced for every mailbox of the account", async () => {
    const user = await userWithMail(1);
    const imap = await session(user);
    await imap.command("CREATE custom");
    const validities = new Set<string>();
    for (const name of ["INBOX", '"INBOX/accounts/hello"', "custom", "Trash", "Drafts"]) {
      const result = await imap.command(`SELECT ${name}`);
      validities.add(result.untagged.find((line) => line.includes("UIDVALIDITY"))!);
    }
    const validity = await storedUidValidity(server, user.username);
    expect([...validities]).toEqual([`* OK [UIDVALIDITY ${validity}] UIDs valid`]);
    imap.close();
  });

  test("a mailbox name other than INBOX and the special-use ones is case-sensitive", async () => {
    const user = await userWithMail(1);
    const imap = await session(user);
    await imap.command("CREATE Custom");
    expect((await imap.command("SELECT custom")).lines).toEqual(["A3 NO Mailbox does not exist"]);
    expect((await imap.command('SELECT "inbox/accounts/hello"')).lines).toEqual(["A4 NO Mailbox does not exist"]);
    expect((await imap.command('SELECT "INBOX/accounts/Hello"')).lines).toEqual(["A5 NO Mailbox does not exist"]);
    expect((await imap.command("SELECT Custom")).status).toBe("OK");
    imap.close();
  });

  test("selecting again refreshes the view; another SELECT switches mailbox without CLOSE", async () => {
    const user = await userWithMail(1);
    const imap = await session(user, "INBOX");
    await deliver(server, { to: `work@${user.username}.mail.test` });
    expect((await imap.command("FETCH 1:* UID")).untagged).toEqual(["* 1 FETCH (UID 1)"]);
    expect((await imap.command("SELECT INBOX")).untagged[0]).toBe("* 2 EXISTS");
    expect((await imap.command("FETCH 1:* UID")).untagged).toEqual(["* 1 FETCH (UID 1)", "* 2 FETCH (UID 2)"]);
    expect((await imap.command('SELECT "INBOX/accounts/work"')).untagged[0]).toBe("* 1 EXISTS");
    expect((await imap.command("FETCH 1:* UID")).untagged).toEqual(["* 1 FETCH (UID 1)"]);
    imap.close();
  });
});

describe("SELECT failures", () => {
  test("an unknown mailbox is refused and leaves no mailbox selected", async () => {
    const user = await userWithMail(1);
    const imap = await session(user, "INBOX");
    expect((await imap.command("SELECT no-such-box")).lines).toEqual(["A3 NO Mailbox does not exist"]);
    expect((await imap.command("FETCH 1 UID")).lines).toEqual(["A4 BAD No mailbox selected"]);
    expect((await imap.command("CLOSE")).lines).toEqual(["A5 BAD No mailbox selected"]);
    imap.close();
  });

  test("the \\Noselect parents are refused with CANNOT, and the mailbox selected before stays selected", async () => {
    // NOTE: RFC 3501 6.3.1 leaves no mailbox selected after a failed SELECT.
    // The unknown-mailbox refusal above does that; these two return before
    // touching the session, as does the empty name.
    const user = await userWithMail(1);
    await sendOverHttp(server, user, { sender: "me", to: "x@remote.example", subject: "s", html: "<p>s</p>" });
    const imap = await session(user, "INBOX");
    expect((await imap.command('SELECT "INBOX/accounts"')).lines).toEqual([
      "A3 NO [CANNOT] INBOX/accounts is not selectable",
    ]);
    expect((await imap.command('EXAMINE "Sent Messages/accounts"')).lines).toEqual([
      "A4 NO [CANNOT] Sent Messages/accounts is not selectable",
    ]);
    expect((await imap.command('SELECT ""')).lines).toEqual(["A5 NO Empty mailbox name"]);
    expect((await imap.command("FETCH 1 UID")).lines).toEqual(["* 1 FETCH (UID 1)", "A6 OK FETCH completed"]);
    imap.close();
  });

  test("the \\Noselect parents are refused even when the account has no mail for them", async () => {
    const imap = await session(await newUser());
    expect((await imap.command('SELECT "INBOX/accounts"')).lines).toEqual([
      "A2 NO [CANNOT] INBOX/accounts is not selectable",
    ]);
    expect((await imap.command('SELECT "Sent Messages/accounts"')).lines).toEqual([
      "A3 NO [CANNOT] Sent Messages/accounts is not selectable",
    ]);
    expect((await imap.command('SELECT "Sent Messages"')).lines).toEqual(["A4 NO Mailbox does not exist"]);
    imap.close();
  });
});

describe("EXAMINE", () => {
  test("answers like SELECT, PERMANENTFLAGS included, but completes READ-ONLY", async () => {
    const user = await userWithMail(2);
    const imap = await session(user);
    const result = await imap.command("EXAMINE INBOX");
    const validity = await storedUidValidity(server, user.username);
    expect(result.lines).toEqual([
      ...block({ exists: 2, unseen: 1, validity, uidNext: 3, highestModseq: 2 }),
      "A2 OK [READ-ONLY] EXAMINE completed",
    ]);
    imap.close();
  });

  test("refuses STORE, EXPUNGE and MOVE on the examined mailbox", async () => {
    const user = await userWithMail(1);
    const imap = await session(user);
    await imap.command("CREATE target");
    await imap.command("EXAMINE INBOX");
    const refusal = "NO [READ-ONLY] Mailbox is read-only";
    expect((await imap.command("STORE 1 +FLAGS (\\Seen)")).lines).toEqual([`A4 ${refusal}`]);
    expect((await imap.command("UID STORE 1 FLAGS.SILENT (\\Deleted)")).lines).toEqual([`A5 ${refusal}`]);
    expect((await imap.command("EXPUNGE")).lines).toEqual([`A6 ${refusal}`]);
    expect((await imap.command("MOVE 1 target")).lines).toEqual([`A7 ${refusal}`]);
    expect((await imap.command("UID MOVE 1 target")).lines).toEqual([`A8 ${refusal}`]);
    expect((await imap.command("FETCH 1 FLAGS")).untagged).toEqual(["* 1 FETCH (FLAGS ())"]);
    imap.close();
  });

  test("still allows COPY out of it and APPEND into it", async () => {
    const user = await userWithMail(1);
    const imap = await session(user);
    await imap.command("CREATE target");
    await imap.command("EXAMINE INBOX");
    const validity = await storedUidValidity(server, user.username);
    expect((await imap.command("COPY 1 target")).lines).toEqual([`A4 OK [COPYUID ${validity} 1 1] COPY completed`]);
    const append = await imap.command(["APPEND INBOX ", lit("Subject: appended\r\n\r\nx\r\n")]);
    expect(append.status).toBe("OK");
    imap.close();
  });

  test("a non-PEEK body fetch still sets \\Seen on the examined mailbox", async () => {
    // NOTE: looks like a bug. RFC 3501 6.3.2: EXAMINE must not change the
    // mailbox's state, including \Seen. Only the read-only *account* is
    // spared the implicit flag; an EXAMINE by a normal account is not.
    const user = await userWithMail(1);
    const imap = await session(user);
    await imap.command("EXAMINE INBOX");
    expect((await imap.command("FETCH 1 BODY[TEXT]")).status).toBe("OK");
    expect((await imap.command("FETCH 1 FLAGS")).untagged).toEqual(["* 1 FETCH (FLAGS (\\Seen))"]);
    const [row] = await server.db.query("SELECT read FROM mails WHERE user_id = $1", [user.id]);
    expect(row).toEqual({ read: true });
    imap.close();
  });

  test("a SELECT after EXAMINE makes the mailbox writable again, and the other way round", async () => {
    const user = await userWithMail(1);
    const imap = await session(user);
    await imap.command("EXAMINE INBOX");
    await imap.command("SELECT INBOX");
    expect((await imap.command("STORE 1 +FLAGS (\\Flagged)")).lines).toEqual([
      "* 1 FETCH (FLAGS (\\Flagged))",
      "A4 OK STORE completed",
    ]);
    await imap.command("EXAMINE INBOX");
    expect((await imap.command("STORE 1 -FLAGS (\\Flagged)")).lines).toEqual([
      "A6 NO [READ-ONLY] Mailbox is read-only",
    ]);
    imap.close();
  });

  test("a failed EXAMINE of an unknown mailbox leaves no mailbox selected", async () => {
    const user = await userWithMail(1);
    const imap = await session(user, "INBOX");
    expect((await imap.command("EXAMINE no-such-box")).lines).toEqual(["A3 NO Mailbox does not exist"]);
    expect((await imap.command("SEARCH ALL")).lines).toEqual(["A4 BAD No mailbox selected"]);
    imap.close();
  });
});

describe("CLOSE, UNSELECT and CHECK", () => {
  test("CLOSE returns to the authenticated state", async () => {
    const user = await userWithMail(1);
    const imap = await session(user, "INBOX");
    expect((await imap.command("CLOSE")).lines).toEqual(["A3 OK CLOSE completed"]);
    expect((await imap.command("FETCH 1 UID")).lines).toEqual(["A4 BAD No mailbox selected"]);
    expect((await imap.command("CLOSE")).lines).toEqual(["A5 BAD No mailbox selected"]);
    expect((await imap.command('LIST "" INBOX')).status).toBe("OK");
    imap.close();
  });

  test("CLOSE does not expunge messages flagged \\Deleted", async () => {
    // NOTE: RFC 3501 6.4.2 has CLOSE permanently remove \Deleted messages
    // from a mailbox selected read-write. Here CLOSE only deselects, exactly
    // like UNSELECT.
    const user = await userWithMail(2);
    const imap = await session(user, "INBOX");
    await imap.command("STORE 1 +FLAGS.SILENT (\\Deleted)");
    expect((await imap.command("CLOSE")).lines).toEqual(["A4 OK CLOSE completed"]);
    const again = await imap.command("SELECT INBOX");
    expect(again.untagged[0]).toBe("* 2 EXISTS");
    expect((await imap.command("FETCH 1 FLAGS")).untagged).toEqual(["* 1 FETCH (FLAGS (\\Deleted))"]);
    const rows = await server.db.query(
      "SELECT deleted, expunged FROM mails WHERE user_id = $1 ORDER BY uid_domain",
      [user.id]
    );
    expect(rows).toEqual([
      { deleted: true, expunged: false },
      { deleted: false, expunged: false },
    ]);
    imap.close();
  });

  test("UNSELECT returns to the authenticated state without expunging", async () => {
    const user = await userWithMail(1);
    const imap = await session(user, "INBOX");
    await imap.command("STORE 1 +FLAGS.SILENT (\\Deleted)");
    expect((await imap.command("UNSELECT")).lines).toEqual(["A4 OK UNSELECT completed"]);
    expect((await imap.command("UNSELECT")).lines).toEqual(["A5 BAD No mailbox selected"]);
    expect((await imap.command("STATUS INBOX (MESSAGES)")).untagged).toEqual(['* STATUS "INBOX" (MESSAGES 1)']);
    imap.close();
  });

  test("CLOSE and UNSELECT work on an examined mailbox", async () => {
    const user = await userWithMail(1);
    const imap = await session(user);
    await imap.command("EXAMINE INBOX");
    expect((await imap.command("CLOSE")).lines).toEqual(["A3 OK CLOSE completed"]);
    await imap.command("EXAMINE INBOX");
    expect((await imap.command("UNSELECT")).lines).toEqual(["A5 OK UNSELECT completed"]);
    imap.close();
  });

  test("CHECK completes with no untagged data and does not refresh the view", async () => {
    const user = await userWithMail(1);
    const imap = await session(user, "INBOX");
    await deliver(server, { to: `hello@${user.username}.mail.test` });
    expect((await imap.command("CHECK")).lines).toEqual(["A3 OK CHECK completed"]);
    expect((await imap.command("FETCH 1:* UID")).untagged).toEqual(["* 1 FETCH (UID 1)"]);
    imap.close();
  });
});
