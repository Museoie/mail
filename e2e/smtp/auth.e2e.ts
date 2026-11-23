import { afterAll, afterEach, beforeAll, describe, expect, test } from "bun:test";
import { ADMIN_PASSWORD, createUser, type TestServer, type TestUser } from "../harness";
import { b64, plainToken, proxyLine, type SmtpClient } from "../harness/smtp";
import { withDatabaseOffline } from "../http/core-helpers";
import { bootAll, closeClients, ehloReply, open, session, uniqueName, type Listener } from "./support";

// Failed AUTH attempts are counted per client address, so the main server
// here sits behind the PROXY protocol and every test names its own address.
// `bare` has no certificate at all.
let server: TestServer;
let bare: TestServer;
let alice: TestUser;


beforeAll(async () => {
  [server, bare] = (await bootAll({ tls: true, env: { PROXY_PROTOCOL: "true" } }, {})) as [TestServer, TestServer];
  alice = await createUser(server, "alice");
  await createUser(bare, "alice");
}, 180_000);

afterEach(closeClients);

afterAll(async () => {
  await Promise.all([server?.stop(), bare?.stop()]);
});

let nextAddress = 0;
const freshAddress = () => {
  const n = ++nextAddress;
  return `10.88.${Math.floor(n / 250)}.${(n % 250) + 1}`;
};

/** A greeted session on one of the listeners, from an address no other test uses. */
const connect = async (listener: Listener, address = freshAddress()) => {
  const client = await open(server, listener, { proxy: proxyLine(address) });
  await client.ehlo();
  return client;
};

/** The port-25 listener after STARTTLS and a second EHLO. */
const encrypted25 = async (address = freshAddress()) => {
  const client = await connect("smtp", address);
  expect((await client.startTls()).raw).toBe("220 Ready to start TLS");
  await client.ehlo();
  return client;
};

const SUCCESS = "235 Authentication successful";
const INVALID = "535 Error: Authentication credentials invalid";
const STARTTLS_FIRST = "538 Error: Must issue a STARTTLS command first";
const USERNAME_PROMPT = "334 VXNlcm5hbWU6";
const PASSWORD_PROMPT = "334 UGFzc3dvcmQ6";

describe("where AUTH is taken", () => {
  test("port 25 refuses AUTH in cleartext, whatever follows the verb", async () => {
    const smtp = await connect("smtp");
    for (const line of [
      `AUTH PLAIN ${plainToken("alice", alice.password)}`,
      "AUTH PLAIN",
      "AUTH LOGIN",
      "AUTH CRAM-MD5",
      "AUTH",
      "auth plain a b c",
    ]) {
      expect({ line, reply: (await smtp.send(line)).raw }).toEqual({ line, reply: STARTTLS_FIRST });
    }
    // Nothing was started: the next line is a command, not a SASL response.
    expect((await smtp.send("NOOP")).raw).toBe("250 OK");
  });

  test("port 25 takes AUTH once STARTTLS has run", async () => {
    const smtp = await encrypted25();
    expect((await smtp.authPlain("alice", alice.password)).raw).toBe(SUCCESS);
  });

  test("the submission port takes AUTH in cleartext and after STARTTLS", async () => {
    const cleartext = await connect("submission");
    expect((await cleartext.authPlain("alice", alice.password)).raw).toBe(SUCCESS);

    const upgraded = await connect("submission");
    await upgraded.startTls();
    await upgraded.ehlo();
    expect((await upgraded.authLogin("alice", alice.password)).raw).toBe(SUCCESS);
  });

  test("the implicit-TLS port takes AUTH", async () => {
    const smtp = await connect("smtps");
    expect((await smtp.authPlain("alice", alice.password)).raw).toBe(SUCCESS);
  });

  test("without a certificate, port 25 advertises AUTH but can never accept it", async () => {
    // NOTE: EHLO lists `AUTH LOGIN PLAIN` although there is no STARTTLS to
    // make it usable; every AUTH is answered 538.
    const smtp = await session(bare);
    expect(smtp.transcript[1]!.raw).toBe(ehloReply({ auth: true, starttls: false }));
    expect((await smtp.authPlain("alice", "password-of-alice")).raw).toBe(STARTTLS_FIRST);
    expect((await smtp.send("STARTTLS")).raw).toBe("454 Error: TLS not available");
  });

  test("AUTH needs EHLO first", async () => {
    const smtp = await open(server, "smtps", { proxy: proxyLine(freshAddress()) });
    expect((await smtp.send(`AUTH PLAIN ${plainToken("alice", alice.password)}`)).raw).toBe("503 Error: send HELO/EHLO first");
  });

  test("a session opened with HELO can authenticate", async () => {
    const smtp = await open(server, "smtps", { proxy: proxyLine(freshAddress()) });
    await smtp.helo();
    expect((await smtp.authPlain("alice", alice.password)).raw).toBe(SUCCESS);
  });
});

describe("AUTH PLAIN", () => {
  test("with the response on the command line", async () => {
    const smtp = await connect("smtps");
    expect((await smtp.send(`AUTH PLAIN ${plainToken("alice", alice.password)}`)).raw).toBe(SUCCESS);
  });

  test("with the response after an empty 334 challenge", async () => {
    const smtp = await connect("smtps");
    const challenge = await smtp.send("AUTH PLAIN");
    expect(challenge.raw).toBe("334 ");
    expect((await smtp.send(plainToken("alice", alice.password))).raw).toBe(SUCCESS);
  });

  test("the mechanism name is matched in any letter case, with extra spaces", async () => {
    const smtp = await connect("smtps");
    expect((await smtp.send(`auth   plain   ${plainToken("alice", alice.password)}  `)).raw).toBe(SUCCESS);
  });

  test("an authorization identity is ignored when an authentication identity is given", async () => {
    const smtp = await connect("smtps");
    expect((await smtp.send(`AUTH PLAIN ${plainToken("alice", alice.password, "admin")}`)).raw).toBe(SUCCESS);
    // The session is alice's, not admin's: admin's own address is not hers to send from.
    await smtp.send("MAIL FROM:<x@alice.mail.test>");
    expect((await smtp.send("RCPT TO:<someone@remote.example>")).raw).toBe("250 Accepted");
  });

  test("with an empty authentication identity, the authorization identity is the username", async () => {
    const smtp = await connect("smtps");
    expect((await smtp.send(`AUTH PLAIN ${b64(`alice\0\0${alice.password}`)}`)).raw).toBe(SUCCESS);
  });

  test("a response that is not three NUL-separated fields is 500", async () => {
    const smtp = await connect("smtps");
    const invalid = "500 Error: invalid userdata";
    for (const token of ["!!!!", b64("alice"), b64(`alice\0${alice.password}`), b64(`\0alice\0${alice.password}\0extra`), "=", "*"]) {
      expect({ token, reply: (await smtp.send(`AUTH PLAIN ${token}`)).raw }).toEqual({ token, reply: invalid });
    }
    await smtp.send("AUTH PLAIN");
    expect((await smtp.send("")).raw).toBe(invalid);
    await smtp.send("AUTH PLAIN");
    expect((await smtp.send("not base64 at all")).raw).toBe(invalid);
    // None of that was an attempt: the right credentials still work.
    expect((await smtp.authPlain("alice", alice.password)).raw).toBe(SUCCESS);
  });

  test("more than one argument is a syntax error", async () => {
    const smtp = await connect("smtps");
    expect((await smtp.send(`AUTH PLAIN ${plainToken("alice", alice.password)} extra`)).raw).toBe("501 Error: syntax: AUTH PLAIN token");
  });

  test("a lone * cancels the continued exchange, and the session carries on", async () => {
    const smtp = await connect("smtps");
    expect((await smtp.send("AUTH PLAIN")).raw).toBe("334 ");
    expect((await smtp.send("*")).raw).toBe("501 Authentication aborted");
    expect((await smtp.send("NOOP")).raw).toBe("250 OK");
    expect((await smtp.authPlain("alice", alice.password, { continued: true })).raw).toBe(SUCCESS);
  });

  test("while a challenge is open, the next line is the response whatever it looks like", async () => {
    const smtp = await connect("smtps");
    expect((await smtp.send("AUTH PLAIN")).raw).toBe("334 ");
    expect((await smtp.send("QUIT")).raw).toBe("500 Error: invalid userdata");
    expect((await smtp.send("NOOP")).raw).toBe("250 OK");
  });

  test("except an HTTP request line, which ends the connection even there", async () => {
    const smtp = await connect("smtps");
    expect((await smtp.send("AUTH PLAIN")).raw).toBe("334 ");
    expect((await smtp.send("GET / HTTP/1.1")).raw).toBe("421 HTTP requests not allowed");
    expect(await smtp.waitForClose(5000)).toBe(true);
  });
});

describe("AUTH LOGIN", () => {
  test("the username and password are asked for in turn", async () => {
    const smtp = await connect("smtps");
    expect((await smtp.send("AUTH LOGIN")).raw).toBe(USERNAME_PROMPT);
    expect((await smtp.send(b64("alice"))).raw).toBe(PASSWORD_PROMPT);
    expect((await smtp.send(b64(alice.password))).raw).toBe(SUCCESS);
    expect(Buffer.from("VXNlcm5hbWU6", "base64").toString()).toBe("Username:");
    expect(Buffer.from("UGFzc3dvcmQ6", "base64").toString()).toBe("Password:");
  });

  test("the username may come on the command line", async () => {
    const smtp = await connect("smtps");
    expect((await smtp.send(`AUTH LOGIN ${b64("alice")}`)).raw).toBe(PASSWORD_PROMPT);
    expect((await smtp.send(b64(alice.password))).raw).toBe(SUCCESS);
  });

  test("a lone * cancels at either prompt", async () => {
    const smtp = await connect("smtps");
    await smtp.send("AUTH LOGIN");
    expect((await smtp.send("*")).raw).toBe("501 Authentication aborted");
    await smtp.send("AUTH LOGIN");
    await smtp.send(b64("alice"));
    expect((await smtp.send(" * ")).raw).toBe("501 Authentication aborted");
    expect((await smtp.authLogin("alice", alice.password)).raw).toBe(SUCCESS);
  });

  test("an empty username is refused before the password is asked for", async () => {
    const smtp = await connect("smtps");
    const missing = "500 Error: missing username";
    await smtp.send("AUTH LOGIN");
    expect((await smtp.send("")).raw).toBe(missing);
    expect((await smtp.send("AUTH LOGIN =")).raw).toBe(missing);
    // On the command line a * is not a cancellation; it decodes to nothing.
    expect((await smtp.send("AUTH LOGIN *")).raw).toBe(missing);
  });

  test("more than one argument is a syntax error", async () => {
    const smtp = await connect("smtps");
    expect((await smtp.send(`AUTH LOGIN ${b64("alice")} ${b64(alice.password)}`)).raw).toBe("501 Error: syntax: AUTH LOGIN");
  });
});

describe("other mechanisms", () => {
  test("anything but PLAIN and LOGIN is 504", async () => {
    const smtp = await connect("smtps");
    for (const line of ["AUTH CRAM-MD5", "AUTH XOAUTH2 dXNlcg==", "AUTH", "AUTH  ", "AUTH PLAINX", "AUTH GSSAPI a b"]) {
      expect({ line, reply: (await smtp.send(line)).raw }).toEqual({ line, reply: "504 Error: Unrecognized authentication type" });
    }
  });
});

describe("credentials", () => {
  test("a wrong password is 535, after half a second", async () => {
    const smtp = await connect("smtps");
    const started = Date.now();
    expect((await smtp.authPlain("alice", "not-her-password")).raw).toBe(INVALID);
    expect(Date.now() - started).toBeGreaterThanOrEqual(450);
    // The connection stays open and the right password then works.
    expect((await smtp.authPlain("alice", alice.password)).raw).toBe(SUCCESS);
  });

  test("an unknown user gets the same answer after the same wait", async () => {
    const smtp = await connect("smtps");
    const started = Date.now();
    expect((await smtp.authLogin("nobody-here", "whatever")).raw).toBe(INVALID);
    expect(Date.now() - started).toBeGreaterThanOrEqual(450);
  });

  test("an empty password is refused", async () => {
    const smtp = await connect("smtps");
    expect((await smtp.send(`AUTH PLAIN ${b64("\0alice\0")}`)).raw).toBe(INVALID);
    await smtp.send("AUTH LOGIN");
    await smtp.send(b64("alice"));
    expect((await smtp.send("")).raw).toBe(INVALID);
  });

  // `<account>@<domain>` is a login scoped to a domain the account owns; an
  // address on a domain it doesn't own is no login at all.
  test("the username is the account name, or the account at a domain it owns: no other letter case", async () => {
    const smtp = await connect("smtps");
    for (const username of [alice.email, "alice@mail.test", "Alice", "ALICE", " alice", "Alice@alice.mail.test"]) {
      expect({ username, reply: (await smtp.authPlain(username, alice.password)).raw }).toEqual({ username, reply: INVALID });
    }
  });

  test("the built-in admin signs in with the configured password", async () => {
    const smtp = await connect("smtps");
    expect((await smtp.authPlain("admin", ADMIN_PASSWORD)).raw).toBe(SUCCESS);
  });

  test("a password longer than 72 octets is compared by its first 72", async () => {
    const long = "p".repeat(80);
    const user = await createUser(server, uniqueName("long"), { password: long });
    const smtp = await connect("smtps");
    expect((await smtp.authPlain(user.username, `${"p".repeat(72)}different-tail`)).raw).toBe(SUCCESS);
    const other = await connect("smtps");
    expect((await other.authPlain(user.username, "p".repeat(71))).raw).toBe(INVALID);
  });

  test("a soft-deleted account cannot sign in", async () => {
    const user = await createUser(server, uniqueName("gone"));
    await server.db.query("UPDATE users SET is_deleted = TRUE WHERE username = $1", [user.username]);
    const smtp = await connect("smtps");
    expect((await smtp.authPlain(user.username, user.password)).raw).toBe(INVALID);
  });

  test("an account that has no password yet (a sign-up in progress) is refused as a wrong password is", async () => {
    // A sign-up is a users row with a NULL password until set-info writes one:
    // the same 535 an unknown name gets, after the same wait, and counted.
    const name = uniqueName("pending");
    await server.db.query("INSERT INTO users (username, email) VALUES ($1, $2)", [name, `${name}@elsewhere.example`]);
    const smtp = await connect("smtps");
    const started = Date.now();
    expect((await smtp.authPlain(name, "anything")).raw).toBe(INVALID);
    expect(Date.now() - started).toBeGreaterThanOrEqual(450);
    for (let i = 0; i < 9; i++) expect((await smtp.authLogin(name, `anything-${i}`)).raw).toBe(INVALID);
    expect((await smtp.authPlain("alice", alice.password)).raw).toBe("535 Too many failed authentication attempts");
  }, 30_000);
});

describe("once authenticated", () => {
  test("EHLO no longer offers AUTH, and a second AUTH is refused", async () => {
    const address = freshAddress();
    const smtp = await connect("smtps", address);
    await smtp.authPlain("alice", alice.password);
    expect((await smtp.ehlo()).raw).toBe(ehloReply({ address, auth: false }));
    const refused = "503 Error: No identity changes permitted";
    expect((await smtp.send(`AUTH PLAIN ${plainToken("admin", ADMIN_PASSWORD)}`)).raw).toBe(refused);
    expect((await smtp.send("AUTH LOGIN")).raw).toBe(refused);
    expect((await smtp.send("AUTH NOPE")).raw).toBe(refused);
    // RSET and a new EHLO do not sign the session out.
    await smtp.send("RSET");
    expect((await smtp.send("AUTH PLAIN")).raw).toBe(refused);
  });

  test("STARTTLS on the submission port forgets a cleartext sign-in", async () => {
    const address = freshAddress();
    const smtp = await connect("submission", address);
    expect((await smtp.authPlain("alice", alice.password)).raw).toBe(SUCCESS);
    expect((await smtp.ehlo()).raw).toBe(ehloReply({ address, auth: false, starttls: true }));
    await smtp.startTls();
    expect((await smtp.ehlo()).raw).toBe(ehloReply({ address, auth: true }));
    expect((await smtp.authPlain("admin", ADMIN_PASSWORD)).raw).toBe(SUCCESS);
  });

  test("AUTH is accepted in the middle of a transaction and leaves the envelope alone", async () => {
    // NOTE: RFC 4954 §4 forbids AUTH during a mail transaction; here it is taken.
    const smtp = await connect("smtps");
    expect((await smtp.send("MAIL FROM:<x@alice.mail.test>")).raw).toBe("250 Accepted");
    expect((await smtp.authPlain("alice", alice.password)).raw).toBe(SUCCESS);
    expect((await smtp.send("RCPT TO:<someone@remote.example>")).raw).toBe("250 Accepted");
  });

  test("a login scoped to a domain the account only views is refused like a wrong password", async () => {
    // A viewer reads a domain and never sends as it.
    await server.db.query(
      `INSERT INTO domain_roles (domain_id, user_id, role)
       SELECT domain_id, $1, 'viewer' FROM domains WHERE name = 'mail.test'
       ON CONFLICT DO NOTHING`,
      [alice.id]
    );
    try {
      const smtp = await connect("smtps");
      expect((await smtp.authPlain("alice@mail.test", alice.password)).raw).toBe(INVALID);
      // Her own domain, scoped or not, still signs her in.
      expect((await smtp.authPlain("alice@alice.mail.test", alice.password)).raw).toBe(SUCCESS);
    } finally {
      await server.db.query("DELETE FROM domain_roles WHERE user_id = $1", [alice.id]);
    }
  });
});

describe("a credential that changes under an open session", () => {
  const CHANGED = "421 4.4.2 Credential changed, please re-authenticate";
  const signedIn = async (user: TestUser): Promise<SmtpClient> => {
    const smtp = await connect("smtps");
    expect((await smtp.authPlain(user.username, user.password)).raw).toBe(SUCCESS);
    return smtp;
  };
  const rotate = (user: TestUser) =>
    server.db.query("UPDATE users SET password_version = COALESCE(password_version, 1) + 1 WHERE username = $1", [user.username]);

  test("MAIL FROM is answered 421 and the connection closed", async () => {
    const user = await createUser(server, uniqueName("rot"));
    const smtp = await signedIn(user);
    await rotate(user);
    expect((await smtp.send("NOOP")).raw).toBe("250 OK");
    expect((await smtp.send(`MAIL FROM:<x@${user.username}.mail.test>`)).raw).toBe(CHANGED);
    expect(await smtp.waitForClose(5000)).toBe(true);
  });

  test("a deleted or soft-deleted account is refused at MAIL FROM the same way", async () => {
    for (const remove of ["DELETE FROM users WHERE username = $1", "UPDATE users SET is_deleted = TRUE WHERE username = $1"]) {
      const user = await createUser(server, uniqueName("gone"));
      const smtp = await signedIn(user);
      await server.db.query(remove, [user.username]);
      expect((await smtp.send("NOOP")).raw).toBe("250 OK");
      expect({ remove, reply: (await smtp.send(`MAIL FROM:<x@${user.username}.mail.test>`)).raw }).toEqual({ remove, reply: CHANGED });
      expect(await smtp.waitForClose(5000)).toBe(true);
    }
  });

  test("RCPT TO is answered 451, not 421, when the change lands after MAIL FROM", async () => {
    // NOTE: looks like a bug. The recipient check does notice the changed
    // credential, but its own catch-all turns that refusal into the transient
    // lookup failure, so the client is told to retry on a connection that
    // will never be allowed to send. The session stays open; no recipient can
    // be added to it, and the next MAIL FROM gets the 421 that was owed here.
    const user = await createUser(server, uniqueName("rot"));
    const smtp = await signedIn(user);
    expect((await smtp.send(`MAIL FROM:<x@${user.username}.mail.test>`)).raw).toBe("250 Accepted");
    await rotate(user);
    const lookupFailure = "451 4.3.0 Error: temporary lookup failure";
    expect((await smtp.send("RCPT TO:<someone@remote.example>")).raw).toBe(lookupFailure);
    expect((await smtp.send("RCPT TO:<hello@mail.test>")).raw).toBe(lookupFailure);
    expect((await smtp.send("DATA")).raw).toBe("503 Error: need RCPT command");
    expect((await smtp.send("RSET")).raw).toBe("250 Flushed");
    expect((await smtp.send(`MAIL FROM:<x@${user.username}.mail.test>`)).raw).toBe(CHANGED);
    expect(await smtp.waitForClose(5000)).toBe(true);
  });

  test("DATA is answered 421 when the change lands after RCPT TO, and nothing is sent", async () => {
    const user = await createUser(server, uniqueName("rot"));
    const smtp = await signedIn(user);
    await smtp.send(`MAIL FROM:<x@${user.username}.mail.test>`);
    await smtp.send("RCPT TO:<someone@remote.example>");
    await rotate(user);
    const sent = server.mailgun.requests.length;
    expect((await smtp.data("Subject: too late\r\n\r\nbody\r\n")).raw).toBe(CHANGED);
    expect(await smtp.waitForClose(5000)).toBe(true);
    expect(server.mailgun.requests.length).toBe(sent);
  });

  test("MAIL FROM is answered 451 while the account cannot be read", async () => {
    const user = await createUser(server, uniqueName("rot"));
    const smtp = await signedIn(user);
    await withDatabaseOffline(server, async () => {
      expect((await smtp.send(`MAIL FROM:<x@${user.username}.mail.test>`)).raw).toBe("451 4.3.0 Error: temporary lookup failure");
      // Transient: the connection stays open.
      expect((await smtp.send("NOOP")).raw).toBe("250 OK");
    });
    expect((await smtp.send(`MAIL FROM:<x@${user.username}.mail.test>`)).raw).toBe("250 Accepted");
  }, 60_000);

  test("a new sign-in with the same password works after the change", async () => {
    const user = await createUser(server, uniqueName("rot"));
    await rotate(user);
    const smtp = await signedIn(user);
    expect((await smtp.send(`MAIL FROM:<x@${user.username}.mail.test>`)).raw).toBe("250 Accepted");
  });

  test("resetting the password over HTTP closes the account's open sessions with 421", async () => {
    const user = await createUser(server, uniqueName("reset"));
    const bystander = await createUser(server, uniqueName("other"));
    const smtp = await signedIn(user);
    const other = await signedIn(bystander);

    await server.db.query("UPDATE users SET token = $2, expiry = NOW() + INTERVAL '1 hour' WHERE username = $1", [
      user.username,
      "reset-token-e2e",
    ]);
    const reset = await server.client().post("/api/users/set-info", {
      email: user.email,
      username: user.username,
      password: "a-new-password-e2e",
      token: "reset-token-e2e",
    });
    expect(reset.body.status).toBe("success");

    // Unasked for: the server speaks first, then hangs up.
    expect((await smtp.read()).raw).toBe("421 Credential changed, please re-authenticate");
    expect(await smtp.waitForClose(5000)).toBe(true);
    expect((await other.send("NOOP")).raw).toBe("250 OK");

    const again = await connect("smtps");
    expect((await again.authPlain(user.username, user.password)).raw).toBe(INVALID);
    expect((await again.authPlain(user.username, "a-new-password-e2e")).raw).toBe(SUCCESS);
  });
});
