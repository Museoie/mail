import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, test } from "bun:test";
import { ADMIN_PASSWORD, createUser, type TestServer, type TestUser } from "../harness";
import { proxyLine } from "../harness/smtp";
import { bootAll, closeClients, crlf, ehloReply, open, uniqueName, type Listener } from "./support";

// SMTP AUTH while single sign-on is on: the password a client presents is an
// app password, which the identity provider is asked about (token
// introspection). The provider here is a stub the test controls.
const CLIENT_ID = "mail";
const CLIENT_SECRET = "test_client_secret_not_real";

interface IntrospectionCall {
  token: string | null;
  authorization: string | null;
  realIp: string | null;
}

const startProvider = () => {
  const calls: IntrospectionCall[] = [];
  const answers = new Map<string, () => Response>();
  let issuer = "";
  const server = Bun.serve({
    port: 0,
    hostname: "127.0.0.1",
    fetch: async (req): Promise<Response> => {
      const { pathname } = new URL(req.url);
      if (pathname === "/.well-known/openid-configuration") {
        return Response.json({
          issuer,
          authorization_endpoint: `${issuer}/authorize`,
          token_endpoint: `${issuer}/token`,
          jwks_uri: `${issuer}/jwks`,
          introspection_endpoint: `${issuer}/introspect`,
        });
      }
      if (pathname === "/introspect" && req.method === "POST") {
        const token = new URLSearchParams(await req.text()).get("token");
        calls.push({ token, authorization: req.headers.get("authorization"), realIp: req.headers.get("x-real-ip") });
        return (answers.get(token ?? "") ?? (() => Response.json({ active: false })))();
      }
      return new Response("not found", { status: 404 });
    },
  });
  issuer = `http://127.0.0.1:${server.port}`;
  return {
    issuer,
    calls,
    /** Makes `token` an active app password of the provider account `sub`. */
    grant: (token: string, sub: string, scope = "imap smtp", username = sub.replace(/^usr_/, "")) =>
      answers.set(token, () => Response.json({ active: true, sub, username, role: "user", scope, jti: `cred_${token}` })),
    /** Answers `token` with whatever `reply` returns. */
    answer: (token: string, reply: () => Response) => answers.set(token, reply),
    callsFor: (token: string) => calls.filter((call) => call.token === token),
    stop: () => void server.stop(true),
  };
};

let provider: ReturnType<typeof startProvider>;
let ssoOnly: TestServer;
let withLocal: TestServer;

beforeAll(async () => {
  provider = startProvider();
  const env = { PROXY_PROTOCOL: "true", AUTH_ISSUER: provider.issuer, AUTH_CLIENT_ID: CLIENT_ID, AUTH_CLIENT_SECRET: CLIENT_SECRET };
  [ssoOnly, withLocal] = (await bootAll({ tls: true, env }, { tls: true, env: { ...env, AUTH_LOCAL_LOGIN: "true" } })) as [TestServer, TestServer];
}, 180_000);

beforeEach(() => {
  provider.calls.length = 0;
});

afterEach(closeClients);

afterAll(async () => {
  await Promise.all([ssoOnly?.stop(), withLocal?.stop()]);
  provider?.stop();
});

let nextAddress = 0;
const freshAddress = () => `10.66.0.${++nextAddress}`;

const connect = async (server: TestServer, address = freshAddress(), listener: Listener = "smtps") => {
  const smtp = await open(server, listener, { proxy: proxyLine(address) });
  await smtp.ehlo();
  return smtp;
};

/**
 * An account linked to the provider subject `usr_<name>`, last changed long
 * enough ago that one introspection settles its app passwords.
 */
const linkedUser = async (server: TestServer): Promise<TestUser & { sub: string }> => {
  const user = await createUser(server, uniqueName("sso"));
  const sub = `usr_${user.username}`;
  await server.db.query("UPDATE users SET auth_sub = $2, updated = NOW() - INTERVAL '1 hour' WHERE username = $1", [user.username, sub]);
  return { ...user, sub };
};

const SUCCESS = "235 Authentication successful";
const INVALID = "535 Error: Authentication credentials invalid";
const UNAVAILABLE = "454 4.7.0 Temporary authentication failure";

describe("single sign-on without local passwords", () => {
  test("the submission port no longer takes AUTH in cleartext", async () => {
    const address = freshAddress();
    const smtp = await connect(ssoOnly, address, "submission");
    expect(smtp.transcript[1]!.raw).toBe(ehloReply({ address, starttls: true }));
    expect((await smtp.send("AUTH PLAIN")).raw).toBe("538 Error: Must issue a STARTTLS command first");
    await smtp.startTls();
    await smtp.ehlo();
    expect((await smtp.send("AUTH PLAIN")).raw).toBe("334 ");
  });

  test("an app password the provider vouches for signs the linked account in", async () => {
    const user = await linkedUser(ssoOnly);
    provider.grant("app-password-1", user.sub);
    const address = freshAddress();
    const smtp = await connect(ssoOnly, address);
    expect((await smtp.authPlain(user.username, "app-password-1")).raw).toBe(SUCCESS);

    // The provider was asked once, as this client, about that secret, and told who is asking.
    expect(provider.callsFor("app-password-1")).toEqual([
      {
        token: "app-password-1",
        authorization: `Basic ${Buffer.from(`${CLIENT_ID}:${CLIENT_SECRET}`).toString("base64")}`,
        realIp: address,
      },
    ]);

    // The session can send.
    const result = await smtp.transaction({
      from: `me@${user.username}.mail.test`,
      to: ["bob@remote.example"],
      data: crlf("To: bob@remote.example", "Subject: via app password", "", "x"),
    });
    expect(result.final.raw).toBe("250 OK: message queued");
    expect(ssoOnly.mailgun.requests.map((request) => request.fields.subject)).toContainEqual(["via app password"]);
  });

  test("AUTH LOGIN works the same", async () => {
    const user = await linkedUser(ssoOnly);
    provider.grant("app-password-login", user.sub);
    const smtp = await connect(ssoOnly);
    expect((await smtp.authLogin(user.username, "app-password-login")).raw).toBe(SUCCESS);
  });

  test("an account changed in the last ten minutes has its app password asked about twice", async () => {
    const user = await linkedUser(ssoOnly);
    await ssoOnly.db.query("UPDATE users SET updated = NOW() WHERE username = $1", [user.username]);
    provider.grant("app-password-fresh", user.sub);
    const smtp = await connect(ssoOnly);
    expect((await smtp.authPlain(user.username, "app-password-fresh")).raw).toBe(SUCCESS);
    expect(provider.callsFor("app-password-fresh")).toHaveLength(2);
  });

  test("an answer is reused for a minute: a second sign-in does not ask again, even if the provider would now say no", async () => {
    const user = await linkedUser(ssoOnly);
    provider.grant("app-password-cached", user.sub);
    expect((await (await connect(ssoOnly)).authPlain(user.username, "app-password-cached")).raw).toBe(SUCCESS);
    provider.answer("app-password-cached", () => Response.json({ active: false }));
    expect((await (await connect(ssoOnly)).authPlain(user.username, "app-password-cached")).raw).toBe(SUCCESS);
    expect(provider.callsFor("app-password-cached")).toHaveLength(1);
  });

  test("what the provider does not vouch for is 535, half a second late", async () => {
    const user = await linkedUser(ssoOnly);
    const other = await linkedUser(ssoOnly);
    provider.grant("imap-only", user.sub, "imap");
    provider.grant("someone-elses", other.sub);
    provider.grant("unlinked", "usr_nobody_here");
    provider.answer("throttled", () => new Response(null, { status: 429 }));

    const smtp = await connect(ssoOnly);
    const started = Date.now();
    // Not an app password at all.
    expect((await smtp.authPlain(user.username, "never-issued")).raw).toBe(INVALID);
    expect(Date.now() - started).toBeGreaterThanOrEqual(450);
    // Valid, but not for SMTP.
    expect((await smtp.authPlain(user.username, "imap-only")).raw).toBe(INVALID);
    // Valid, but another account's.
    expect((await smtp.authPlain(user.username, "someone-elses")).raw).toBe(INVALID);
    // Valid, but of a provider account no local account is linked to.
    expect((await smtp.authPlain(user.username, "unlinked")).raw).toBe(INVALID);
    // The provider is rate limiting this server.
    expect((await smtp.authPlain(user.username, "throttled")).raw).toBe(INVALID);
  });

  test("local passwords no longer sign in, the admin's included", async () => {
    const user = await linkedUser(ssoOnly);
    const smtp = await connect(ssoOnly);
    expect((await smtp.authPlain(user.username, user.password)).raw).toBe(INVALID);
    expect((await smtp.authPlain("admin", ADMIN_PASSWORD)).raw).toBe(INVALID);
    // Each was passed to the provider as a would-be app password.
    expect(provider.calls.map((call) => call.token)).toEqual([user.password, ADMIN_PASSWORD]);
  });

  test("a provider that cannot answer is 454, at once, and is not held against the client", async () => {
    const user = await linkedUser(ssoOnly);
    provider.answer("while-down", () => new Response("boom", { status: 500 }));
    provider.answer("malformed", () => Response.json({ active: true, sub: user.sub }));
    const smtp = await connect(ssoOnly);
    const started = Date.now();
    for (let i = 0; i < 12; i++) expect((await smtp.authPlain(user.username, "while-down")).raw).toBe(UNAVAILABLE);
    expect((await smtp.authPlain(user.username, "malformed")).raw).toBe(UNAVAILABLE);
    expect(Date.now() - started).toBeLessThan(3000);
    // Twelve of those would have locked the address out had they counted.
    provider.grant("after-the-outage", user.sub);
    expect((await smtp.authPlain(user.username, "after-the-outage")).raw).toBe(SUCCESS);
  });
});

describe("single sign-on with local passwords kept (AUTH_LOCAL_LOGIN)", () => {
  test("the submission port still refuses AUTH in cleartext", async () => {
    const smtp = await connect(withLocal, freshAddress(), "submission");
    expect((await smtp.send("AUTH PLAIN")).raw).toBe("538 Error: Must issue a STARTTLS command first");
  });

  test("a local password signs in without the provider being asked", async () => {
    const user = await linkedUser(withLocal);
    const smtp = await connect(withLocal);
    expect((await smtp.authPlain(user.username, user.password)).raw).toBe(SUCCESS);
    expect((await (await connect(withLocal)).authPlain("admin", ADMIN_PASSWORD)).raw).toBe(SUCCESS);
    expect(provider.calls).toEqual([]);
  });

  test("an app password is tried when the local password does not match", async () => {
    const user = await linkedUser(withLocal);
    provider.grant("app-password-mixed", user.sub);
    const smtp = await connect(withLocal);
    expect((await smtp.authPlain(user.username, "app-password-mixed")).raw).toBe(SUCCESS);
    expect(provider.callsFor("app-password-mixed")).toHaveLength(1);
  });

  test("neither a local nor an app password: 535", async () => {
    const user = await linkedUser(withLocal);
    const smtp = await connect(withLocal);
    expect((await smtp.authPlain(user.username, "neither")).raw).toBe(INVALID);
    expect(provider.callsFor("neither")).toHaveLength(1);
  });
});
