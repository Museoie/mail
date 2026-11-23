/**
 * App passwords: with single sign-on on, IMAP and SMTP check the password a
 * mail app presents with the provider. Driven over raw sockets, since the
 * exact reply a mail app gets is what tells "wrong password" from "try again
 * later".
 *
 * The mail servers count failed sign-ins per client address, and every test
 * here connects from 127.0.0.1: each server below sees fewer than ten
 * rejections before it is restarted or stopped.
 */
import { afterAll, beforeAll, beforeEach, describe, expect, test } from "bun:test";
import net from "net";
import tls from "tls";
import { ADMIN_PASSWORD, ReplyReader, createUser, startServer, type TestServer } from "../harness";
import { formEncode, jsonReply, person, signIn, startIssuer, type FakeIssuer, type Identity } from "../harness/issuer";

const IMAP_CAPABILITIES = "IMAP4rev1 LITERAL+ SASL-IR LOGIN-REFERRALS ID ENABLE IDLE MOVE CONDSTORE AUTH=PLAIN";
const IMAP_REJECTED = "NO [AUTHENTICATIONFAILED] Invalid credentials.";
const IMAP_UNAVAILABLE = "NO [UNAVAILABLE] Authentication is temporarily unavailable";
const IMAP_NEEDS_TLS = "NO [PRIVACYREQUIRED] Use STARTTLS or the TLS port to sign in";
const SMTP_ACCEPTED = "235 Authentication successful";
const SMTP_REJECTED = "535 Error: Authentication credentials invalid";
const SMTP_UNAVAILABLE = "454 4.7.0 Temporary authentication failure";
const SMTP_NEEDS_TLS = "538 Error: Must issue a STARTTLS command first";
const INTROSPECT = "/oauth/introspect";

const alice = person("alice");

const connect = (port: number, secure: boolean): net.Socket =>
  secure
    ? tls.connect({ port, host: "127.0.0.1", rejectUnauthorized: false })
    : net.createConnection({ port, host: "127.0.0.1" });

/** A line-at-a-time IMAP conversation. */
class ImapConnection {
  private buffer = "";
  private lines: string[] = [];
  private waiters: ((line: string | undefined) => void)[] = [];
  private closed = false;
  private tag = 0;
  greeting = "";

  private constructor(private readonly socket: net.Socket) {
    socket.on("data", (chunk) => {
      this.buffer += chunk.toString("latin1");
      for (let at = this.buffer.indexOf("\r\n"); at !== -1; at = this.buffer.indexOf("\r\n")) {
        const line = this.buffer.slice(0, at);
        this.buffer = this.buffer.slice(at + 2);
        const waiter = this.waiters.shift();
        if (waiter) waiter(line);
        else this.lines.push(line);
      }
    });
    const end = () => {
      this.closed = true;
      for (const waiter of this.waiters.splice(0)) waiter(undefined);
    };
    socket.on("close", end);
    socket.on("error", end);
  }

  static async open(server: TestServer, secure = true): Promise<ImapConnection> {
    const connection = new ImapConnection(connect(secure ? server.ports.imaps : server.ports.imap, secure));
    connection.greeting = (await connection.line()) ?? "";
    return connection;
  }

  /** The next line, or undefined once the server has hung up. */
  line(timeoutMs = 15_000): Promise<string | undefined> {
    const queued = this.lines.shift();
    if (queued !== undefined) return Promise.resolve(queued);
    if (this.closed) return Promise.resolve(undefined);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("Timed out waiting for an IMAP line")), timeoutMs);
      this.waiters.push((line) => {
        clearTimeout(timer);
        resolve(line);
      });
    });
  }

  /** Sends one command and returns its tagged reply without the tag. */
  async command(text: string): Promise<string> {
    const tag = `t${++this.tag}`;
    this.socket.write(`${tag} ${text}\r\n`);
    for (;;) {
      const line = await this.line();
      if (line === undefined) return "(connection closed)";
      if (line.startsWith(`${tag} `)) return line.slice(tag.length + 1);
    }
  }

  login(username: string, password: string) {
    return this.command(`LOGIN ${username} ${password}`);
  }

  close() {
    this.socket.destroy();
  }
}

/** One IMAP sign-in on a fresh TLS connection: the tagged reply to LOGIN. */
const imapLogin = async (server: TestServer, username: string, password: string) => {
  const connection = await ImapConnection.open(server);
  try {
    return await connection.login(username, password);
  } finally {
    connection.close();
  }
};

class SmtpConnection {
  private readonly reader: ReplyReader;

  private constructor(private readonly socket: net.Socket) {
    this.reader = new ReplyReader(socket);
  }

  static async open(server: TestServer, port: number, secure: boolean): Promise<SmtpConnection> {
    const connection = new SmtpConnection(connect(port, secure));
    await connection.reader.next();
    await connection.send("EHLO client.remote.example");
    return connection;
  }

  /** Sends one line and returns the reply, or undefined once the server has hung up. */
  async send(line: string): Promise<string | undefined> {
    this.socket.write(`${line}\r\n`);
    return this.reader.next();
  }

  next() {
    return this.reader.next();
  }

  authPlain(username: string, password: string) {
    return this.send(`AUTH PLAIN ${Buffer.from(`\0${username}\0${password}`).toString("base64")}`);
  }

  close() {
    this.socket.destroy();
  }
}

/** One SMTP sign-in on a fresh implicit-TLS connection: the reply to AUTH PLAIN. */
const smtpAuth = async (server: TestServer, username: string, password: string) => {
  const connection = await SmtpConnection.open(server, server.ports.smtps, true);
  try {
    return await connection.authPlain(username, password);
  } finally {
    connection.close();
  }
};

const link = async (server: TestServer, issuer: FakeIssuer, who: Identity) => {
  const { done } = await signIn(server.client(), issuer, { who });
  expect(done.headers.get("location")).toBe("/");
};

/**
 * An account that changed in the last ten minutes has its app passwords
 * asked for afresh on every sign-in. This makes every account look settled.
 */
const settleAccounts = (server: TestServer) =>
  server.db.query("UPDATE users SET updated = now() - interval '1 hour'");

describe("app passwords, with password sign-in off", () => {
  let issuer: FakeIssuer;
  let server: TestServer;

  beforeAll(async () => {
    issuer = await startIssuer();
    server = await startServer({ env: issuer.env(), tls: true });
    await link(server, issuer, alice);
    await link(server, issuer, person("bob"));
  }, 90_000);
  afterAll(async () => {
    await server?.stop();
    await issuer?.stop();
  });

  beforeEach(async () => {
    issuer.resetReplies();
    issuer.up();
    issuer.appPasswords.clear();
    await settleAccounts(server);
    issuer.clearCalls();
  });

  describe("on IMAP", () => {
    test("LOGIN with an app password of the linked account is accepted", async () => {
      const { secret } = issuer.appPassword(alice);
      const connection = await ImapConnection.open(server);
      try {
        expect(connection.greeting).toBe(`* OK [CAPABILITY ${IMAP_CAPABILITIES}] IMAP4rev1 Service Ready`);
        expect(await connection.login("alice", secret)).toBe(`OK [CAPABILITY ${IMAP_CAPABILITIES}] LOGIN completed`);
        // It is alice's mailbox that opens.
        expect(await connection.command("SELECT INBOX")).toMatch(/^OK \[READ-WRITE\]/);
      } finally {
        connection.close();
      }

      const calls = issuer.seen(INTROSPECT);
      expect(calls).toHaveLength(1);
      expect(calls[0]!.method).toBe("POST");
      expect([...calls[0]!.form]).toEqual([["token", secret]]);
      expect(calls[0]!.headers.get("x-real-ip")).toBe("127.0.0.1");
      const credentials = `${formEncode(issuer.clientId)}:${formEncode(issuer.clientSecret)}`;
      expect(calls[0]!.headers.get("authorization")).toBe(`Basic ${Buffer.from(credentials).toString("base64")}`);
      expect(calls[0]!.headers.get("content-type")).toBe("application/x-www-form-urlencoded");
      expect(calls[0]!.headers.get("accept")).toBe("application/json");
    });

    test("AUTHENTICATE PLAIN with an app password is accepted", async () => {
      const { secret } = issuer.appPassword(alice);
      const connection = await ImapConnection.open(server);
      try {
        const reply = await connection.command(
          `AUTHENTICATE PLAIN ${Buffer.from(`\0alice\0${secret}`).toString("base64")}`
        );
        expect(reply).toBe(`OK [CAPABILITY ${IMAP_CAPABILITIES}] AUTHENTICATE completed`);
      } finally {
        connection.close();
      }
    });

    test("an app password is rejected without the imap scope, under another username, or once inactive", async () => {
      const smtpOnly = issuer.appPassword(alice, { scope: "smtp" });
      expect(await imapLogin(server, "alice", smtpOnly.secret)).toBe(IMAP_REJECTED);

      const good = issuer.appPassword(alice);
      // bob is an account, but not the one this app password belongs to.
      expect(await imapLogin(server, "bob", good.secret)).toBe(IMAP_REJECTED);
      // The username is the account's, to the letter.
      expect(await imapLogin(server, "Alice", good.secret)).toBe(IMAP_REJECTED);

      const revoked = issuer.appPassword(alice, { active: false });
      expect(await imapLogin(server, "alice", revoked.secret)).toBe(IMAP_REJECTED);

      // Someone the provider knows but who never signed in here has no account to open.
      const stranger = issuer.appPassword(person("stranger"));
      expect(await imapLogin(server, "stranger", stranger.secret)).toBe(IMAP_REJECTED);

      // After all that, the right one still works.
      expect(await imapLogin(server, "alice", good.secret)).toMatch(/^OK /);
    }, 60_000);

    test("a local password is refused, and is put to the provider like any other", async () => {
      expect(await imapLogin(server, "admin", ADMIN_PASSWORD)).toBe(IMAP_REJECTED);
      expect(issuer.seen(INTROSPECT).map((call) => call.form.get("token"))).toEqual([ADMIN_PASSWORD]);
    });

    test("a provider that cannot answer gets NO [UNAVAILABLE], and the connection stays usable", async () => {
      const { secret } = issuer.appPassword(alice);
      const connection = await ImapConnection.open(server);
      try {
        await issuer.down();
        expect(await connection.login("alice", secret)).toBe(IMAP_UNAVAILABLE);
        issuer.up();

        for (const answer of [
          () => jsonReply(500, { error: "server_error" }),
          () => new Response("OK", { status: 200 }),
          // The provider refusing this server's own credentials says nothing about the app password.
          () => jsonReply(401, { error: "invalid_client" }),
          // An active answer that does not say whose it is cannot be used.
          () => jsonReply(200, { active: true, username: "alice", role: "user", scope: "imap" }),
        ]) {
          issuer.replyOnce(INTROSPECT, answer);
          expect(await connection.login("alice", secret)).toBe(IMAP_UNAVAILABLE);
        }
        expect(await connection.login("alice", secret)).toMatch(/^OK /);
      } finally {
        issuer.up();
        connection.close();
      }
      // None of that counted as a failed attempt: plenty more are still allowed.
    }, 60_000);

    test("a provider that is rate limiting the check reads as a rejection", async () => {
      const { secret } = issuer.appPassword(alice);
      issuer.replyOnce(INTROSPECT, () => jsonReply(429, { error: "slow_down" }));
      expect(await imapLogin(server, "alice", secret)).toBe(IMAP_REJECTED);
    });

    test("an accepted app password is not asked about again for a minute, even with the provider down", async () => {
      const { secret } = issuer.appPassword(alice);
      expect(await imapLogin(server, "alice", secret)).toMatch(/^OK /);
      expect(issuer.seen(INTROSPECT)).toHaveLength(1);

      expect(await imapLogin(server, "alice", secret)).toMatch(/^OK /);
      expect(issuer.seen(INTROSPECT)).toHaveLength(1);

      await issuer.down();
      try {
        expect(await imapLogin(server, "alice", secret)).toMatch(/^OK /);
        // The answer is kept by password: another one is not vouched for by it.
        expect(await imapLogin(server, "alice", `${secret}x`)).toBe(IMAP_UNAVAILABLE);
      } finally {
        issuer.up();
      }
    });

    test("an account changed in the last ten minutes has its app password asked for afresh", async () => {
      const known = issuer.appPassword(alice);
      expect(await imapLogin(server, "alice", known.secret)).toMatch(/^OK /);
      expect(issuer.seen(INTROSPECT)).toHaveLength(1);

      // Something changed the account (a revocation another copy of the server handled, say).
      await server.db.query("UPDATE users SET updated = now() WHERE username = 'alice'");
      known.active = false;
      expect(await imapLogin(server, "alice", known.secret)).toBe(IMAP_REJECTED);
      expect(issuer.seen(INTROSPECT)).toHaveLength(2);

      // And with nobody to ask, the remembered answer is not good enough.
      known.active = true;
      expect(await imapLogin(server, "alice", known.secret)).toMatch(/^OK /);
      await issuer.down();
      try {
        expect(await imapLogin(server, "alice", known.secret)).toBe(IMAP_UNAVAILABLE);
      } finally {
        issuer.up();
      }
    }, 60_000);

    test("no password is taken before TLS", async () => {
      const { secret } = issuer.appPassword(alice);
      const connection = await ImapConnection.open(server, false);
      try {
        expect(connection.greeting).toBe(
          "* OK [CAPABILITY IMAP4rev1 LITERAL+ SASL-IR LOGIN-REFERRALS ID ENABLE IDLE MOVE CONDSTORE LOGINDISABLED STARTTLS] IMAP4rev1 Service Ready"
        );
        expect(await connection.login("alice", secret)).toBe(IMAP_NEEDS_TLS);
        expect(
          await connection.command(`AUTHENTICATE PLAIN ${Buffer.from(`\0alice\0${secret}`).toString("base64")}`)
        ).toBe(IMAP_NEEDS_TLS);
      } finally {
        connection.close();
      }
      expect(issuer.calls).toEqual([]);
    });

    test("a provider event that revokes the account closes its open connections", async () => {
      const { secret } = issuer.appPassword(alice);
      const mine = await ImapConnection.open(server);
      const other = await ImapConnection.open(server);
      try {
        expect(await mine.login("alice", secret)).toMatch(/^OK /);
        expect(await other.login("bob", issuer.appPassword(person("bob")).secret)).toMatch(/^OK /);

        const event = issuer.eventToken({ "credential.revoked": { sub: "usr_alice", id: "apw_1" } });
        const posted = await server.client().request("POST", "/api/auth/events", { body: event });
        expect(posted.status).toBe(202);

        expect(await mine.line()).toBe("* BYE Credential changed, please re-authenticate");
        expect(await mine.line()).toBeUndefined();
        // bob's connection is not alice's.
        expect(await other.command("NOOP")).toMatch(/^OK /);
      } finally {
        mine.close();
        other.close();
      }
    });
  });

  describe("on SMTP", () => {
    // A fresh process: the rejections above are not held against this address.
    beforeAll(() => server.restart(), 90_000);

    test("AUTH PLAIN with an app password of the linked account is accepted", async () => {
      const { secret } = issuer.appPassword(alice);
      expect(await smtpAuth(server, "alice", secret)).toBe(SMTP_ACCEPTED);

      const calls = issuer.seen(INTROSPECT);
      expect(calls).toHaveLength(1);
      expect([...calls[0]!.form]).toEqual([["token", secret]]);
      expect(calls[0]!.headers.get("x-real-ip")).toBe("127.0.0.1");
    });

    test("AUTH LOGIN with an app password is accepted", async () => {
      const { secret } = issuer.appPassword(alice);
      const connection = await SmtpConnection.open(server, server.ports.smtps, true);
      try {
        expect(await connection.send("AUTH LOGIN")).toBe("334 VXNlcm5hbWU6");
        expect(await connection.send(Buffer.from("alice").toString("base64"))).toBe("334 UGFzc3dvcmQ6");
        expect(await connection.send(Buffer.from(secret).toString("base64"))).toBe(SMTP_ACCEPTED);
        // And the connection may send as alice.
        expect(await connection.send("MAIL FROM:<alice@alice.mail.test>")).toBe("250 Accepted");
      } finally {
        connection.close();
      }
    });

    test("an app password is rejected without the smtp scope, under another username, or once inactive", async () => {
      const imapOnly = issuer.appPassword(alice, { scope: "imap" });
      expect(await smtpAuth(server, "alice", imapOnly.secret)).toBe(SMTP_REJECTED);

      const good = issuer.appPassword(alice);
      expect(await smtpAuth(server, "bob", good.secret)).toBe(SMTP_REJECTED);

      const revoked = issuer.appPassword(alice, { active: false });
      expect(await smtpAuth(server, "alice", revoked.secret)).toBe(SMTP_REJECTED);

      expect(await smtpAuth(server, "alice", good.secret)).toBe(SMTP_ACCEPTED);
    }, 60_000);

    test("a local password is refused, and is put to the provider like any other", async () => {
      expect(await smtpAuth(server, "admin", ADMIN_PASSWORD)).toBe(SMTP_REJECTED);
      expect(issuer.seen(INTROSPECT).map((call) => call.form.get("token"))).toEqual([ADMIN_PASSWORD]);
    });

    test("a provider that cannot answer gets 454, and the connection stays usable", async () => {
      const { secret } = issuer.appPassword(alice);
      const connection = await SmtpConnection.open(server, server.ports.smtps, true);
      try {
        await issuer.down();
        expect(await connection.authPlain("alice", secret)).toBe(SMTP_UNAVAILABLE);
        issuer.up();
        issuer.replyOnce(INTROSPECT, () => jsonReply(503, {}));
        expect(await connection.authPlain("alice", secret)).toBe(SMTP_UNAVAILABLE);
        expect(await connection.authPlain("alice", secret)).toBe(SMTP_ACCEPTED);
      } finally {
        issuer.up();
        connection.close();
      }
    });

    test("the submission port takes no password before STARTTLS", async () => {
      const { secret } = issuer.appPassword(alice);
      const connection = await SmtpConnection.open(server, server.ports.submission, false);
      try {
        expect(await connection.authPlain("alice", secret)).toBe(SMTP_NEEDS_TLS);
        expect(await connection.send("AUTH LOGIN")).toBe(SMTP_NEEDS_TLS);
      } finally {
        connection.close();
      }
      expect(issuer.calls).toEqual([]);
    });

    test("a provider event that revokes the account closes its open connections", async () => {
      const { secret } = issuer.appPassword(alice);
      const mine = await SmtpConnection.open(server, server.ports.smtps, true);
      const other = await SmtpConnection.open(server, server.ports.smtps, true);
      try {
        expect(await mine.authPlain("alice", secret)).toBe(SMTP_ACCEPTED);
        expect(await other.authPlain("bob", issuer.appPassword(person("bob")).secret)).toBe(SMTP_ACCEPTED);

        const event = issuer.eventToken({ "user.disabled": { sub: "usr_alice" } });
        const posted = await server.client().request("POST", "/api/auth/events", { body: event });
        expect(posted.status).toBe(202);

        expect(await mine.next()).toBe("421 Credential changed, please re-authenticate");
        expect(await mine.next()).toBeUndefined();
        expect(await other.send("NOOP")).toMatch(/^250 /);
      } finally {
        mine.close();
        other.close();
      }
    });
  });
});

describe("app passwords next to local passwords (AUTH_LOCAL_LOGIN=true)", () => {
  let issuer: FakeIssuer;
  let server: TestServer;

  beforeAll(async () => {
    issuer = await startIssuer();
    server = await startServer({ env: issuer.env({ AUTH_LOCAL_LOGIN: "true" }), tls: true });
    // carol signed up here with a password, and is linked by her address at her first single sign-on.
    await createUser(server, "carol", { email: "carol@people.example" });
    await link(server, issuer, person("carol"));
    await settleAccounts(server);
  }, 90_000);
  afterAll(async () => {
    await server?.stop();
    await issuer?.stop();
  });

  beforeEach(() => {
    issuer.appPasswords.clear();
    issuer.clearCalls();
  });

  test("the local password is tried first and never sent to the provider", async () => {
    expect(await imapLogin(server, "carol", "password-of-carol")).toBe(
      `OK [CAPABILITY ${IMAP_CAPABILITIES}] LOGIN completed`
    );
    expect(await smtpAuth(server, "carol", "password-of-carol")).toBe(SMTP_ACCEPTED);
    expect(await imapLogin(server, "admin", ADMIN_PASSWORD)).toMatch(/^OK /);
    expect(issuer.calls).toEqual([]);

    // Even with the provider down.
    await issuer.down();
    try {
      expect(await imapLogin(server, "carol", "password-of-carol")).toMatch(/^OK /);
      expect(await smtpAuth(server, "carol", "password-of-carol")).toBe(SMTP_ACCEPTED);
    } finally {
      issuer.up();
    }
  });

  test("a password that is not the local one is checked as an app password", async () => {
    const { secret } = issuer.appPassword(person("carol"));
    expect(await imapLogin(server, "carol", secret)).toMatch(/^OK /);
    expect(await smtpAuth(server, "carol", secret)).toBe(SMTP_ACCEPTED);

    expect(await imapLogin(server, "carol", "neither-of-them")).toBe(IMAP_REJECTED);
    expect(await smtpAuth(server, "carol", "nor-this-one")).toBe(SMTP_REJECTED);
    expect(issuer.seen(INTROSPECT).map((call) => call.form.get("token"))).toEqual([
      secret,
      "neither-of-them",
      "nor-this-one",
    ]);
  });

  test("with the provider down, a wrong local password is not called wrong", async () => {
    await issuer.down();
    try {
      expect(await imapLogin(server, "carol", "not-her-password")).toBe(IMAP_UNAVAILABLE);
      expect(await smtpAuth(server, "carol", "not-her-password")).toBe(SMTP_UNAVAILABLE);
    } finally {
      issuer.up();
    }
  });

  test("no password of either kind is taken before TLS", async () => {
    const imap = await ImapConnection.open(server, false);
    const submission = await SmtpConnection.open(server, server.ports.submission, false);
    try {
      expect(imap.greeting).toContain("LOGINDISABLED");
      expect(await imap.login("carol", "password-of-carol")).toBe(IMAP_NEEDS_TLS);
      expect(await submission.authPlain("carol", "password-of-carol")).toBe(SMTP_NEEDS_TLS);
    } finally {
      imap.close();
      submission.close();
    }
  });
});

describe("mail sign-in with single sign-on off", () => {
  let issuer: FakeIssuer;
  let server: TestServer;

  beforeAll(async () => {
    issuer = await startIssuer();
    server = await startServer({ tls: true });
  }, 90_000);
  afterAll(async () => {
    await server?.stop();
    await issuer?.stop();
  });

  test("takes local passwords only, also before TLS where it always did", async () => {
    const imap = await ImapConnection.open(server, false);
    const submission = await SmtpConnection.open(server, server.ports.submission, false);
    try {
      expect(imap.greeting).toBe(`* OK [CAPABILITY ${IMAP_CAPABILITIES} STARTTLS] IMAP4rev1 Service Ready`);
      expect(await imap.login("admin", ADMIN_PASSWORD)).toBe(
        `OK [CAPABILITY ${IMAP_CAPABILITIES} STARTTLS] LOGIN completed`
      );
      expect(await submission.authPlain("admin", ADMIN_PASSWORD)).toBe(SMTP_ACCEPTED);
    } finally {
      imap.close();
      submission.close();
    }
    expect(await imapLogin(server, "admin", "an-app-password")).toBe(IMAP_REJECTED);
    expect(await smtpAuth(server, "admin", "an-app-password")).toBe(SMTP_REJECTED);
    expect(issuer.calls).toEqual([]);
  });
});
