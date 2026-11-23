import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { ADMIN_PASSWORD, createUser, type TestServer, type TestUser } from "../harness";
import { ImapClient, lit, openSession, utf8 } from "../harness/imap";
import { deliver, sendOverHttp, startImapServer, storedUidValidity } from "./support";

let server: TestServer;
let userCount = 0;

/** A fresh account, so a test's mailboxes are its own. */
const newUser = (): Promise<TestUser> => createUser(server, `box${++userCount}`);

const session = async (user: TestUser, mailbox?: string): Promise<ImapClient> =>
  openSession(server, user.username, user.password, mailbox);

beforeAll(async () => {
  server = await startImapServer();
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

const UTILITY = [
  '* LIST (\\Drafts \\HasNoChildren) "/" "Drafts"',
  '* LIST (\\Junk \\HasNoChildren) "/" "Junk"',
  '* LIST (\\Flagged \\HasNoChildren) "/" "Starred"',
  '* LIST (\\Trash \\HasNoChildren) "/" "Trash"',
];

describe("LIST: the mailbox tree", () => {
  test("a new account has INBOX and the four special-use mailboxes, in this order", async () => {
    const imap = await session(await newUser());
    const result = await imap.command('LIST "" "*"');
    expect(result.lines).toEqual(['* LIST (\\HasNoChildren) "/" "INBOX"', ...UTILITY, "A2 OK LIST completed"]);
    imap.close();
  });

  test("received mail adds INBOX/accounts and one mailbox per recipient local part, newest mail first", async () => {
    const user = await newUser();
    for (const local of ["zeta", "alpha", "zeta", "Mid"]) {
      await deliver(server, { to: `${local}@${user.username}.mail.test` });
    }
    const imap = await session(user);
    const result = await imap.command('LIST "" "*"');
    expect(result.lines).toEqual([
      '* LIST (\\HasChildren) "/" "INBOX"',
      ...UTILITY,
      '* LIST (\\HasChildren \\Noselect) "/" "INBOX/accounts"',
      // Ordered by the Date of each address's latest mail, newest first; the
      // local part is listed in lower case.
      '* LIST (\\HasNoChildren) "/" "INBOX/accounts/mid"',
      '* LIST (\\HasNoChildren) "/" "INBOX/accounts/zeta"',
      '* LIST (\\HasNoChildren) "/" "INBOX/accounts/alpha"',
      "A2 OK LIST completed",
    ]);
    imap.close();
  });

  test("sent mail adds Sent Messages, its accounts parent and one mailbox per sender", async () => {
    const user = await newUser();
    await sendOverHttp(server, user, {
      sender: "press",
      to: "friend@remote.example",
      subject: "Sent",
      html: "<p>Hi</p>",
    });
    const imap = await session(user);
    const result = await imap.command('LIST "" "*"');
    expect(result.lines).toEqual([
      '* LIST (\\HasNoChildren) "/" "INBOX"',
      ...UTILITY,
      '* LIST (\\HasChildren) "/" "Sent Messages"',
      '* LIST (\\HasChildren \\Noselect) "/" "Sent Messages/accounts"',
      '* LIST (\\HasNoChildren) "/" "Sent Messages/accounts/press"',
      "A2 OK LIST completed",
    ]);
    imap.close();
  });

  test("the admin gets account mailboxes for the bare domain only, though admin.<domain> mail is in INBOX", async () => {
    await deliver(server, { to: "hello@mail.test" });
    await deliver(server, { to: "info@admin.mail.test" });
    const imap = await openSession(server, "admin", ADMIN_PASSWORD);
    const result = await imap.command('LIST "" "INBOX*"');
    expect(result.lines).toEqual([
      '* LIST (\\HasChildren) "/" "INBOX"',
      '* LIST (\\HasChildren \\Noselect) "/" "INBOX/accounts"',
      '* LIST (\\HasNoChildren) "/" "INBOX/accounts/hello"',
      "A2 OK LIST completed",
    ]);
    const status = await imap.command("STATUS INBOX (MESSAGES)");
    expect(status.untagged).toEqual(['* STATUS "INBOX" (MESSAGES 2)']);
    imap.close();
  });

  test("user-created mailboxes follow the server-defined ones, in name order", async () => {
    const imap = await session(await newUser());
    for (const name of ["gamma", "alpha", "beta"]) await imap.command(`CREATE ${name}`);
    const result = await imap.command('LIST "" "*"');
    expect(result.untagged).toEqual([
      '* LIST (\\HasNoChildren) "/" "INBOX"',
      ...UTILITY,
      '* LIST (\\HasNoChildren) "/" "alpha"',
      '* LIST (\\HasNoChildren) "/" "beta"',
      '* LIST (\\HasNoChildren) "/" "gamma"',
    ]);
    imap.close();
  });

  test("a created child marks an existing parent \\HasChildren; a missing parent is not listed", async () => {
    const imap = await session(await newUser());
    await imap.command("CREATE top");
    await imap.command("CREATE top/child");
    await imap.command("CREATE lone/deep/leaf");
    expect((await imap.command('LIST "" "top*"')).untagged).toEqual([
      '* LIST (\\HasChildren) "/" "top"',
      '* LIST (\\HasNoChildren) "/" "top/child"',
    ]);
    // CREATE does not create the superior names.
    expect((await imap.command('LIST "" "lone*"')).untagged).toEqual([
      '* LIST (\\HasNoChildren) "/" "lone/deep/leaf"',
    ]);
    expect((await imap.command('LIST "" "lone/%"')).untagged).toEqual([]);
    imap.close();
  });
});

describe("LIST: reference and pattern", () => {
  let imap: ImapClient;

  beforeAll(async () => {
    const user = await newUser();
    await deliver(server, { to: `hello@${user.username}.mail.test` });
    await deliver(server, { to: `work@${user.username}.mail.test` });
    imap = await session(user);
    await imap.command("CREATE archive");
    await imap.command("CREATE archive/2025");
  });

  afterAll(() => imap?.close());

  const list = async (args: string) => (await imap.command(`LIST ${args}`)).untagged;

  test("% stops at the hierarchy delimiter", async () => {
    expect(await list('"" "%"')).toEqual([
      '* LIST (\\HasChildren) "/" "INBOX"',
      ...UTILITY,
      '* LIST (\\HasChildren) "/" "archive"',
    ]);
    expect(await list('"" "INBOX/%"')).toEqual(['* LIST (\\HasChildren \\Noselect) "/" "INBOX/accounts"']);
    expect(await list('"" "%/accounts/%"')).toEqual([
      '* LIST (\\HasNoChildren) "/" "INBOX/accounts/work"',
      '* LIST (\\HasNoChildren) "/" "INBOX/accounts/hello"',
    ]);
    expect(await list('"" "%/%"')).toEqual([
      '* LIST (\\HasChildren \\Noselect) "/" "INBOX/accounts"',
      '* LIST (\\HasNoChildren) "/" "archive/2025"',
    ]);
  });

  test("* crosses the delimiter and may sit anywhere in the pattern", async () => {
    expect(await list('"" "INBOX*"')).toEqual([
      '* LIST (\\HasChildren) "/" "INBOX"',
      '* LIST (\\HasChildren \\Noselect) "/" "INBOX/accounts"',
      '* LIST (\\HasNoChildren) "/" "INBOX/accounts/work"',
      '* LIST (\\HasNoChildren) "/" "INBOX/accounts/hello"',
    ]);
    expect(await list('"" "*/accounts"')).toEqual(['* LIST (\\HasChildren \\Noselect) "/" "INBOX/accounts"']);
    // INBOX matches a lower-case pattern letter: its comparison ignores case.
    expect(await list('"" "*o*"')).toEqual([
      '* LIST (\\HasChildren) "/" "INBOX"',
      '* LIST (\\HasChildren \\Noselect) "/" "INBOX/accounts"',
      '* LIST (\\HasNoChildren) "/" "INBOX/accounts/work"',
      '* LIST (\\HasNoChildren) "/" "INBOX/accounts/hello"',
    ]);
    expect(await list('"" "*%*"')).toHaveLength(10);
  });

  test("the reference is prefixed to the pattern as plain text", async () => {
    expect(await list('"INBOX/" "%"')).toEqual(['* LIST (\\HasChildren \\Noselect) "/" "INBOX/accounts"']);
    expect(await list('"INBOX" "/*"')).toEqual([
      '* LIST (\\HasChildren \\Noselect) "/" "INBOX/accounts"',
      '* LIST (\\HasNoChildren) "/" "INBOX/accounts/work"',
      '* LIST (\\HasNoChildren) "/" "INBOX/accounts/hello"',
    ]);
    expect(await list('"arch" "ive"')).toEqual(['* LIST (\\HasChildren) "/" "archive"']);
    expect(await list('"~" "*"')).toEqual([]);
  });

  test("an exact name lists that mailbox; an unknown one lists nothing and still completes OK", async () => {
    expect(await list('"" "INBOX/accounts/hello"')).toEqual(['* LIST (\\HasNoChildren) "/" "INBOX/accounts/hello"']);
    expect(await list('"" Drafts')).toEqual(['* LIST (\\Drafts \\HasNoChildren) "/" "Drafts"']);
    const none = await imap.command('LIST "" "no-such-box"');
    expect(none.lines).toEqual([`${none.tag} OK LIST completed`]);
  });

  test("reference and pattern may be atoms or quoted strings", async () => {
    expect(await list('"" archive')).toEqual(['* LIST (\\HasChildren) "/" "archive"']);
    expect(await list("arch ive")).toEqual(['* LIST (\\HasChildren) "/" "archive"']);
    expect(await list('"" archive/%')).toEqual(['* LIST (\\HasNoChildren) "/" "archive/2025"']);
    expect(await list('"" *2025')).toEqual(['* LIST (\\HasNoChildren) "/" "archive/2025"']);
  });

  test("only INBOX itself is matched without regard to case", async () => {
    expect(await list('"" inbox')).toEqual(['* LIST (\\HasChildren) "/" "INBOX"']);
    expect(await list('"" "InBoX"')).toEqual(['* LIST (\\HasChildren) "/" "INBOX"']);
    // Its children, and every other name, are case-sensitive in LIST.
    expect(await list('"" "inbox/*"')).toEqual([]);
    expect(await list('"" drafts')).toEqual([]);
    expect(await list('"" ARCHIVE')).toEqual([]);
  });

  test("an empty pattern returns the delimiter and echoes the reference", async () => {
    expect((await imap.command('LIST "" ""')).untagged).toEqual(['* LIST (\\Noselect) "/" ""']);
    expect((await imap.command('LIST "INBOX/" ""')).untagged).toEqual(['* LIST (\\Noselect) "/" "INBOX/"']);
    expect((await imap.command('LIST "no such \\"ref\\"" ""')).untagged).toEqual([
      '* LIST (\\Noselect) "/" "no such \\"ref\\""',
    ]);
  });

  test("the reference may be a literal, the pattern may not", async () => {
    const reference = await imap.command(["LIST ", lit("arch"), " ive"]);
    expect(reference.lines).toEqual([
      "+ go ahead",
      '* LIST (\\HasChildren) "/" "archive"',
      `${reference.tag} OK LIST completed`,
    ]);
    const pattern = await imap.command(['LIST "" ', lit("archive")]);
    expect(pattern.lines).toEqual(["+ go ahead", `${pattern.tag} BAD Invalid mailbox name in LIST`]);
  });

  test("LIST-EXTENDED selection options are a syntax error; trailing return options are ignored", async () => {
    const selection = await imap.command('LIST (SPECIAL-USE) "" "*"');
    expect(selection.lines).toEqual([`${selection.tag} BAD Invalid reference name in LIST`]);
    const returns = await imap.command('LIST "" "archive" RETURN (SPECIAL-USE)');
    expect(returns.untagged).toEqual(['* LIST (\\HasChildren) "/" "archive"']);
  });
});

describe("CREATE", () => {
  test("creates a mailbox that can be listed, selected and queried", async () => {
    const imap = await session(await newUser());
    expect((await imap.command("CREATE Archive")).lines).toEqual(["A2 OK CREATE completed"]);
    expect((await imap.command('LIST "" Archive')).untagged).toEqual(['* LIST (\\HasNoChildren) "/" "Archive"']);
    expect((await imap.command("STATUS Archive (MESSAGES UIDNEXT UNSEEN RECENT HIGHESTMODSEQ)")).untagged).toEqual([
      '* STATUS "Archive" (MESSAGES 0 UIDNEXT 1 UNSEEN 0 RECENT 0 HIGHESTMODSEQ 1)',
    ]);
    expect((await imap.command("SELECT Archive")).tagged).toBe("A5 OK [READ-WRITE] SELECT completed");
    imap.close();
  });

  test("stores one row per mailbox, subscribed", async () => {
    const user = await newUser();
    const imap = await session(user);
    await imap.command("CREATE Stored");
    const rows = await server.db.query(
      "SELECT name, address, special_use, subscribed, parent_id FROM mailboxes WHERE user_id = $1",
      [user.id]
    );
    expect(rows).toEqual([
      { name: "Stored", address: null, special_use: null, subscribed: true, parent_id: null },
    ]);
    imap.close();
  });

  test("refuses a name that exists, compared without regard to case", async () => {
    const imap = await session(await newUser());
    await imap.command("CREATE Archive");
    expect((await imap.command("CREATE Archive")).lines).toEqual([
      "A3 NO [ALREADYEXISTS] Mailbox already exists",
    ]);
    expect((await imap.command("CREATE archive")).lines).toEqual([
      "A4 NO [ALREADYEXISTS] Mailbox already exists",
    ]);
    imap.close();
  });

  test("refuses INBOX and the special-use names in any letter case", async () => {
    const imap = await session(await newUser());
    for (const name of ["INBOX", "inbox", "Drafts", "drafts", "JUNK", "Starred", "TRASH"]) {
      const result = await imap.command(`CREATE ${name}`);
      expect({ name, lines: result.lines }).toEqual({
        name,
        lines: [`${result.tag} NO [ALREADYEXISTS] Mailbox already exists`],
      });
    }
    imap.close();
  });

  test("refuses an empty name", async () => {
    const imap = await session(await newUser());
    expect((await imap.command('CREATE ""')).lines).toEqual(["A2 NO Empty mailbox name"]);
    imap.close();
  });

  test("accepts a name of 255 UTF-8 octets and refuses 256", async () => {
    const imap = await session(await newUser());
    expect((await imap.command(`CREATE ${"n".repeat(255)}`)).lines).toEqual(["A2 OK CREATE completed"]);
    expect((await imap.command(`CREATE ${"m".repeat(256)}`)).lines).toEqual([
      "A3 NO [LIMIT] Mailbox name exceeds 255 bytes",
    ]);
    // The limit counts octets, not characters: 128 two-octet letters are 256.
    expect((await imap.command(`CREATE "${utf8("é".repeat(128))}"`)).lines).toEqual([
      "A4 NO [LIMIT] Mailbox name exceeds 255 bytes",
    ]);
    expect((await imap.command(`CREATE "${utf8(`${"é".repeat(127)}a`)}"`)).lines).toEqual([
      "A5 OK CREATE completed",
    ]);
    imap.close();
  });

  test("keeps names with spaces, quotes, backslashes, 8-bit and modified-UTF-7 text exactly as sent", async () => {
    const user = await newUser();
    const imap = await session(user);
    const names = [
      "Lit Box",
      " spaced ",
      'with "quote" and \\ backslash',
      "Entwürfe ✉",
      "&AOk-t&AOk-",
      "trailing/",
    ];
    await imap.command(["CREATE ", lit("Lit Box")]);
    await imap.command('CREATE " spaced "');
    await imap.command('CREATE "with \\"quote\\" and \\\\ backslash"');
    await imap.command(`CREATE "${utf8("Entwürfe ✉")}"`);
    await imap.command('CREATE "&AOk-t&AOk-"');
    await imap.command('CREATE "trailing/"');

    const stored = await server.db.query<{ name: string }>("SELECT name FROM mailboxes WHERE user_id = $1", [
      user.id,
    ]);
    expect(stored.map((row) => row.name).sort()).toEqual([...names].sort());

    // LIST answers each in a quoted string: " and \ escaped, UTF-8 sent as is.
    expect((await imap.command('LIST "" "Lit Box"')).untagged).toEqual(['* LIST (\\HasNoChildren) "/" "Lit Box"']);
    expect((await imap.command('LIST "" " spaced "')).untagged).toEqual(['* LIST (\\HasNoChildren) "/" " spaced "']);
    expect((await imap.command('LIST "" "with*"')).untagged).toEqual([
      '* LIST (\\HasNoChildren) "/" "with \\"quote\\" and \\\\ backslash"',
    ]);
    expect((await imap.command('LIST "" "Entw*"')).untagged).toEqual([
      `* LIST (\\HasNoChildren) "/" "${utf8("Entwürfe ✉")}"`,
    ]);
    expect((await imap.command('LIST "" "&AOk-*"')).untagged).toEqual([
      '* LIST (\\HasNoChildren) "/" "&AOk-t&AOk-"',
    ]);
    expect((await imap.command('LIST "" "trailing*"')).untagged).toEqual([
      '* LIST (\\HasNoChildren) "/" "trailing/"',
    ]);
    // A non-ASCII name selects by its UTF-8 octets.
    expect((await imap.command(`SELECT "${utf8("Entwürfe ✉")}"`)).status).toBe("OK");
    imap.close();
  });

  test("lets a user mailbox be created under INBOX", async () => {
    const imap = await session(await newUser());
    expect((await imap.command("CREATE INBOX/sub")).lines).toEqual(["A2 OK CREATE completed"]);
    expect((await imap.command('LIST "" "INBOX*"')).untagged).toEqual([
      '* LIST (\\HasChildren) "/" "INBOX"',
      '* LIST (\\HasNoChildren) "/" "INBOX/sub"',
    ]);
    imap.close();
  });

  test("accepts the name of a per-address mailbox that already exists, leaving a hidden duplicate row", async () => {
    // NOTE: RFC 3501 6.3.3 makes it an error to create a name that refers to
    // an existing mailbox. `INBOX/accounts` and `INBOX/accounts/<name>` are
    // listed (and selectable) without a row behind them, so CREATE succeeds
    // and stores a row LIST then hides — which RENAME can afterwards move.
    const user = await newUser();
    await deliver(server, { to: `hello@${user.username}.mail.test` });
    const imap = await session(user);
    expect((await imap.command('CREATE "INBOX/accounts"')).lines).toEqual(["A2 OK CREATE completed"]);
    expect((await imap.command('CREATE "INBOX/accounts/hello"')).lines).toEqual(["A3 OK CREATE completed"]);
    expect((await imap.command('LIST "" "INBOX/*"')).untagged).toEqual([
      '* LIST (\\HasChildren \\Noselect) "/" "INBOX/accounts"',
      '* LIST (\\HasNoChildren) "/" "INBOX/accounts/hello"',
    ]);
    expect((await imap.command('RENAME "INBOX/accounts/hello" moved')).lines).toEqual(["A5 OK RENAME completed"]);
    // The per-address mailbox is still there, with its mail; `moved` is empty.
    expect((await imap.command('STATUS "INBOX/accounts/hello" (MESSAGES)')).untagged).toEqual([
      '* STATUS "INBOX/accounts/hello" (MESSAGES 1)',
    ]);
    expect((await imap.command("STATUS moved (MESSAGES)")).untagged).toEqual(['* STATUS "moved" (MESSAGES 0)']);
    imap.close();
  });

  test("Sent Messages can be created by hand before any mail is sent", async () => {
    const imap = await session(await newUser());
    expect((await imap.command('STATUS "Sent Messages" (MESSAGES)')).lines).toEqual([
      "A2 NO Mailbox does not exist",
    ]);
    expect((await imap.command('CREATE "Sent Messages"')).lines).toEqual(["A3 OK CREATE completed"]);
    expect((await imap.command('LIST "" "Sent*"')).untagged).toEqual([
      '* LIST (\\HasNoChildren) "/" "Sent Messages"',
    ]);
    imap.close();
  });

  test("refuses the 1001st mailbox, but still reports an existing name as existing", async () => {
    const user = await newUser();
    await server.db.query(
      "INSERT INTO mailboxes (user_id, name) SELECT $1, 'seed-' || n FROM generate_series(1, 999) AS n",
      [user.id]
    );
    const imap = await session(user);
    expect((await imap.command("CREATE the-thousandth")).lines).toEqual(["A2 OK CREATE completed"]);
    expect((await imap.command("CREATE one-too-many")).lines).toEqual([
      "A3 NO [LIMIT] Mailbox limit of 1000 reached",
    ]);
    expect((await imap.command("CREATE seed-7")).lines).toEqual([
      "A4 NO [ALREADYEXISTS] Mailbox already exists",
    ]);
    // Deleting one makes room again.
    expect((await imap.command("DELETE seed-7")).status).toBe("OK");
    expect((await imap.command("CREATE one-too-many")).lines).toEqual(["A6 OK CREATE completed"]);
    imap.close();
  });
});

describe("DELETE", () => {
  test("removes a user-created mailbox", async () => {
    const user = await newUser();
    const imap = await session(user);
    await imap.command("CREATE Archive");
    expect((await imap.command("DELETE Archive")).lines).toEqual(["A3 OK DELETE completed"]);
    expect((await imap.command('LIST "" Archive')).untagged).toEqual([]);
    expect((await imap.command("SELECT Archive")).lines).toEqual(["A5 NO Mailbox does not exist"]);
    expect(await server.db.query("SELECT name FROM mailboxes WHERE user_id = $1", [user.id])).toEqual([]);
    imap.close();
  });

  test("finds the mailbox without regard to letter case, unlike SELECT", async () => {
    const imap = await session(await newUser());
    await imap.command("CREATE Archive");
    expect((await imap.command("SELECT archive")).lines).toEqual(["A3 NO Mailbox does not exist"]);
    expect((await imap.command("DELETE archive")).lines).toEqual(["A4 OK DELETE completed"]);
    expect((await imap.command('LIST "" "Archive"')).untagged).toEqual([]);
    imap.close();
  });

  test("refuses INBOX, the special-use mailboxes, unknown and empty names", async () => {
    const imap = await session(await newUser());
    const cases: [string, string][] = [
      ["INBOX", "NO [CANNOT] Cannot delete INBOX"],
      ["inbox", "NO [CANNOT] Cannot delete INBOX"],
      ["Drafts", "NO [CANNOT] Cannot delete system mailbox"],
      ["junk", "NO [CANNOT] Cannot delete system mailbox"],
      ["Starred", "NO [CANNOT] Cannot delete system mailbox"],
      ["TRASH", "NO [CANNOT] Cannot delete system mailbox"],
      ["never-created", "NO [NONEXISTENT] Mailbox does not exist"],
      ['""', "NO Empty mailbox name"],
    ];
    for (const [name, expected] of cases) {
      const result = await imap.command(`DELETE ${name}`);
      expect({ name, lines: result.lines }).toEqual({ name, lines: [`${result.tag} ${expected}`] });
    }
    imap.close();
  });

  test("answers NONEXISTENT for a per-address mailbox and for the accounts parent", async () => {
    const user = await newUser();
    await deliver(server, { to: `hello@${user.username}.mail.test` });
    const imap = await session(user);
    for (const name of ['"INBOX/accounts/hello"', '"INBOX/accounts"']) {
      const result = await imap.command(`DELETE ${name}`);
      expect(result.lines).toEqual([`${result.tag} NO [NONEXISTENT] Mailbox does not exist`]);
    }
    imap.close();
  });

  test("leaves the mailbox's messages behind: creating the name again brings them back", async () => {
    // NOTE: looks like a bug. DELETE removes the mailbox row only; the
    // messages stay filed under the name, with their UIDs, and a later CREATE
    // of the same name shows them again under the same UIDVALIDITY.
    const user = await newUser();
    await deliver(server, { to: `hello@${user.username}.mail.test` });
    const imap = await session(user, "INBOX");
    await imap.command("CREATE keep");
    await imap.command("COPY 1 keep");
    expect((await imap.command("DELETE keep")).status).toBe("OK");
    expect((await imap.command("CREATE keep")).status).toBe("OK");
    expect((await imap.command("STATUS keep (MESSAGES UIDNEXT)")).untagged).toEqual([
      '* STATUS "keep" (MESSAGES 1 UIDNEXT 2)',
    ]);
    imap.close();
  });

  test("deleting the selected mailbox leaves the session selected on it", async () => {
    // NOTE: the session keeps serving the deleted mailbox until it selects
    // something else; nothing tells the client the mailbox is gone.
    const user = await newUser();
    await deliver(server, { to: `hello@${user.username}.mail.test` });
    const imap = await session(user, "INBOX");
    await imap.command("CREATE doomed");
    await imap.command("COPY 1 doomed");
    await imap.command("SELECT doomed");
    expect((await imap.command("DELETE doomed")).status).toBe("OK");
    expect((await imap.command("FETCH 1 UID")).lines).toEqual(["* 1 FETCH (UID 1)", "A7 OK FETCH completed"]);
    expect((await imap.command("SELECT doomed")).lines).toEqual(["A8 NO Mailbox does not exist"]);
    expect((await imap.command("FETCH 1 UID")).lines).toEqual(["A9 BAD No mailbox selected"]);
    imap.close();
  });
});

describe("RENAME", () => {
  test("renames a user-created mailbox", async () => {
    const user = await newUser();
    const imap = await session(user);
    await imap.command("CREATE Archive");
    expect((await imap.command("RENAME Archive Archive2")).lines).toEqual(["A3 OK RENAME completed"]);
    expect((await imap.command('LIST "" "Archive*"')).untagged).toEqual([
      '* LIST (\\HasNoChildren) "/" "Archive2"',
    ]);
    // The row keeps its identity and its per-row validity counter is bumped.
    expect(await server.db.query("SELECT name, uid_validity FROM mailboxes WHERE user_id = $1", [user.id])).toEqual([
      { name: "Archive2", uid_validity: 2 },
    ]);
    imap.close();
  });

  test("does not rename children along with their parent", async () => {
    const imap = await session(await newUser());
    await imap.command("CREATE top");
    await imap.command("CREATE top/child");
    await imap.command("RENAME top roof");
    expect((await imap.command('LIST "" "roof*"')).untagged).toEqual(['* LIST (\\HasNoChildren) "/" "roof"']);
    expect((await imap.command('LIST "" "top*"')).untagged).toEqual(['* LIST (\\HasNoChildren) "/" "top/child"']);
    imap.close();
  });

  test("finds the source without regard to letter case and stores the new name as given", async () => {
    const imap = await session(await newUser());
    await imap.command("CREATE Archive");
    expect((await imap.command("RENAME ARCHIVE Renamed")).lines).toEqual(["A3 OK RENAME completed"]);
    expect((await imap.command('LIST "" "*enamed"')).untagged).toEqual(['* LIST (\\HasNoChildren) "/" "Renamed"']);
    imap.close();
  });

  test("refuses each invalid source and target with its own response code", async () => {
    const imap = await session(await newUser());
    await imap.command("CREATE Archive");
    await imap.command("CREATE Other");
    const cases: [string, string][] = [
      ["Missing Fresh", "NO [NONEXISTENT] Mailbox does not exist"],
      ["Archive Other", "NO [ALREADYEXISTS] Target mailbox already exists"],
      ["Archive Archive", "NO [ALREADYEXISTS] Target mailbox already exists"],
      ["Archive OTHER", "NO [ALREADYEXISTS] Target mailbox already exists"],
      ["Archive INBOX", "NO [ALREADYEXISTS] Target mailbox already exists"],
      ["Archive inbox", "NO [ALREADYEXISTS] Target mailbox already exists"],
      ["Archive Trash", "NO [ALREADYEXISTS] Target mailbox already exists"],
      ["Archive drafts", "NO [ALREADYEXISTS] Target mailbox already exists"],
      ["INBOX Fresh", "NO [CANNOT] RENAME INBOX is not supported"],
      ["inbox Fresh", "NO [CANNOT] RENAME INBOX is not supported"],
      ["Drafts Fresh", "NO [CANNOT] Cannot rename system mailbox"],
      ["junk Fresh", "NO [CANNOT] Cannot rename system mailbox"],
      [`Archive ${"m".repeat(256)}`, "NO [LIMIT] Mailbox name exceeds 255 bytes"],
      ['"" Fresh', "NO Empty mailbox name"],
      ['Archive ""', "NO Empty mailbox name"],
    ];
    for (const [args, expected] of cases) {
      const result = await imap.command(`RENAME ${args}`);
      expect({ args, lines: result.lines }).toEqual({ args, lines: [`${result.tag} ${expected}`] });
    }
    // Nothing was renamed along the way.
    expect((await imap.command('LIST "" "*r*"')).untagged).toEqual([
      '* LIST (\\Drafts \\HasNoChildren) "/" "Drafts"',
      '* LIST (\\Flagged \\HasNoChildren) "/" "Starred"',
      '* LIST (\\Trash \\HasNoChildren) "/" "Trash"',
      '* LIST (\\HasNoChildren) "/" "Archive"',
      '* LIST (\\HasNoChildren) "/" "Other"',
    ]);
    imap.close();
  });

  test("answers NONEXISTENT for the per-address and sent mailboxes", async () => {
    const user = await newUser();
    await deliver(server, { to: `hello@${user.username}.mail.test` });
    await sendOverHttp(server, user, { sender: "me", to: "x@remote.example", subject: "s", html: "<p>s</p>" });
    const imap = await session(user);
    for (const name of ['"INBOX/accounts/hello"', '"INBOX/accounts"', '"Sent Messages"', '"Sent Messages/accounts/me"']) {
      const result = await imap.command(`RENAME ${name} Fresh`);
      expect({ name, lines: result.lines }).toEqual({
        name,
        lines: [`${result.tag} NO [NONEXISTENT] Mailbox does not exist`],
      });
    }
    imap.close();
  });

  test("does not take the messages along: the new name is empty and the old name still owns them", async () => {
    // NOTE: looks like a bug (data loss as a client sees it). Messages are
    // filed under the mailbox *name*; RENAME changes the name on the mailbox
    // row only, so the renamed mailbox is empty and the messages reappear
    // when a mailbox with the old name is created.
    const user = await newUser();
    await deliver(server, { to: `hello@${user.username}.mail.test` });
    await deliver(server, { to: `hello@${user.username}.mail.test` });
    const imap = await session(user, "INBOX");
    await imap.command("CREATE before");
    await imap.command("COPY 1:2 before");
    expect((await imap.command("STATUS before (MESSAGES UIDNEXT)")).untagged).toEqual([
      '* STATUS "before" (MESSAGES 2 UIDNEXT 3)',
    ]);
    expect((await imap.command("RENAME before after")).status).toBe("OK");
    expect((await imap.command("STATUS after (MESSAGES UIDNEXT)")).untagged).toEqual([
      '* STATUS "after" (MESSAGES 0 UIDNEXT 1)',
    ]);
    expect((await imap.command("CREATE before")).status).toBe("OK");
    expect((await imap.command("STATUS before (MESSAGES UIDNEXT)")).untagged).toEqual([
      '* STATUS "before" (MESSAGES 2 UIDNEXT 3)',
    ]);
    imap.close();
  });
});

describe("SUBSCRIBE, UNSUBSCRIBE and LSUB", () => {
  test("LSUB lists every mailbox of a new account as subscribed", async () => {
    const imap = await session(await newUser());
    await imap.command("CREATE alpha");
    const result = await imap.command('LSUB "" "*"');
    expect(result.lines).toEqual([
      '* LSUB (\\HasNoChildren) "/" "INBOX"',
      '* LSUB (\\Drafts \\HasNoChildren) "/" "Drafts"',
      '* LSUB (\\Junk \\HasNoChildren) "/" "Junk"',
      '* LSUB (\\Flagged \\HasNoChildren) "/" "Starred"',
      '* LSUB (\\Trash \\HasNoChildren) "/" "Trash"',
      '* LSUB (\\HasNoChildren) "/" "alpha"',
      "A3 OK LSUB completed",
    ]);
    imap.close();
  });

  test("UNSUBSCRIBE removes a user-created mailbox from LSUB and SUBSCRIBE puts it back; LIST is unaffected", async () => {
    const user = await newUser();
    const imap = await session(user);
    await imap.command("CREATE alpha");
    await imap.command("CREATE beta");
    expect((await imap.command("UNSUBSCRIBE alpha")).lines).toEqual(["A4 OK UNSUBSCRIBE completed"]);
    expect((await imap.command('LSUB "" "*a*"')).untagged).toEqual([
      '* LSUB (\\Drafts \\HasNoChildren) "/" "Drafts"',
      '* LSUB (\\Flagged \\HasNoChildren) "/" "Starred"',
      '* LSUB (\\Trash \\HasNoChildren) "/" "Trash"',
      '* LSUB (\\HasNoChildren) "/" "beta"',
    ]);
    expect((await imap.command('LIST "" "alpha"')).untagged).toEqual(['* LIST (\\HasNoChildren) "/" "alpha"']);
    expect(
      await server.db.query("SELECT name, subscribed FROM mailboxes WHERE user_id = $1 ORDER BY name", [user.id])
    ).toEqual([
      { name: "alpha", subscribed: false },
      { name: "beta", subscribed: true },
    ]);

    expect((await imap.command("SUBSCRIBE alpha")).lines).toEqual(["A7 OK SUBSCRIBE completed"]);
    expect((await imap.command('LSUB "" "alpha"')).untagged).toEqual(['* LSUB (\\HasNoChildren) "/" "alpha"']);
    imap.close();
  });

  test("both commands answer OK for unknown names and change nothing for server-defined mailboxes", async () => {
    const user = await newUser();
    await deliver(server, { to: `hello@${user.username}.mail.test` });
    const imap = await session(user);
    const names = ["never-created", "INBOX", "Drafts", '"INBOX/accounts/hello"', '"INBOX/accounts"', '""'];
    for (const name of names) {
      const unsubscribe = await imap.command(`UNSUBSCRIBE ${name}`);
      expect(unsubscribe.lines).toEqual([`${unsubscribe.tag} OK UNSUBSCRIBE completed`]);
    }
    expect((await imap.command('LSUB "" "*"')).untagged).toEqual([
      '* LSUB (\\HasChildren) "/" "INBOX"',
      '* LSUB (\\Drafts \\HasNoChildren) "/" "Drafts"',
      '* LSUB (\\Junk \\HasNoChildren) "/" "Junk"',
      '* LSUB (\\Flagged \\HasNoChildren) "/" "Starred"',
      '* LSUB (\\Trash \\HasNoChildren) "/" "Trash"',
      '* LSUB (\\HasChildren \\Noselect) "/" "INBOX/accounts"',
      '* LSUB (\\HasNoChildren) "/" "INBOX/accounts/hello"',
    ]);
    for (const name of names) {
      const subscribe = await imap.command(`SUBSCRIBE ${name}`);
      expect(subscribe.lines).toEqual([`${subscribe.tag} OK SUBSCRIBE completed`]);
    }
    // Subscribing to an unknown name does not create or list it.
    expect((await imap.command('LSUB "" "never*"')).untagged).toEqual([]);
    imap.close();
  });

  test("LSUB with % names an unsubscribed or missing parent of a subscribed mailbox as \\Noselect", async () => {
    const imap = await session(await newUser());
    await imap.command("CREATE projects/work/q3");
    await imap.command("CREATE top");
    await imap.command("CREATE top/child");
    await imap.command("UNSUBSCRIBE top");

    // `projects` has no mailbox of its own; `top` exists but is unsubscribed.
    expect((await imap.command('LSUB "" "%"')).untagged).toEqual([
      '* LSUB (\\HasNoChildren) "/" "INBOX"',
      '* LSUB (\\Drafts \\HasNoChildren) "/" "Drafts"',
      '* LSUB (\\Junk \\HasNoChildren) "/" "Junk"',
      '* LSUB (\\Flagged \\HasNoChildren) "/" "Starred"',
      '* LSUB (\\Trash \\HasNoChildren) "/" "Trash"',
      '* LSUB (\\HasChildren \\Noselect) "/" "top"',
      '* LSUB (\\HasChildren \\Noselect) "/" "projects"',
    ]);
    expect((await imap.command('LSUB "" "projects/%"')).untagged).toEqual([
      '* LSUB (\\HasChildren \\Noselect) "/" "projects/work"',
    ]);
    // With * the subscribed mailboxes themselves match, so no parent is invented.
    expect((await imap.command('LSUB "" "*/*"')).untagged).toEqual([
      '* LSUB (\\HasNoChildren) "/" "projects/work/q3"',
      '* LSUB (\\HasNoChildren) "/" "top/child"',
    ]);
    imap.close();
  });

  test("LSUB with an empty pattern returns the delimiter", async () => {
    const imap = await session(await newUser());
    expect((await imap.command('LSUB "" ""')).lines).toEqual(['* LSUB (\\Noselect) "/" ""', "A2 OK LSUB completed"]);
    expect((await imap.command('LSUB "ref/" ""')).untagged).toEqual(['* LSUB (\\Noselect) "/" "ref/"']);
    imap.close();
  });
});

describe("STATUS", () => {
  let user: TestUser;
  let imap: ImapClient;

  beforeAll(async () => {
    user = await newUser();
    await deliver(server, { to: `hello@${user.username}.mail.test` });
    await deliver(server, { to: `hello@${user.username}.mail.test` });
    await deliver(server, { to: `work@${user.username}.mail.test` });
    await sendOverHttp(server, user, { sender: "me", to: "x@remote.example", subject: "s", html: "<p>s</p>" });
    imap = await session(user);
    await imap.command("CREATE custom");
  });

  afterAll(() => imap?.close());

  const status = async (args: string) => (await imap.command(`STATUS ${args}`)).lines.map((line) =>
    line.replace(/^A\d+ /, "")
  );

  test("reports every item in the order asked", async () => {
    const lines = await status("INBOX (MESSAGES RECENT UIDNEXT UIDVALIDITY UNSEEN HIGHESTMODSEQ)");
    // UIDVALIDITY is fixed (to the clock) the first time it is asked for, then stored.
    const validity = await storedUidValidity(server, user.username);
    expect(lines).toEqual([
      `* STATUS "INBOX" (MESSAGES 3 RECENT 0 UIDNEXT 4 UIDVALIDITY ${validity} UNSEEN 3 HIGHESTMODSEQ 3)`,
      "OK STATUS completed",
    ]);
    expect(await status("INBOX (UNSEEN MESSAGES)")).toEqual([
      '* STATUS "INBOX" (UNSEEN 3 MESSAGES 3)',
      "OK STATUS completed",
    ]);
  });

  test("item names are case-insensitive; an empty list is answered with an empty list", async () => {
    expect(await status("INBOX (messages Unseen)")).toEqual([
      '* STATUS "INBOX" (MESSAGES 3 UNSEEN 3)',
      "OK STATUS completed",
    ]);
    expect(await status("INBOX ()")).toEqual(['* STATUS "INBOX" ()', "OK STATUS completed"]);
  });

  test("a repeated item is reported each time", async () => {
    expect(await status("INBOX (MESSAGES MESSAGES)")).toEqual([
      '* STATUS "INBOX" (MESSAGES 3 MESSAGES 3)',
      "OK STATUS completed",
    ]);
  });

  test("INBOX and the special-use names are canonicalized in the answer", async () => {
    expect(await status("inbox (MESSAGES)")).toEqual(['* STATUS "INBOX" (MESSAGES 3)', "OK STATUS completed"]);
    expect(await status("drafts (MESSAGES)")).toEqual(['* STATUS "Drafts" (MESSAGES 0)', "OK STATUS completed"]);
    expect(await status("TRASH (MESSAGES UIDNEXT)")).toEqual([
      '* STATUS "Trash" (MESSAGES 0 UIDNEXT 1)',
      "OK STATUS completed",
    ]);
  });

  test("each kind of mailbox counts its own messages and has its own UIDNEXT", async () => {
    expect(await status('"INBOX/accounts/hello" (MESSAGES UIDNEXT UNSEEN)')).toEqual([
      '* STATUS "INBOX/accounts/hello" (MESSAGES 2 UIDNEXT 3 UNSEEN 2)',
      "OK STATUS completed",
    ]);
    expect(await status('"INBOX/accounts/work" (MESSAGES UIDNEXT UNSEEN)')).toEqual([
      '* STATUS "INBOX/accounts/work" (MESSAGES 1 UIDNEXT 2 UNSEEN 1)',
      "OK STATUS completed",
    ]);
    // Mail sent over HTTP is stored read.
    expect(await status('"Sent Messages" (MESSAGES UIDNEXT UNSEEN)')).toEqual([
      '* STATUS "Sent Messages" (MESSAGES 1 UIDNEXT 2 UNSEEN 0)',
      "OK STATUS completed",
    ]);
    expect(await status('"Sent Messages/accounts/me" (MESSAGES UIDNEXT UNSEEN)')).toEqual([
      '* STATUS "Sent Messages/accounts/me" (MESSAGES 1 UIDNEXT 2 UNSEEN 0)',
      "OK STATUS completed",
    ]);
    expect(await status("custom (MESSAGES UIDNEXT UNSEEN RECENT HIGHESTMODSEQ)")).toEqual([
      '* STATUS "custom" (MESSAGES 0 UIDNEXT 1 UNSEEN 0 RECENT 0 HIGHESTMODSEQ 1)',
      "OK STATUS completed",
    ]);
    // Drafts and Junk count in INBOX's UID space, Starred and Trash in their own.
    expect(await status("Drafts (MESSAGES UIDNEXT)")).toEqual([
      '* STATUS "Drafts" (MESSAGES 0 UIDNEXT 4)',
      "OK STATUS completed",
    ]);
    expect(await status("Junk (MESSAGES UIDNEXT)")).toEqual(['* STATUS "Junk" (MESSAGES 0 UIDNEXT 4)', "OK STATUS completed"]);
    expect(await status("Starred (MESSAGES UIDNEXT)")).toEqual([
      '* STATUS "Starred" (MESSAGES 0 UIDNEXT 1)',
      "OK STATUS completed",
    ]);
  });

  test("the \\Noselect parents answer STATUS with zeros", async () => {
    expect(await status('"INBOX/accounts" (MESSAGES UIDNEXT UNSEEN)')).toEqual([
      '* STATUS "INBOX/accounts" (MESSAGES 0 UIDNEXT 1 UNSEEN 0)',
      "OK STATUS completed",
    ]);
    expect(await status('"Sent Messages/accounts" (MESSAGES UIDNEXT UNSEEN)')).toEqual([
      '* STATUS "Sent Messages/accounts" (MESSAGES 0 UIDNEXT 1 UNSEEN 0)',
      "OK STATUS completed",
    ]);
  });

  test("UIDVALIDITY is one value per account, the same for every mailbox", async () => {
    await status("INBOX (UIDVALIDITY)");
    const validity = await storedUidValidity(server, user.username);
    expect(validity).toBeGreaterThan(1_700_000_000);
    expect(validity).toBeLessThanOrEqual(Math.floor(Date.now() / 1000));
    for (const name of ["INBOX", '"INBOX/accounts/hello"', '"Sent Messages"', "custom", "Trash"]) {
      const lines = await status(`${name} (UIDVALIDITY)`);
      expect(lines[0]).toMatch(new RegExp(`\\(UIDVALIDITY ${validity}\\)$`));
    }
  });

  test("an unknown mailbox, a wrong letter case and an empty name do not exist", async () => {
    for (const name of ["never-created", "CUSTOM", '"inbox/accounts/hello"', '""', '"Sent Messages/accounts/nobody"']) {
      expect(await status(`${name} (MESSAGES)`)).toEqual(["NO Mailbox does not exist"]);
    }
  });

  test("works on the selected mailbox and does not change the selection", async () => {
    const other = await session(user, "INBOX");
    const result = await other.command('STATUS "INBOX/accounts/work" (MESSAGES)');
    expect(result.untagged).toEqual(['* STATUS "INBOX/accounts/work" (MESSAGES 1)']);
    expect((await other.command("STATUS INBOX (MESSAGES)")).untagged).toEqual(['* STATUS "INBOX" (MESSAGES 3)']);
    expect((await other.command("FETCH 3 UID")).untagged).toEqual(["* 3 FETCH (UID 3)"]);
    other.close();
  });
});
