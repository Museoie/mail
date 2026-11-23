import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import {
  ADMIN_PASSWORD,
  createUser,
  rawHttp,
  type ApiClient,
  type ApiResult,
  type TestServer,
  type TestUser,
} from "../harness";
import { startServer } from "./core-helpers";
import {
  ANONYMOUS_LOGIN_TEXT,
  INTERNAL_ERROR,
  INVALID_BODY,
  INVALID_CREDENTIALS,
  JSON_TYPE,
  NOT_FOUND,
  rawRequest,
  userRowByEmail,
  withDatabaseOffline,
} from "./core-helpers";

const LOGIN_LIMITED = { status: "failed", message: "Too many login attempts, try again later" };
const TOKEN_LIMITED = { status: "failed", message: "Too many token requests, try again later" };
const CLIENT_ERROR_LIMITED = { status: "failed", message: "Too many client error reports, try again later" };
const MAILGUN_LIMITED = { status: "failed", message: "Too many Mailgun webhook requests, try again later" };
const INVALID_EMAIL = { status: "failed", message: "Signup failed because email is invalid." };
const SUCCESS = { status: "success" };

/** Production posture: a client names its own address with X-Forwarded-For. */
let prod: TestServer;
let alice: TestUser;
let bob: TestUser;

beforeAll(async () => {
  prod = await startServer({ production: true });
  alice = await createUser(prod, "alice");
  bob = await createUser(prod, "bob");
}, 90_000);

afterAll(async () => {
  await prod?.stop();
});

const login = (client: ApiClient, username: string, password: string) =>
  client.post("/api/users/login", { username, password });

/** A refusal; a client that holds a session still gets its cookie renewed on one. */
const expectLimited = (result: ApiResult, body: unknown, { signedIn = false } = {}) => {
  expect(result.status).toBe(429);
  expect(result.body).toEqual(body);
  expect(result.headers.get("content-type")).toBe(JSON_TYPE);
  expect(result.setCookies).toHaveLength(signedIn ? 1 : 0);
};

describe("login: five failures per address in fifteen minutes", () => {
  test("the sixth attempt is refused, even with the right password", async () => {
    const client = prod.client();
    for (let i = 0; i < 5; i++) {
      const result = await login(client, "alice", "wrong");
      expect({ i, status: result.status, body: result.body }).toEqual({ i, status: 200, body: INVALID_CREDENTIALS });
    }
    const refused = await login(client, "alice", alice.password);
    expectLimited(refused, LOGIN_LIMITED);
    expect(refused.text).toBe('{"status":"failed","message":"Too many login attempts, try again later"}');
    // And it stays refused.
    expectLimited(await login(client, "alice", alice.password), LOGIN_LIMITED);
  });

  test("the failures add up across the accounts they name", async () => {
    const client = prod.client();
    const attempts: [string, string][] = [
      ["alice", "wrong"],
      ["bob", "wrong"],
      ["nobody-1", "x"],
      ["nobody-2", "x"],
      ["admin", "wrong"],
    ];
    for (const [username, password] of attempts) expect((await login(client, username, password)).status).toBe(200);
    // An account this address never tried is refused as well.
    expectLimited(await login(client, "admin", ADMIN_PASSWORD), LOGIN_LIMITED);
    expectLimited(await login(client, "carol-never-tried", "x"), LOGIN_LIMITED);
  });

  test("failures that name no account count too", async () => {
    const client = prod.client();
    for (let i = 0; i < 5; i++) {
      expect((await client.post("/api/users/login", { password: "no identity" })).body).toEqual(INVALID_CREDENTIALS);
    }
    expectLimited(await login(client, "alice", alice.password), LOGIN_LIMITED);
  });

  test("four failures leave room for one more attempt", async () => {
    const client = prod.client();
    for (let i = 0; i < 4; i++) await login(client, "alice", "wrong");
    const fifth = await login(client, "alice", alice.password);
    expect(fifth.body.status).toBe("success");
  });

  test("a success forgives nothing at the address: its failures keep counting", async () => {
    const client = prod.client();
    for (let i = 0; i < 4; i++) await login(client, "alice", "wrong");
    expect((await login(client, "alice", alice.password)).body.status).toBe("success");
    // One more failure, on another account, is the fifth at this address.
    expect((await login(client, "bob", "wrong")).body).toEqual(INVALID_CREDENTIALS);
    expectLimited(await login(client, "alice", alice.password), LOGIN_LIMITED, { signedIn: true });
    expectLimited(await login(client, "bob", bob.password), LOGIN_LIMITED, { signedIn: true });
    // The same account from an address with no failures is fine.
    expect((await login(prod.client(), "alice", alice.password)).body.status).toBe("success");
  });

  test("successes in between do not reset the count", async () => {
    const client = prod.client();
    await login(client, "alice", "wrong");
    await login(client, "alice", "wrong");
    expect((await login(client, "alice", alice.password)).body.status).toBe("success");
    await login(client, "alice", "wrong");
    expect((await login(client, "alice", alice.password)).body.status).toBe("success");
    await login(client, "alice", "wrong");
    await login(client, "alice", "wrong");
    expectLimited(await login(client, "alice", alice.password), LOGIN_LIMITED, { signedIn: true });
  });

  test("successful logins alone never run into the limit", async () => {
    const client = prod.client();
    for (let i = 0; i < 8; i++) expect((await login(client, "alice", alice.password)).body.status).toBe("success");
  });

  test("identities that differ in case or surrounding space are still five failures", async () => {
    const client = prod.client();
    for (const username of ["alice ", " Alice", "ALICE", "aLiCe  ", "\talice"]) {
      expect((await login(client, username, alice.password)).body).toEqual(INVALID_CREDENTIALS);
    }
    expectLimited(await login(client, "alice", alice.password), LOGIN_LIMITED);
  });

  test("an email identity, and a username given next to an email, count the same way", async () => {
    const client = prod.client();
    const bodies = [
      { email: alice.email, password: "wrong" },
      { email: alice.email.toUpperCase(), password: "wrong" },
      { username: "", email: alice.email, password: "wrong" },
      { username: "alice", email: bob.email, password: "wrong" },
      { username: "bob", email: alice.email, password: "wrong" },
    ];
    for (const body of bodies) expect((await client.post("/api/users/login", body)).body).toEqual(INVALID_CREDENTIALS);
    expectLimited(await client.post("/api/users/login", { email: alice.email, password: alice.password }), LOGIN_LIMITED);
  });

  test("a body refused for its shape does not count", async () => {
    const client = prod.client();
    const shapes: unknown[] = [
      [],
      {},
      { username: "alice" },
      { username: "alice", password: "" },
      { username: "alice", password: 5 },
      { username: 5, password: "x" },
      { email: null, password: "x" },
      { username: ["alice"], password: "x" },
    ];
    for (let round = 0; round < 2; round++) {
      for (const body of shapes) expect((await client.post("/api/users/login", body)).status).toBe(200);
    }
    expect((await login(client, "alice", alice.password)).body.status).toBe("success");
  });

  test("a malformed body does not count", async () => {
    const client = prod.client();
    for (let i = 0; i < 7; i++) {
      const result = await client.request("POST", "/api/users/login", {
        body: "{",
        headers: { "Content-Type": "application/json" },
      });
      expect(result.status).toBe(400);
    }
    expect((await login(client, "alice", alice.password)).body.status).toBe("success");
  });

  test("a 500 does not count", async () => {
    const client = prod.client();
    await withDatabaseOffline(prod, async () => {
      for (let i = 0; i < 7; i++) {
        const result = await login(client, "alice", "x");
        expect({ i, status: result.status, body: result.body }).toEqual({ i, status: 500, body: INTERNAL_ERROR });
      }
    });
    expect((await login(client, "alice", alice.password)).body.status).toBe("success");
  }, 60_000);

  test("an account with no password yet (a sign-up in progress) counts as a wrong password does", async () => {
    await prod.db.query(
      "INSERT INTO users (username, email, token) VALUES ('user_ratelimit', 'pending-rl@elsewhere.example', 't') ON CONFLICT DO NOTHING"
    );
    const client = prod.client();
    for (let i = 0; i < 5; i++) {
      expect((await login(client, "user_ratelimit", "x")).body).toEqual(INVALID_CREDENTIALS);
    }
    expectLimited(await login(client, "alice", alice.password), LOGIN_LIMITED);
  });

  test("once limited, the refusal comes before the body is looked at", async () => {
    const client = prod.client();
    for (let i = 0; i < 5; i++) await login(client, "alice", "wrong");
    for (const body of [[], {}, { username: 5, password: 5 }, { password: "x" }]) {
      expectLimited(await client.post("/api/users/login", body), LOGIN_LIMITED);
    }
    expectLimited(await client.request("POST", "/api/users/login"), LOGIN_LIMITED);
    // A body that cannot be parsed is still refused first, as a 400.
    const malformed = await client.request("POST", "/api/users/login", {
      body: "{",
      headers: { "Content-Type": "application/json" },
    });
    expect(malformed.status).toBe(400);
  });

  test("the limit covers POST below /login in any spelling, and nothing else", async () => {
    const client = prod.client();
    const notLimited = await client.post("/api/users/login/extra", {});
    expect(notLimited.status).toBe(404);
    for (let i = 0; i < 5; i++) await login(client, "alice", "wrong");

    for (const path of ["/api/users/login/", "/API/Users/LOGIN", "/api/users/login/extra", "/api/users/login/a/b"]) {
      expectLimited(await client.post(path, { username: "alice", password: alice.password }), LOGIN_LIMITED);
    }
    // Not under /login.
    expect((await client.post("/api/users/loginx", {})).status).toBe(404);
    // Other methods are never limited.
    const get = await client.get("/api/users/login");
    expect(get.status).toBe(200);
    expect(get.text).toBe(ANONYMOUS_LOGIN_TEXT);
    expect((await client.head("/api/users/login")).status).toBe(200);
    expect((await client.delete("/api/users/login")).body).toEqual(SUCCESS);
    expect((await client.request("PUT", "/api/users/login")).status).toBe(404);
    // The other user routes have their own limits, or none.
    expect((await client.post("/api/users/token", { email: 1 })).body).toEqual(INVALID_EMAIL);
    expect((await client.post("/api/users/set-info", {})).body.message).toBe("email is required and must be a string.");
    expect((await client.post("/api/client-error", {})).body).toEqual(SUCCESS);
  });

  test("every address has its own count", async () => {
    const noisy = prod.client();
    for (let i = 0; i < 5; i++) await login(noisy, "alice", "wrong");
    expectLimited(await login(noisy, "alice", alice.password), LOGIN_LIMITED);
    expect((await login(prod.client(), "alice", alice.password)).body.status).toBe("success");
  });

  test("a signed-in client that gets limited keeps its session", async () => {
    const client = prod.client();
    expect((await login(client, "alice", alice.password)).body.status).toBe("success");
    for (let i = 0; i < 5; i++) await login(client, "bob", "wrong");
    expectLimited(await login(client, "bob", bob.password), LOGIN_LIMITED, { signedIn: true });
    expect((await client.get("/api/users/login")).body.body.user.username).toBe("alice");
  });
});

describe("token: three requests per address in fifteen minutes", () => {
  const token = (client: ApiClient, email: unknown) => client.post("/api/users/token", { email });

  test("requests refused for their shape count", async () => {
    const client = prod.client();
    expect((await client.post("/api/users/token", [])).body).toEqual(INVALID_BODY);
    expect((await token(client, 5)).body).toEqual(INVALID_EMAIL);
    expect((await token(client, "not-an-address")).body).toEqual(INVALID_EMAIL);
    const refused = await token(client, "someone@elsewhere.example");
    expectLimited(refused, TOKEN_LIMITED);
    expect(refused.text).toBe('{"status":"failed","message":"Too many token requests, try again later"}');
    expect(await userRowByEmail(prod, "someone@elsewhere.example")).toBeUndefined();
  });

  test("the silent successes for admin's addresses count", async () => {
    const client = prod.client();
    expect((await token(client, "admin@mail.test")).body).toEqual(SUCCESS);
    expect((await token(client, "anything@mail.test")).body).toEqual(SUCCESS);
    expect((await token(client, "x@admin.mail.test")).body).toEqual(SUCCESS);
    expectLimited(await token(client, "anything@mail.test"), TOKEN_LIMITED);
  });

  test("the silent success for a reserved account at an outside address counts", async () => {
    await prod.db.query("UPDATE users SET email = 'owner@elsewhere.example' WHERE username = 'admin'");
    try {
      const client = prod.client();
      for (let i = 0; i < 3; i++) expect((await token(client, "owner@elsewhere.example")).body).toEqual(SUCCESS);
      expectLimited(await token(client, "owner@elsewhere.example"), TOKEN_LIMITED);
    } finally {
      await prod.db.query("UPDATE users SET email = 'admin@mail.test' WHERE username = 'admin'");
    }
  });

  test("two requests leave room for a third", async () => {
    const client = prod.client();
    await token(client, 1);
    await token(client, 2);
    expect((await token(client, "third@mail.test")).body).toEqual(SUCCESS);
    expectLimited(await token(client, "fourth@mail.test"), TOKEN_LIMITED);
  });

  test("mails that were sent count", async () => {
    const client = prod.client();
    const sent = prod.mailgun.requests.length;
    for (const email of ["rl-one@elsewhere.example", "rl-two@elsewhere.example", alice.email]) {
      expect((await token(client, email)).body).toEqual(SUCCESS);
    }
    expectLimited(await token(client, "rl-four@elsewhere.example"), TOKEN_LIMITED);
    expect(prod.mailgun.requests.length).toBe(sent + 3);
    expect(await userRowByEmail(prod, "rl-four@elsewhere.example")).toBeUndefined();
  });

  test("a mail that Mailgun refused does not count", async () => {
    const client = prod.client();
    for (const email of ["rl-refused-1@elsewhere.example", "rl-refused-2@elsewhere.example"]) {
      prod.mailgun.replyOnce(() => Response.json({ message: "Forbidden" }, { status: 401 }));
      const result = await token(client, email);
      expect(result.status).toBe(500);
    }
    for (const email of ["rl-ok-1@elsewhere.example", "rl-ok-2@elsewhere.example", "rl-ok-3@elsewhere.example"]) {
      expect((await token(client, email)).body).toEqual(SUCCESS);
    }
    expectLimited(await token(client, "rl-ok-4@elsewhere.example"), TOKEN_LIMITED);
  });

  test("asking again for an unfinished sign-up mails it again, and counts as any send does", async () => {
    await prod.db.query(
      "INSERT INTO users (username, email, token) VALUES ('user_tokenlimit', 'pending-token-rl@elsewhere.example', 't') ON CONFLICT DO NOTHING"
    );
    const client = prod.client();
    const sent = prod.mailgun.requests.length;
    for (let i = 0; i < 3; i++) {
      expect((await token(client, "pending-token-rl@elsewhere.example")).body).toEqual(SUCCESS);
    }
    expectLimited(await token(client, "pending-token-rl@elsewhere.example"), TOKEN_LIMITED);
    expect(prod.mailgun.requests.length).toBe(sent + 3);
  });

  test("a malformed body does not count", async () => {
    const client = prod.client();
    for (let i = 0; i < 5; i++) {
      const result = await client.request("POST", "/api/users/token", {
        body: "[1,",
        headers: { "Content-Type": "application/json" },
      });
      expect(result.status).toBe(400);
    }
    expect((await token(client, "anything@mail.test")).body).toEqual(SUCCESS);
  });

  test("the limit covers POST below /token in any spelling, and nothing else", async () => {
    const client = prod.client();
    expect((await client.post("/api/users/token/extra", {})).status).toBe(404);
    for (let i = 0; i < 3; i++) await token(client, i);
    for (const path of ["/api/users/token/", "/API/USERS/TOKEN", "/api/users/token/extra"]) {
      expectLimited(await client.post(path, { email: "anything@mail.test" }), TOKEN_LIMITED);
    }
    expect((await client.get("/api/users/token")).status).toBe(404);
    expect((await client.post("/api/users/tokens", {})).status).toBe(404);
    // Signing in is a different limit.
    expect((await login(client, "alice", alice.password)).body.status).toBe("success");
    expect((await client.post("/api/users/set-info", {})).body.message).toBe("email is required and must be a string.");
  });

  test("every address has its own count", async () => {
    const noisy = prod.client();
    for (let i = 0; i < 3; i++) await token(noisy, i);
    expectLimited(await token(noisy, 4), TOKEN_LIMITED);
    expect((await token(prod.client(), 4)).body).toEqual(INVALID_EMAIL);
  });
});

describe("client-error: five reports per address in fifteen minutes", () => {
  test("the sixth report is refused, whatever the reports carried", async () => {
    const client = prod.client();
    const bodies: unknown[] = [{ message: "one" }, {}, [], { message: 5 }, undefined];
    for (const body of bodies) {
      const result =
        body === undefined ? await client.request("POST", "/api/client-error") : await client.post("/api/client-error", body);
      expect({ status: result.status, body: result.body }).toEqual({ status: 200, body: SUCCESS });
    }
    const refused = await client.post("/api/client-error", { message: "six" });
    expectLimited(refused, CLIENT_ERROR_LIMITED);
    expect(refused.text).toBe('{"status":"failed","message":"Too many client error reports, try again later"}');
  });

  test("requests the route does not answer neither count nor get refused", async () => {
    const client = prod.client();
    for (let i = 0; i < 3; i++) {
      expect((await client.get("/api/client-error")).body).toEqual(NOT_FOUND);
      expect((await client.post("/api/client-error/extra", {})).body).toEqual(NOT_FOUND);
    }
    for (let i = 0; i < 5; i++) expect((await client.post("/api/client-error", {})).status).toBe(200);
    expectLimited(await client.post("/api/client-error/", {}), CLIENT_ERROR_LIMITED);
    expectLimited(await client.post("/API/Client-Error", {}), CLIENT_ERROR_LIMITED);
    expect((await client.get("/api/client-error")).status).toBe(404);
    expect((await client.post("/api/client-error/extra", {})).status).toBe(404);
    expect((await client.request("PUT", "/api/client-error")).status).toBe(404);
  });

  test("a malformed body does not count", async () => {
    const client = prod.client();
    for (let i = 0; i < 7; i++) {
      const result = await client.request("POST", "/api/client-error", {
        body: "nope",
        headers: { "Content-Type": "application/json" },
      });
      expect(result.status).toBe(400);
    }
    expect((await client.post("/api/client-error", {})).body).toEqual(SUCCESS);
  });

  test("every address has its own count", async () => {
    const noisy = prod.client();
    for (let i = 0; i < 5; i++) await noisy.post("/api/client-error", {});
    expect((await noisy.post("/api/client-error", {})).status).toBe(429);
    expect((await prod.client().post("/api/client-error", {})).status).toBe(200);
  });
});

describe("mailgun-events: sixty requests per address in fifteen minutes", () => {
  const NOT_CONFIGURED = { status: "failed", message: "Webhook not configured" };

  test("the sixty-first request is refused; unanswered requests do not count until then", async () => {
    const client = prod.client();
    // Not answered by the route, so not counted.
    for (let i = 0; i < 3; i++) {
      expect((await client.get("/api/mailgun-events")).body).toEqual(NOT_FOUND);
      expect((await client.post("/api/mailgun-events/extra", {})).body).toEqual(NOT_FOUND);
    }
    for (let i = 0; i < 60; i++) {
      const result = await client.post("/api/mailgun-events", i % 2 ? {} : []);
      expect({ i, status: result.status, body: result.body }).toEqual({ i, status: 200, body: NOT_CONFIGURED });
    }
    const refused = await client.post("/api/mailgun-events", {});
    expectLimited(refused, MAILGUN_LIMITED);
    expect(refused.text).toBe('{"status":"failed","message":"Too many Mailgun webhook requests, try again later"}');

    // Once over the limit, everything under the path is refused, whatever the method.
    for (const [method, path] of [
      ["GET", "/api/mailgun-events"],
      ["DELETE", "/api/mailgun-events"],
      ["POST", "/api/mailgun-events/"],
      ["POST", "/API/Mailgun-Events"],
      ["POST", "/api/mailgun-events/extra"],
      ["GET", "/api/mailgun-events/a/b"],
    ] as const) {
      const result = await client.request(method, path);
      expect({ method, path, status: result.status, body: result.body }).toEqual({
        method,
        path,
        status: 429,
        body: MAILGUN_LIMITED,
      });
    }
    expect((await client.head("/api/mailgun-events")).status).toBe(429);
    // Another path, and another address, are not.
    expect((await client.post("/api/mailgun-eventsx", {})).status).toBe(404);
    expect((await client.get("/api/ping")).status).toBe(200);
    expect((await prod.client().post("/api/mailgun-events", {})).body).toEqual(NOT_CONFIGURED);
  });

  test("a malformed body does not count", async () => {
    const client = prod.client();
    for (let i = 0; i < 62; i++) {
      const result = await client.request("POST", "/api/mailgun-events", {
        body: "{",
        headers: { "Content-Type": "application/json" },
      });
      expect(result.status).toBe(400);
    }
    expect((await client.post("/api/mailgun-events", {})).body).toEqual(NOT_CONFIGURED);
  });
});

describe("which address a request is counted under, in production", () => {
  // The client-error limit (five per address) is the probe: fill one address's
  // quota, then see which other X-Forwarded-For values land in the same bucket.
  const report = (forwardedFor: string | undefined) =>
    prod
      .client({ forwarded: false })
      .request("POST", "/api/client-error", {
        json: {},
        headers: forwardedFor === undefined ? {} : { "X-Forwarded-For": forwardedFor, "X-Forwarded-Proto": "https" },
      })
      .then((result) => result.status);

  const fill = async (forwardedFor: string | undefined) => {
    for (let i = 0; i < 5; i++) expect(await report(forwardedFor)).toBe(200);
    expect(await report(forwardedFor)).toBe(429);
  };

  test("the one forwarded address", async () => {
    await fill("203.0.113.10");
    expect(await report("203.0.113.10")).toBe(429);
    expect(await report("203.0.113.11")).toBe(200);
  });

  test("of several hops, the one nearest the server that is not a trusted proxy", async () => {
    await fill("203.0.113.20");
    expect(await report("198.51.100.200, 203.0.113.20")).toBe(429);
    expect(await report("1.2.3.4, 5.6.7.8, 203.0.113.20")).toBe(429);
    // The other way round it is another address.
    expect(await report("203.0.113.20, 198.51.100.201")).toBe(200);
  });

  test("hops at private and loopback addresses are skipped as proxies", async () => {
    await fill("203.0.113.30");
    for (const chain of [
      "203.0.113.30, 10.0.0.1",
      "203.0.113.30, 192.168.1.1",
      "203.0.113.30, 172.16.0.1",
      "203.0.113.30, 172.31.255.255",
      "203.0.113.30, 127.0.0.1",
      "203.0.113.30, 127.255.255.254",
      "203.0.113.30, 10.0.0.1, 192.168.1.1, 172.20.5.5, 127.0.0.1",
      "9.9.9.9, 203.0.113.30, 10.0.0.1",
    ]) {
      expect({ chain, status: await report(chain) }).toEqual({ chain, status: 429 });
    }
  });

  test("hops just outside the private ranges are not proxies", async () => {
    await fill("203.0.113.40");
    for (const hop of ["172.15.255.255", "172.32.0.1", "192.169.0.1", "11.0.0.1", "100.64.0.1", "169.254.1.1", "126.255.255.255", "128.0.0.1"]) {
      const chain = `203.0.113.40, ${hop}`;
      expect({ chain, status: await report(chain) }).toEqual({ chain, status: 200 });
    }
  });

  test("IPv6 loopback, unique-local and IPv4-mapped private hops are proxies too", async () => {
    await fill("203.0.113.50");
    for (const hop of ["::1", "fd00::1", "fc00::abcd", "fdff:ffff::1", "::ffff:10.0.0.1", "::ffff:192.168.0.9", "::ffff:127.0.0.1"]) {
      const chain = `203.0.113.50, ${hop}`;
      expect({ chain, status: await report(chain) }).toEqual({ chain, status: 429 });
    }
    for (const hop of ["2001:db8::1", "fe80::1", "::ffff:203.0.113.99", "fe00::1", "::2"]) {
      const chain = `203.0.113.50, ${hop}`;
      expect({ chain, status: await report(chain) }).toEqual({ chain, status: 200 });
    }
  });

  test("an IPv6 client address is a bucket of its own", async () => {
    await fill("2001:db8::50");
    expect(await report("2001:db8::50")).toBe(429);
    expect(await report("9.9.9.9, 2001:db8::50, 10.0.0.1")).toBe(429);
    expect(await report("2001:db8::51")).toBe(200);
  });

  test("a client cannot rotate out of its bucket by putting addresses in front of its own", async () => {
    for (let i = 0; i < 5; i++) expect(await report(`198.18.0.${i + 1}, 203.0.113.60`)).toBe(200);
    expect(await report("198.18.0.99, 203.0.113.60")).toBe(429);
    expect(await report("10.0.0.1, 203.0.113.60")).toBe(429);
    expect(await report("203.0.113.60")).toBe(429);
  });

  test("when every hop is a trusted proxy, the farthest one is the client", async () => {
    await fill("10.9.8.7");
    expect(await report("10.9.8.7, 10.0.0.1")).toBe(429);
    expect(await report("10.9.8.7, 192.168.1.1, 127.0.0.1")).toBe(429);
    expect(await report("10.9.8.8, 10.9.8.7")).toBe(200);
  });

  test("a hop that is not an address is taken as the client", async () => {
    await fill("unknown");
    expect(await report("203.0.113.70, unknown")).toBe(429);
    expect(await report("unknown, 10.0.0.1")).toBe(429);
    expect(await report("203.0.113.70, UNKNOWN")).toBe(200);
    expect(await report("203.0.113.70, 10.0.0.1:8080")).toBe(200);
    expect(await report("203.0.113.70, [::1]")).toBe(200);
    expect(await report("203.0.113.70, 010.0.0.1")).toBe(200);
    expect(await report("203.0.113.70, localhost")).toBe(200);
  });

  test("spaces around a hop and empty hops are dropped; other whitespace is not", async () => {
    await fill("203.0.113.80");
    expect(await report("203.0.113.80 ,  10.0.0.1")).toBe(429);
    expect(await report("203.0.113.80,10.0.0.1")).toBe(429);
    expect(await report(",, 203.0.113.80 ,,")).toBe(429);
    expect(await report("9.9.9.9,   203.0.113.80   ")).toBe(429);
    // A tab is not trimmed, so the hop is not the same text.
    expect(await report("9.9.9.9,\t203.0.113.80")).toBe(200);
  });

  test("a repeated X-Forwarded-For header is read as one list", async () => {
    await fill("203.0.113.90");
    const post = async (lines: string[]) => {
      const head =
        "POST /api/client-error HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\n" +
        lines.map((line) => `X-Forwarded-For: ${line}\r\n`).join("") +
        "\r\n";
      return (await rawHttp(prod.ports.http, head)).status;
    };
    expect(await post(["203.0.113.90"])).toBe(429);
    expect(await post(["9.9.9.9", "203.0.113.90"])).toBe(429);
    expect(await post(["203.0.113.90", "10.0.0.1"])).toBe(429);
    expect(await post(["203.0.113.90", "9.9.9.9"])).toBe(200);
  });

  test("without the header the connection's own address counts, shared by everyone who sends none", async () => {
    await fill(undefined);
    expect(await report(undefined)).toBe(429);
    // The same bucket, named explicitly.
    expect(await report("127.0.0.1")).toBe(429);
    expect(await report("127.0.0.1, 10.0.0.1")).toBe(429);
    const raw = await rawHttp(prod.ports.http, rawRequest("POST", "/api/client-error", {}, ""));
    expect(raw.status).toBe(429);
    expect(await report("203.0.113.100")).toBe(200);
  });
});

describe("in development the forwarding headers are ignored", () => {
  let dev: TestServer;
  beforeAll(async () => {
    dev = await startServer();
  }, 90_000);
  afterAll(async () => {
    await dev?.stop();
  });

  const from = (address: string) => dev.client({ headers: { "X-Forwarded-For": address, "X-Forwarded-Proto": "https" } });

  test("client-error reports from 'different addresses' share one count", async () => {
    for (let i = 0; i < 5; i++) expect((await from(`203.0.113.${i + 1}`).post("/api/client-error", {})).status).toBe(200);
    expectLimited(await from("203.0.113.200").post("/api/client-error", {}), CLIENT_ERROR_LIMITED);
    expectLimited(await dev.client().post("/api/client-error", {}), CLIENT_ERROR_LIMITED);
  });

  test("failed logins from 'different addresses' share one count", async () => {
    for (let i = 0; i < 5; i++) {
      const result = await from(`203.0.113.${i + 1}`).post("/api/users/login", { username: "admin", password: "wrong" });
      expect(result.body).toEqual(INVALID_CREDENTIALS);
    }
    const refused = await from("203.0.113.201").post("/api/users/login", { username: "admin", password: ADMIN_PASSWORD });
    expectLimited(refused, LOGIN_LIMITED);
    expectLimited(
      await dev.client().post("/api/users/login", { username: "admin", password: ADMIN_PASSWORD }),
      LOGIN_LIMITED
    );
  });

  test("token requests from 'different addresses' share one count", async () => {
    for (let i = 0; i < 3; i++) {
      expect((await from(`203.0.113.${i + 1}`).post("/api/users/token", { email: i })).body).toEqual(INVALID_EMAIL);
    }
    expectLimited(await from("203.0.113.202").post("/api/users/token", { email: "x@mail.test" }), TOKEN_LIMITED);
  });
});
