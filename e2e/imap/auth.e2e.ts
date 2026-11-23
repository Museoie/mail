import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { ADMIN_PASSWORD, createUser, type TestServer, type TestUser } from "../harness";
import { b64, ImapClient, lit, quote, saslPlain } from "../harness/imap";
import { CAPABILITIES, raws, startImapServer } from "./support";

// Failed sign-ins are counted per client address and never forgiven, and ten
// of them lock the address out for fifteen minutes. Every test on `server`
// connects from 127.0.0.1, so this file as a whole may fail a sign-in at most
// nine times there: each such test notes what it spends. The lock-out itself
// is tested on `proxied`, where each connection names its own client address.
let server: TestServer;
let proxied: TestServer;
let alice: TestUser;
let bob: TestUser;
let quoted: TestUser;

const INVALID = "NO [AUTHENTICATIONFAILED] Invalid credentials.";
const LOCKED = "NO [AUTHENTICATIONFAILED] Too many failed attempts";

beforeAll(async () => {
  [server, proxied] = await Promise.all([
    startImapServer(),
    startImapServer({ env: { PROXY_PROTOCOL: "true" } }),
  ]);
  alice = await createUser(server, "alice");
  bob = await createUser(server, "bob");
  quoted = await createUser(server, "carol", { password: 'pa ss"wo\\rd {5}' });
  await createUser(proxied, "alice");
  await createUser(proxied, "bob");
}, 90_000);

afterAll(async () => {
  await Promise.all([server?.stop(), proxied?.stop()]);
});

describe("LOGIN", () => {
  test("signs the built-in admin in and repeats the capability list", async () => {
    const imap = await ImapClient.connect(server);
    const result = await imap.command(`LOGIN admin ${ADMIN_PASSWORD}`);
    expect(result.lines).toEqual([`A1 OK [CAPABILITY ${CAPABILITIES}] LOGIN completed`]);
    expect((await imap.command('LIST "" INBOX')).status).toBe("OK");
    imap.close();
  });

  test("signs a registered user in by username", async () => {
    const imap = await ImapClient.connect(server);
    const result = await imap.command(`LOGIN alice ${alice.password}`);
    expect(result.lines).toEqual([`A1 OK [CAPABILITY ${CAPABILITIES}] LOGIN completed`]);
    imap.close();
  });

  test("accepts quoted credentials", async () => {
    const imap = await ImapClient.connect(server);
    const result = await imap.command(`LOGIN "alice" "${alice.password}"`);
    expect(result.status).toBe("OK");
    imap.close();
  });

  test("accepts a quoted password with spaces, escaped quotes and a literal-like tail", async () => {
    const imap = await ImapClient.connect(server);
    const result = await imap.command(`LOGIN carol ${quote(quoted.password)}`);
    expect(result.lines).toEqual([`A1 OK [CAPABILITY ${CAPABILITIES}] LOGIN completed`]);
    imap.close();
  });

  test("accepts the same password as a literal", async () => {
    const imap = await ImapClient.connect(server);
    const result = await imap.command(["LOGIN carol ", lit(quoted.password, false)]);
    expect(result.lines).toEqual([`A1 OK [CAPABILITY ${CAPABILITIES}] LOGIN completed`]);
    imap.close();
  });

  test("ignores arguments after the password", async () => {
    const imap = await ImapClient.connect(server);
    const result = await imap.command(`LOGIN alice ${alice.password} extra words`);
    expect(result.status).toBe("OK");
    imap.close();
  });

  // Spends 3 failures.
  test("refuses a wrong password, an unknown user and empty credentials with the same answer", async () => {
    const imap = await ImapClient.connect(server);
    const started = Date.now();
    expect((await imap.command("LOGIN alice wrong-password")).lines).toEqual([`A1 ${INVALID}`]);
    // Each failure is answered after a half-second delay.
    expect(Date.now() - started).toBeGreaterThanOrEqual(450);
    expect((await imap.command("LOGIN nobody-here wrong-password")).lines).toEqual([`A2 ${INVALID}`]);
    expect((await imap.command('LOGIN "" ""')).lines).toEqual([`A3 ${INVALID}`]);
    // The connection stays open and unauthenticated.
    expect((await imap.command('LIST "" "*"')).lines).toEqual(["A4 NO Not authenticated."]);
    imap.close();
  });

  // Spends 3 failures.
  test("matches the username exactly: another letter case or an address form is refused", async () => {
    const imap = await ImapClient.connect(server);
    expect((await imap.command(`LOGIN ALICE ${alice.password}`)).lines).toEqual([`A1 ${INVALID}`]);
    expect((await imap.command(`LOGIN alice@mail.test ${alice.password}`)).lines).toEqual([`A2 ${INVALID}`]);
    expect((await imap.command(`LOGIN hello@alice.mail.test ${alice.password}`)).lines).toEqual([
      `A3 ${INVALID}`,
    ]);
    imap.close();
  });

  test("on an authenticated connection signs in again, even as someone else", async () => {
    const imap = await ImapClient.connect(server);
    await imap.login("alice", alice.password);
    await imap.command("CREATE alice-only");
    expect((await imap.command('LIST "" alice-only')).untagged).toEqual([
      '* LIST (\\HasNoChildren) "/" "alice-only"',
    ]);

    const again = await imap.command(`LOGIN alice ${alice.password}`);
    expect(again.lines).toEqual([`A4 OK [CAPABILITY ${CAPABILITIES}] LOGIN completed`]);

    const switched = await imap.command(`LOGIN bob ${bob.password}`);
    expect(switched.lines).toEqual([`A5 OK [CAPABILITY ${CAPABILITIES}] LOGIN completed`]);
    // The connection now sees bob's mailboxes, not alice's.
    expect((await imap.command('LIST "" alice-only')).lines).toEqual(["A6 OK LIST completed"]);
    imap.close();
  });

  test("a second LOGIN keeps the mailbox the first one selected", async () => {
    // NOTE: the selected mailbox and its sequence map survive a re-LOGIN as a
    // different user: FETCH is then answered from the new user's store under
    // the old mailbox name.
    const imap = await ImapClient.connect(server);
    await imap.login("alice", alice.password);
    await imap.command("SELECT INBOX");
    await imap.login("bob", bob.password);
    expect((await imap.command("CLOSE")).lines).toEqual(["A4 OK CLOSE completed"]);
    imap.close();
  });

  // Spends 1 failure.
  test("a failed LOGIN on an authenticated connection leaves the first sign-in in place", async () => {
    const imap = await ImapClient.connect(server);
    await imap.login("alice", alice.password);
    expect((await imap.command("LOGIN bob wrong-password")).lines).toEqual([`A2 ${INVALID}`]);
    expect((await imap.command('LIST "" INBOX')).status).toBe("OK");
    imap.close();
  });
});

describe("AUTHENTICATE PLAIN", () => {
  test("signs in with an initial response", async () => {
    const imap = await ImapClient.connect(server);
    const result = await imap.command(`AUTHENTICATE PLAIN ${saslPlain("alice", alice.password)}`);
    expect(result.lines).toEqual([`A1 OK [CAPABILITY ${CAPABILITIES}] AUTHENTICATE completed`]);
    expect((await imap.command("SELECT INBOX")).status).toBe("OK");
    imap.close();
  });

  test("without an initial response sends an empty challenge and takes the answer on the next line", async () => {
    const imap = await ImapClient.connect(server);
    const first = await imap.command("AUTHENTICATE PLAIN", { stopAtContinuation: true });
    expect(first.lines).toEqual(["+ "]);
    // The challenge is "+", one space, CRLF.
    expect(imap.transcript.endsWith("+ \r\n")).toBe(true);
    const done = await imap.respond(first.tag, saslPlain("alice", alice.password));
    expect(done.lines).toEqual([`A1 OK [CAPABILITY ${CAPABILITIES}] AUTHENTICATE completed`]);
    // The line after the exchange is a command again.
    expect((await imap.command("NOOP")).lines).toEqual(["A2 OK NOOP completed"]);
    imap.close();
  });

  test("ignores the authorization identity", async () => {
    const imap = await ImapClient.connect(server);
    const result = await imap.command(
      `AUTHENTICATE PLAIN ${saslPlain("alice", alice.password, "someone-else")}`
    );
    expect(result.status).toBe("OK");
    // Signed in as alice (the authentication identity): bob's password would not open this.
    expect((await imap.command('LIST "" INBOX')).status).toBe("OK");
    imap.close();
  });

  test("is cancelled by * with BAD", async () => {
    const imap = await ImapClient.connect(server);
    const first = await imap.command("AUTHENTICATE PLAIN", { stopAtContinuation: true });
    const done = await imap.respond(first.tag, "*");
    expect(done.lines).toEqual(["A1 BAD Authentication cancelled"]);
    expect((await imap.command("NOOP")).lines).toEqual(["A2 OK NOOP completed"]);
    imap.close();
  });

  test("answers an empty or blank response line with another challenge", async () => {
    const imap = await ImapClient.connect(server);
    const first = await imap.command("AUTHENTICATE PLAIN", { stopAtContinuation: true });
    imap.write("\r\n");
    expect(await imap.continuation()).toBe("+ ");
    imap.write("   \r\n");
    expect(await imap.continuation()).toBe("+ ");
    const done = await imap.respond(first.tag, "*");
    expect(done.lines).toEqual(["A1 BAD Authentication cancelled"]);
    imap.close();
  });

  test("rejects a response that is not three NUL-separated fields with BAD", async () => {
    const imap = await ImapClient.connect(server);
    const malformed = [
      "!!!notbase64!!!",
      "=",
      b64(`alice\0${alice.password}`),
      b64("a\0b\0c\0d"),
      b64("no separators at all"),
    ];
    for (const [index, response] of malformed.entries()) {
      const result = await imap.command(`AUTHENTICATE PLAIN ${response}`);
      expect({ response, lines: result.lines }).toEqual({
        response,
        lines: [`A${index + 1} BAD Invalid PLAIN response format`],
      });
    }
    // The same through the challenge.
    const first = await imap.command("AUTHENTICATE PLAIN", { stopAtContinuation: true });
    const done = await imap.respond(first.tag, "!!!notbase64!!!");
    expect(done.lines).toEqual([`${first.tag} BAD Invalid PLAIN response format`]);
    imap.close();
  });

  // Spends 1 failure.
  test("refuses a wrong password the way LOGIN does", async () => {
    const imap = await ImapClient.connect(server);
    const result = await imap.command(`AUTHENTICATE PLAIN ${saslPlain("alice", "wrong-password")}`);
    expect(result.lines).toEqual([`A1 ${INVALID}`]);
    imap.close();
  });

  test("refuses every other mechanism, and PLAIN in lower case", async () => {
    const imap = await ImapClient.connect(server);
    const response = saslPlain("alice", alice.password);
    for (const [index, mechanism] of ["LOGIN", "CRAM-MD5", "XOAUTH2", "plain", "Plain"].entries()) {
      const result = await imap.command(`AUTHENTICATE ${mechanism} ${response}`);
      expect({ mechanism, lines: result.lines }).toEqual({
        mechanism,
        lines: [`A${index + 1} NO Only PLAIN authentication supported`],
      });
    }
    // A mechanism the server refuses draws no challenge either.
    expect((await imap.command("AUTHENTICATE LOGIN")).lines).toEqual([
      "A6 NO Only PLAIN authentication supported",
    ]);
    imap.close();
  });

  test("on an authenticated connection signs in again", async () => {
    const imap = await ImapClient.connect(server);
    await imap.login("alice", alice.password);
    const result = await imap.command(`AUTHENTICATE PLAIN ${saslPlain("bob", bob.password)}`);
    expect(result.lines).toEqual([`A2 OK [CAPABILITY ${CAPABILITIES}] AUTHENTICATE completed`]);
    imap.close();
  });
});

describe("a credential changed after sign-in", () => {
  test("the next command is refused, the connection drops to not-authenticated and can sign in again", async () => {
    const dave = await createUser(server, "dave");
    const imap = await ImapClient.connect(server);
    await imap.login("dave", dave.password);
    await imap.command("SELECT INBOX");

    // What a password change does to the account row, wherever it was made.
    await server.db.query(
      "UPDATE users SET password_version = COALESCE(password_version, 1) + 1 WHERE username = 'dave'"
    );

    expect((await imap.command("NOOP")).lines).toEqual([
      "A3 NO [AUTHENTICATIONFAILED] Credential changed, please re-authenticate",
    ]);
    expect((await imap.command("FETCH 1:* FLAGS")).lines).toEqual(["A4 NO Not authenticated."]);
    // Commands valid before sign-in still work.
    expect((await imap.command("NOOP")).lines).toEqual(["A5 OK NOOP completed"]);

    const again = await imap.command(`LOGIN dave ${dave.password}`);
    expect(again.status).toBe("OK");
    expect((await imap.command("NOOP")).lines).toEqual(["A7 OK NOOP completed"]);
    imap.close();
  });

  test("deleting or soft-deleting the account refuses the next command the same way", async () => {
    for (const [name, remove] of [
      ["gina", "DELETE FROM users WHERE username = 'gina'"],
      ["hank", "UPDATE users SET is_deleted = TRUE WHERE username = 'hank'"],
    ] as const) {
      const user = await createUser(server, name);
      const imap = await ImapClient.connect(server);
      await imap.login(name, user.password);
      await imap.command("SELECT INBOX");

      await server.db.query(remove);

      expect({ name, lines: (await imap.command("NOOP")).lines }).toEqual({
        name,
        lines: ["A3 NO [AUTHENTICATIONFAILED] Credential changed, please re-authenticate"],
      });
      expect((await imap.command("FETCH 1:* FLAGS")).lines).toEqual(["A4 NO Not authenticated."]);
      imap.close();
    }
  });

  test("LOGOUT is still answered normally", async () => {
    const erin = await createUser(server, "erin");
    const imap = await ImapClient.connect(server);
    await imap.login("erin", erin.password);
    await server.db.query(
      "UPDATE users SET password_version = COALESCE(password_version, 1) + 1 WHERE username = 'erin'"
    );
    expect((await imap.command("LOGOUT")).lines).toEqual([
      "* BYE IMAP4rev1 Server logging out",
      "A2 OK LOGOUT completed",
    ]);
    await imap.waitForClose();
  });
});

describe("the failed sign-in limit", () => {
  let nextAddress = 10;
  const address = () => `203.0.113.${nextAddress++}`;
  const from = (ip: string) =>
    ImapClient.connect(proxied, { preface: `PROXY TCP4 ${ip} 10.0.0.1 40000 143\r\n` });

  /** Fails `count` sign-ins from `ip` on one connection and returns the tagged answers. */
  const fail = async (ip: string, count: number, username = "alice") => {
    const imap = await from(ip);
    const answers: string[] = [];
    for (let i = 0; i < count; i++) {
      answers.push((await imap.command(`LOGIN ${username} wrong-${i}`)).text!);
    }
    return { imap, answers };
  };

  // Every failure costs half a second, so the three ten-failure scenarios run
  // side by side here, each from its own address, and the tests read them.
  let lockedOut: { ip: string; imap: ImapClient; answers: string[] };
  let mixedAccounts: { answers: string[]; closed: boolean };
  let afterSuccess: { answers: string[]; success: string; tenth: string[]; rest: string[] };

  beforeAll(async () => {
    const scenarioLockedOut = async () => {
      const ip = address();
      return { ip, ...(await fail(ip, 10)) };
    };
    const scenarioMixedAccounts = async () => {
      const ip = address();
      const first = await fail(ip, 5, "alice");
      first.imap.close();
      const second = await fail(ip, 5, "bob");
      await second.imap.waitForClose();
      return { answers: [...first.answers, ...second.answers], closed: second.imap.closed };
    };
    const scenarioAfterSuccess = async () => {
      const { imap, answers } = await fail(address(), 9);
      const success = (await imap.command("LOGIN alice password-of-alice")).status!;
      const tenth = (await imap.command("LOGIN alice wrong-again")).lines;
      const rest = raws(await imap.waitForClose());
      return { answers, success, tenth, rest };
    };
    [lockedOut, mixedAccounts, afterSuccess] = await Promise.all([
      scenarioLockedOut(),
      scenarioMixedAccounts(),
      scenarioAfterSuccess(),
    ]);
  }, 60_000);

  test("the tenth failure from an address is answered 'Too many failed attempts' and the connection is closed", async () => {
    expect(lockedOut.answers).toEqual([
      ...Array(9).fill("[AUTHENTICATIONFAILED] Invalid credentials."),
      "[AUTHENTICATIONFAILED] Too many failed attempts",
    ]);
    expect(await lockedOut.imap.waitForClose()).toEqual([]);
  });

  test("a locked-out address is refused unchecked and disconnected, whatever the account or mechanism", async () => {
    const password = "password-of-alice";
    for (const command of [
      `LOGIN alice ${password}`,
      "LOGIN bob password-of-bob",
      `LOGIN admin ${ADMIN_PASSWORD}`,
      `AUTHENTICATE PLAIN ${saslPlain("alice", password)}`,
    ]) {
      const locked = await from(lockedOut.ip);
      // Commands that need no sign-in are still served.
      expect((await locked.command("NOOP")).lines).toEqual(["A1 OK NOOP completed"]);
      expect((await locked.command(command)).lines).toEqual([`A2 ${LOCKED}`]);
      expect(await locked.waitForClose()).toEqual([]);
    }
  });

  test("another client address is not affected", async () => {
    const other = await from(address());
    expect((await other.command("LOGIN alice password-of-alice")).status).toBe("OK");
    other.close();
  });

  test("failures for different accounts from one address add up", () => {
    expect(mixedAccounts.answers).toEqual([
      ...Array(9).fill("[AUTHENTICATIONFAILED] Invalid credentials."),
      "[AUTHENTICATIONFAILED] Too many failed attempts",
    ]);
    expect(mixedAccounts.closed).toBe(true);
  });

  test("a successful sign-in does not forgive earlier failures", () => {
    expect(afterSuccess.answers).toEqual(Array(9).fill("[AUTHENTICATIONFAILED] Invalid credentials."));
    expect(afterSuccess.success).toBe("OK");
    // The tenth failure still locks the address, even on the signed-in connection.
    expect(afterSuccess.tenth).toEqual([`A11 ${LOCKED}`]);
    expect(afterSuccess.rest).toEqual([]);
  });

  test("a malformed AUTHENTICATE response is not counted as a failure", async () => {
    const imap = await from(address());
    for (let i = 0; i < 12; i++) {
      expect((await imap.command("AUTHENTICATE PLAIN !!!")).text).toBe("Invalid PLAIN response format");
    }
    expect((await imap.command("LOGIN alice password-of-alice")).status).toBe("OK");
    imap.close();
  });

  test("an account with no password yet (a sign-up in progress) is refused as a wrong password is, and counted", async () => {
    // A sign-up is a users row with a NULL password until set-info writes one.
    await proxied.db.query(
      "INSERT INTO users (username, email, token, expiry) VALUES ('user_pending1', 'pending@elsewhere.example', 'tok', NOW() + INTERVAL '1 hour')"
    );
    const { imap, answers } = await fail(address(), 9, "user_pending1");
    expect(answers).toEqual(Array(9).fill("[AUTHENTICATIONFAILED] Invalid credentials."));
    expect((await imap.command(`AUTHENTICATE PLAIN ${saslPlain("user_pending1", "tok")}`)).lines).toEqual([`A10 ${LOCKED}`]);
    expect(await imap.waitForClose()).toEqual([]);
  }, 30_000);
});
