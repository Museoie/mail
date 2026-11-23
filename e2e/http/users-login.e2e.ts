import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import {
  ADMIN_PASSWORD,
  adminUser,
  createUser,
  hashPassword,
  type TestServer,
  type TestUser,
} from "../harness";
import { startServer } from "./core-helpers";
import {
  ANONYMOUS_LOGIN_TEXT,
  BAD_REQUEST,
  INVALID_BODY,
  INVALID_CREDENTIALS,
  JSON_TYPE,
  NON_STRINGS,
  NOT_FOUND,
  SESSION_ID,
  expectSessionCookie,
  isAboutSevenDaysAhead,
  onlySetCookie,
  sessionCookie,
  sessionCount,
  sessionIdOf,
  sessionIdOfValue,
  sessionRow,
  sessionRows,
  sessionSecret,
} from "./core-helpers";

const RO_PASSWORD = "read-only-password-e2e";

/** Production posture: every client has its own address, so its own login quota. */
let server: TestServer;
/** Development posture: plain cookies, no read-only account. */
let dev: TestServer;
let admin: { id: string; email: string };
let alice: TestUser;
let bob: TestUser;

beforeAll(async () => {
  server = await startServer({ production: true, env: { ADMIN_RO_PASSWORD: RO_PASSWORD } });
  dev = await startServer();
  admin = await adminUser(server);
  alice = await createUser(server, "alice");
  bob = await createUser(server, "bob");
}, 120_000);

afterAll(async () => {
  await server?.stop();
  await dev?.stop();
});

const userText = (user: { id: string; email: string; username: string }, version = 1) =>
  `{"id":"${user.id}","email":"${user.email}","username":"${user.username}","passwordVersion":${version}}`;

describe("GET /api/users/login", () => {
  test("says nobody is signed in, with the app version and the sign-in mode", async () => {
    const before = await sessionCount(server);
    const { status, text, headers, setCookies } = await server.client().get("/api/users/login");
    expect(status).toBe(200);
    expect(text).toBe(ANONYMOUS_LOGIN_TEXT);
    expect(headers.get("content-type")).toBe(JSON_TYPE);
    expect(setCookies).toEqual([]);
    expect(await sessionCount(server)).toBe(before);
  });

  test("returns the signed-in user, key for key", async () => {
    const client = server.client();
    await client.login(alice.username, alice.password);
    const { status, text } = await client.get("/api/users/login");
    expect(status).toBe(200);
    expect(text).toBe(
      `{"status":"success","body":{"user":${userText(alice)},"app":{"version":"1"},"sso":{"enabled":false,"localLogin":true}}}`
    );
  });

  test("marks a read-only session and names the credential it signed in with", async () => {
    const client = server.client();
    await client.login("admin-ro", RO_PASSWORD);
    const { text } = await client.get("/api/users/login");
    expect(text).toBe(
      `{"status":"success","body":{"user":{"id":"${admin.id}","email":"admin@mail.test","username":"admin",` +
        `"passwordVersion":1,"isReadOnly":true,"authenticatedAs":"admin-ro"},` +
        `"app":{"version":"1"},"sso":{"enabled":false,"localLogin":true}}}`
    );
  });

  test("does not repeat a pending reset token that the login response carried", async () => {
    const carol = await createUser(server, "carol-get");
    const expiry = new Date(Date.now() + 3_600_000).toISOString();
    await server.db.query("UPDATE users SET token = 'pending-token', expiry = $2 WHERE user_id = $1", [carol.id, expiry]);
    const client = server.client();
    const login = await client.login(carol.username, carol.password);
    expect(login.body.body.token).toBe("pending-token");
    const { body } = await client.get("/api/users/login");
    expect(body.body.user).toEqual({ id: carol.id, email: carol.email, username: carol.username, passwordVersion: 1 });
  });

  test("answers HEAD with the headers of GET and no body", async () => {
    const client = server.client();
    const get = await client.get("/api/users/login");
    const head = await client.head("/api/users/login");
    expect(head.status).toBe(200);
    expect(head.text).toBe("");
    expect(head.headers.get("content-type")).toBe(JSON_TYPE);
    expect(head.headers.get("content-length")).toBe(String(ANONYMOUS_LOGIN_TEXT.length));
    expect(head.headers.get("etag")).toBe(get.headers.get("etag"));
  });

  test("answers with a trailing slash and in any letter case", async () => {
    const client = server.client();
    for (const path of ["/api/users/login/", "/API/USERS/LOGIN", "/Api/Users/Login/", "/api/users/login?next=%2F"]) {
      const { status, text } = await client.get(path);
      expect({ path, status, text }).toEqual({ path, status: 200, text: ANONYMOUS_LOGIN_TEXT });
    }
  });

  test("is not answered for PUT or PATCH, nor below the path", async () => {
    const client = server.client();
    for (const method of ["PUT", "PATCH"]) {
      const { status, body } = await client.request(method, "/api/users/login");
      expect({ method, status, body }).toEqual({ method, status: 404, body: NOT_FOUND });
    }
    for (const path of ["/api/users/login/extra", "/api/users/login//", "/api/users", "/api/users/"]) {
      const { status, body } = await client.get(path);
      expect({ path, status, body }).toEqual({ path, status: 404, body: NOT_FOUND });
    }
  });
});

describe("POST /api/users/login — the request body", () => {
  const post = (body: unknown) => server.client().post("/api/users/login", body);

  test.each(NON_STRINGS)("refuses a password that is %s", async (_label, password) => {
    const { status, body, setCookies } = await post({ username: "admin", password });
    expect(status).toBe(200);
    expect(body).toEqual(INVALID_CREDENTIALS);
    expect(setCookies).toEqual([]);
  });

  test("refuses an empty or missing password", async () => {
    for (const body of [{ username: "admin", password: "" }, { username: "admin" }, {}]) {
      const result = await post(body);
      expect({ sent: body, status: result.status, body: result.body }).toEqual({
        sent: body,
        status: 200,
        body: INVALID_CREDENTIALS,
      });
    }
  });

  test.each(NON_STRINGS)("refuses a username that is %s, even with the right email and password", async (_l, username) => {
    const { status, body } = await post({ username, email: admin.email, password: ADMIN_PASSWORD });
    expect(status).toBe(200);
    expect(body).toEqual(INVALID_CREDENTIALS);
  });

  test.each(NON_STRINGS)("refuses an email that is %s, even with the right username and password", async (_l, email) => {
    const { status, body } = await post({ username: "admin", email, password: ADMIN_PASSWORD });
    expect(status).toBe(200);
    expect(body).toEqual(INVALID_CREDENTIALS);
  });

  test("a body with the wrong types does not count against the login quota", async () => {
    const client = server.client();
    for (const [, value] of NON_STRINGS) {
      expect((await client.post("/api/users/login", { username: "admin", password: value })).body).toEqual(
        INVALID_CREDENTIALS
      );
      expect((await client.post("/api/users/login", { username: value, password: ADMIN_PASSWORD })).body).toEqual(
        INVALID_CREDENTIALS
      );
    }
    const { body } = await client.post("/api/users/login", { username: "admin", password: ADMIN_PASSWORD });
    expect(body.status).toBe("success");
  });

  test("refuses a body that is an array", async () => {
    for (const body of [[], [{ username: "admin", password: ADMIN_PASSWORD }], ["admin", ADMIN_PASSWORD]]) {
      const result = await post(body);
      expect(result.status).toBe(200);
      expect(result.body).toEqual(INVALID_BODY);
      expect(result.setCookies).toEqual([]);
    }
  });

  test("answers 400 to a body that is not JSON", async () => {
    const { status, body, headers } = await server.client().request("POST", "/api/users/login", {
      body: "username=admin",
      headers: { "Content-Type": "application/json" },
    });
    expect(status).toBe(400);
    expect(body).toEqual(BAD_REQUEST);
    expect(headers.get("content-type")).toBe(JSON_TYPE);
  });

  test("treats a missing or empty body as no credentials", async () => {
    const client = server.client();
    const none = await client.request("POST", "/api/users/login");
    expect(none.body).toEqual(INVALID_CREDENTIALS);
    const empty = await client.request("POST", "/api/users/login", {
      body: "",
      headers: { "Content-Type": "application/json" },
    });
    expect(empty.status).toBe(200);
    expect(empty.body).toEqual(INVALID_CREDENTIALS);
  });

  test("does not read a form-encoded body", async () => {
    const { status, body } = await server.client().request("POST", "/api/users/login", {
      body: new URLSearchParams({ username: "admin", password: ADMIN_PASSWORD }),
    });
    expect(status).toBe(200);
    expect(body).toEqual(INVALID_CREDENTIALS);
  });

  test("does not read JSON sent under another content type", async () => {
    const json = JSON.stringify({ username: "admin", password: ADMIN_PASSWORD });
    for (const type of ["text/plain", "application/jsonx", "application/vnd.api+json", "text/json"]) {
      const { status, body } = await server.client().request("POST", "/api/users/login", {
        body: json,
        headers: { "Content-Type": type },
      });
      expect({ type, status, body }).toEqual({ type, status: 200, body: INVALID_CREDENTIALS });
    }
  });

  test("reads JSON whatever the case and parameters of its content type", async () => {
    const json = JSON.stringify({ username: "admin", password: ADMIN_PASSWORD });
    for (const type of ["Application/JSON", "application/json;charset=UTF-8", 'application/json; charset="utf-8"; x=y']) {
      const { body } = await server.client().request("POST", "/api/users/login", {
        body: json,
        headers: { "Content-Type": type },
      });
      expect({ type, status: body?.status }).toEqual({ type, status: "success" });
    }
  });

  test("ignores fields it does not know", async () => {
    const { body } = await post({ username: "admin", password: ADMIN_PASSWORD, remember: true, extra: { a: [1] } });
    expect(body.status).toBe("success");
  });

  test("takes the last of a repeated key", async () => {
    const raw = `{"username":"nobody","username":"admin","password":"wrong","password":${JSON.stringify(ADMIN_PASSWORD)}}`;
    const { body } = await server.client().request("POST", "/api/users/login", {
      body: raw,
      headers: { "Content-Type": "application/json" },
    });
    expect(body.status).toBe("success");
  });
});

describe("POST /api/users/login — who signs in", () => {
  test("by username", async () => {
    const { status, text } = await server.client().post("/api/users/login", {
      username: alice.username,
      password: alice.password,
    });
    expect(status).toBe(200);
    expect(text).toBe(`{"status":"success","body":${userText(alice)}}`);
  });

  test("by email", async () => {
    const { text } = await server.client().post("/api/users/login", { email: alice.email, password: alice.password });
    expect(text).toBe(`{"status":"success","body":${userText(alice)}}`);
  });

  test("by username and email that name the same account", async () => {
    const { text } = await server.client().post("/api/users/login", {
      username: alice.username,
      email: alice.email,
      password: alice.password,
    });
    expect(text).toBe(`{"status":"success","body":${userText(alice)}}`);
  });

  test("not by a username and an email of two different accounts", async () => {
    for (const password of [alice.password, bob.password]) {
      const { body, setCookies } = await server.client().post("/api/users/login", {
        username: alice.username,
        email: bob.email,
        password,
      });
      expect(body).toEqual(INVALID_CREDENTIALS);
      expect(setCookies).toEqual([]);
    }
  });

  test("an empty username or email is ignored in favour of the other", async () => {
    const byEmail = await server.client().post("/api/users/login", {
      username: "",
      email: alice.email,
      password: alice.password,
    });
    expect(byEmail.body.status).toBe("success");
    const byUsername = await server.client().post("/api/users/login", {
      username: alice.username,
      email: "",
      password: alice.password,
    });
    expect(byUsername.body.status).toBe("success");
  });

  test("nobody, when neither a username nor an email is given", async () => {
    for (const body of [{ password: alice.password }, { username: "", email: "", password: alice.password }]) {
      const result = await server.client().post("/api/users/login", body);
      expect(result.body).toEqual(INVALID_CREDENTIALS);
    }
  });

  test("the username and the email are matched exactly", async () => {
    const variants = [
      { username: "Alice" },
      { username: "ALICE" },
      { username: " alice" },
      { username: "alice " },
      { email: alice.email.toUpperCase() },
      { email: ` ${alice.email}` },
    ];
    for (const identity of variants) {
      const { body } = await server.client().post("/api/users/login", { ...identity, password: alice.password });
      expect({ identity, body }).toEqual({ identity, body: INVALID_CREDENTIALS });
    }
  });

  test("refuses a wrong password", async () => {
    const { status, body, setCookies } = await server.client().post("/api/users/login", {
      username: alice.username,
      password: `${alice.password}x`,
    });
    expect(status).toBe(200);
    expect(body).toEqual(INVALID_CREDENTIALS);
    expect(setCookies).toEqual([]);
  });

  test("refuses another account's password", async () => {
    const { body } = await server.client().post("/api/users/login", { username: alice.username, password: bob.password });
    expect(body).toEqual(INVALID_CREDENTIALS);
  });

  test("refuses an unknown user the same way", async () => {
    const before = await sessionCount(server);
    const { status, body, setCookies } = await server.client().post("/api/users/login", {
      username: "nobody-here",
      password: "whatever",
    });
    expect(status).toBe(200);
    expect(body).toEqual(INVALID_CREDENTIALS);
    expect(setCookies).toEqual([]);
    expect(await sessionCount(server)).toBe(before);
  });

  test("a password is case sensitive and not trimmed", async () => {
    for (const password of [alice.password.toUpperCase(), ` ${alice.password}`, `${alice.password} `]) {
      const { body } = await server.client().post("/api/users/login", { username: alice.username, password });
      expect(body).toEqual(INVALID_CREDENTIALS);
    }
  });

  test("returns a pending reset token and its expiry with the user", async () => {
    const carol = await createUser(server, "carol");
    const expiry = "2031-02-03T04:05:06.789Z";
    await server.db.query("UPDATE users SET token = $2, expiry = $3 WHERE user_id = $1", [carol.id, "reset-token-1", expiry]);
    const { text } = await server.client().post("/api/users/login", { username: "carol", password: carol.password });
    expect(text).toBe(
      `{"status":"success","body":{"id":"${carol.id}","email":"${carol.email}","username":"carol",` +
        `"token":"reset-token-1","expiry":"${expiry}","passwordVersion":1}}`
    );
  });

  test("reports the credential generation the account is on", async () => {
    const dave = await createUser(server, "dave");
    await server.db.query("UPDATE users SET password_version = 7 WHERE user_id = $1", [dave.id]);
    const client = server.client();
    const { body } = await client.login("dave", dave.password);
    expect(body.body.passwordVersion).toBe(7);
    expect((await sessionRow(server, sessionIdOf(client)))!.session_password_version).toBe(7);

    await server.db.query("UPDATE users SET password_version = NULL WHERE user_id = $1", [dave.id]);
    const again = server.client();
    expect((await again.login("dave", dave.password)).body.body.passwordVersion).toBe(1);
    expect((await sessionRow(server, sessionIdOf(again)))!.session_password_version).toBe(1);
  });

  test("refuses an account that has no password yet (a sign-up in progress) as it refuses a wrong password", async () => {
    // A sign-up is a row with a NULL password until set-info writes one.
    await server.db.query(
      "INSERT INTO users (username, email, token, expiry) VALUES ('user_pending1', 'pending@elsewhere.example', 'tok', NOW() + INTERVAL '1 hour')"
    );
    const before = await sessionCount(server);
    const unknown = await server.client().post("/api/users/login", { username: "user_nobody1", password: "anything" });
    for (const identity of [{ username: "user_pending1" }, { email: "pending@elsewhere.example" }]) {
      for (const password of ["anything", "tok"]) {
        const { status, text, headers, setCookies } = await server.client().post("/api/users/login", {
          ...identity,
          password,
        });
        // Nothing tells the account apart from one that does not exist.
        expect({ identity, password, status, text }).toEqual({ identity, password, status: 200, text: unknown.text });
        expect(headers.get("content-type")).toBe(JSON_TYPE);
        expect(setCookies).toEqual([]);
      }
    }
    expect(JSON.parse(unknown.text)).toEqual(INVALID_CREDENTIALS);
    expect(await sessionCount(server)).toBe(before);
  });

  test("a soft-deleted account cannot sign in", async () => {
    const erin = await createUser(server, "erin");
    await server.db.query("UPDATE users SET is_deleted = TRUE WHERE user_id = $1", [erin.id]);
    const { body } = await server.client().post("/api/users/login", { username: "erin", password: erin.password });
    expect(body).toEqual(INVALID_CREDENTIALS);
    await server.db.query("UPDATE users SET is_deleted = NULL WHERE user_id = $1", [erin.id]);
    const restored = await server.client().post("/api/users/login", { username: "erin", password: erin.password });
    expect(restored.body.status).toBe("success");
  });

  test("an account without an email cannot sign in, whatever its password", async () => {
    const frank = await createUser(server, "frank");
    for (const email of [null, ""]) {
      await server.db.query("UPDATE users SET email = $2 WHERE user_id = $1", [frank.id, email]);
      const { body } = await server.client().post("/api/users/login", { username: "frank", password: frank.password });
      expect({ email, body }).toEqual({ email, body: INVALID_CREDENTIALS });
    }
  });

  test("a stored password that is not a digest never matches", async () => {
    await server.db.query(
      "INSERT INTO users (username, password, email) VALUES ('plain', 'plain-text-password', 'plain@elsewhere.example'), ('emptypw', '', 'emptypw@elsewhere.example')"
    );
    const plain = await server.client().post("/api/users/login", { username: "plain", password: "plain-text-password" });
    expect(plain.status).toBe(200);
    expect(plain.body).toEqual(INVALID_CREDENTIALS);
    const empty = await server.client().post("/api/users/login", { username: "emptypw", password: "x" });
    expect(empty.status).toBe(200);
    expect(empty.body).toEqual(INVALID_CREDENTIALS);
  });

  test("verifies the $2a$ and $2y$ spellings of a bcrypt digest", async () => {
    const digest = await hashPassword("legacy-password");
    expect(digest.startsWith("$2b$")).toBe(true);
    await server.db.query(
      "INSERT INTO users (username, password, email) VALUES ('legacy-a', $1, 'legacy-a@elsewhere.example'), ('legacy-y', $2, 'legacy-y@elsewhere.example')",
      [digest.replace("$2b$", "$2a$"), digest.replace("$2b$", "$2y$")]
    );
    for (const username of ["legacy-a", "legacy-y"]) {
      const ok = await server.client().post("/api/users/login", { username, password: "legacy-password" });
      expect({ username, status: ok.body.status }).toEqual({ username, status: "success" });
      const wrong = await server.client().post("/api/users/login", { username, password: "legacy-passwore" });
      expect(wrong.body).toEqual(INVALID_CREDENTIALS);
    }
  });
});

describe("POST /api/users/login — the password", () => {
  test("only its first 72 bytes count", async () => {
    const long = `${"a".repeat(72)}-tail-that-is-never-read`;
    const user = await createUser(server, "longpw", { password: long });
    const exact = await server.client().post("/api/users/login", { username: user.username, password: long });
    expect(exact.body.status).toBe("success");
    const otherTail = await server.client().post("/api/users/login", {
      username: user.username,
      password: `${"a".repeat(72)}completely different`,
    });
    expect(otherTail.body.status).toBe("success");
    const cut = await server.client().post("/api/users/login", { username: user.username, password: "a".repeat(72) });
    expect(cut.body.status).toBe("success");
    const short = await server.client().post("/api/users/login", { username: user.username, password: "a".repeat(71) });
    expect(short.body).toEqual(INVALID_CREDENTIALS);
    const differsAt72 = await server.client().post("/api/users/login", {
      username: user.username,
      password: `${"a".repeat(71)}b`,
    });
    expect(differsAt72.body).toEqual(INVALID_CREDENTIALS);
  });

  test("the 72 bytes are counted in UTF-8, cutting through a character if need be", async () => {
    // 71 ASCII bytes, then "é" (C3 A9): byte 72 is C3, and A9 is never read.
    const password = `${"k".repeat(71)}é`;
    const user = await createUser(server, "splitpw", { password });
    const same = await server.client().post("/api/users/login", { username: user.username, password });
    expect(same.body.status).toBe("success");
    // "è" is C3 A8: the same first byte, so the same 72 bytes.
    const sibling = await server.client().post("/api/users/login", {
      username: user.username,
      password: `${"k".repeat(71)}è`,
    });
    expect(sibling.body.status).toBe("success");
    // "ñ" is C3 B1 — also the same first byte.
    const other = await server.client().post("/api/users/login", {
      username: user.username,
      password: `${"k".repeat(71)}ñ and more`,
    });
    expect(other.body.status).toBe("success");
    // "ē" is C4 93: byte 72 differs.
    const different = await server.client().post("/api/users/login", {
      username: user.username,
      password: `${"k".repeat(71)}ē`,
    });
    expect(different.body).toEqual(INVALID_CREDENTIALS);
  });

  test("accepts unicode passwords and compares them byte for byte", async () => {
    const password = "pässwörd 日本語 🔑 ñ";
    const user = await createUser(server, "unicodepw", { password });
    const ok = await server.client().post("/api/users/login", { username: user.username, password });
    expect(ok.body.status).toBe("success");
    // The same text in decomposed form is other bytes.
    const decomposed = await server.client().post("/api/users/login", {
      username: user.username,
      password: password.normalize("NFD"),
    });
    expect(decomposed.body).toEqual(INVALID_CREDENTIALS);
    const escaped = await server.client().request("POST", "/api/users/login", {
      body: `{"username":"unicodepw","password":"p\\u00e4ssw\\u00f6rd \\u65e5\\u672c\\u8a9e \\ud83d\\udd11 \\u00f1"}`,
      headers: { "Content-Type": "application/json" },
    });
    expect(escaped.body.status).toBe("success");
  });

  test("a password may hold quotes, backslashes, control characters and a NUL-free binary soup", async () => {
    const password = "p\"a\\s's\tw\no\rr\u0001d\u007f<>&%00";
    const user = await createUser(server, "oddpw", { password });
    const ok = await server.client().post("/api/users/login", { username: user.username, password });
    expect(ok.body.status).toBe("success");
  });
});

describe("POST /api/users/login — the session it issues", () => {
  test("writes the session row", async () => {
    const client = server.client();
    const login = await client.login(alice.username, alice.password);
    const id = sessionIdOf(client);
    expect(id).toMatch(SESSION_ID);
    const row = (await sessionRow(server, id))!;
    expect(row).toMatchObject({
      session_id: id,
      session_user_id: alice.id,
      session_username: "alice",
      session_email: alice.email,
      session_is_read_only: null,
      session_authenticated_as: null,
      session_password_version: 1,
      auth_sid: null,
      auth_refresh_token: null,
      auth_recheck_at: null,
      cookie_original_max_age: "604800000",
      cookie_signed: null,
      cookie_http_only: true,
      cookie_path: "/",
      cookie_domain: null,
      cookie_secure: "true",
      cookie_same_site: '"strict"',
    });
    const maxAge = Number(row.cookie_max_age);
    expect(maxAge).toBeGreaterThan(604_800_000 - 5_000);
    expect(maxAge).toBeLessThanOrEqual(604_800_000);
    expect(isAboutSevenDaysAhead(row.cookie_expires!)).toBe(true);
    expect(Math.abs(row.updated.getTime() - Date.now())).toBeLessThan(15_000);

    // The cookie's Expires is the row's expiry, to the second.
    const cookie = onlySetCookie(login);
    expect(Math.floor(row.cookie_expires!.getTime() / 1000)).toBe(Date.parse(cookie.attributes.expires!) / 1000);
  });

  test("sets a Secure cookie in production", async () => {
    const login = await server.client().login(alice.username, alice.password);
    const cookie = onlySetCookie(login);
    expectSessionCookie(cookie, { secure: true });
    expect(login.setCookies[0]).toMatch(
      /^connect\.sid=s%3A[A-Za-z0-9_-]{32}\.[A-Za-z0-9%]+; Path=\/; Expires=[A-Za-z0-9,: ]+ GMT; HttpOnly; Secure; SameSite=Strict$/
    );
  });

  test("sets a plain cookie in development, and stores it as not secure", async () => {
    const client = dev.client();
    const login = await client.login("admin", ADMIN_PASSWORD);
    const cookie = onlySetCookie(login);
    expectSessionCookie(cookie, { secure: false });
    expect(login.setCookies[0]).toMatch(
      /^connect\.sid=s%3A[A-Za-z0-9_-]{32}\.[A-Za-z0-9%]+; Path=\/; Expires=[A-Za-z0-9,: ]+ GMT; HttpOnly; SameSite=Strict$/
    );
    expect((await sessionRow(dev, sessionIdOf(client)))!.cookie_secure).toBe("false");
  });

  test("signs the cookie with the server's secret", async () => {
    const client = server.client();
    const login = await client.login(alice.username, alice.password);
    const id = sessionIdOf(client);
    expect(login.setCookies[0]!.split(";")[0]).toBe(sessionCookie(id, sessionSecret(server)));
  });

  test("a failed login sets no cookie and writes no row", async () => {
    const before = await sessionCount(server);
    const { setCookies } = await server.client().post("/api/users/login", { username: "alice", password: "nope" });
    expect(setCookies).toEqual([]);
    expect(await sessionCount(server)).toBe(before);
  });

  test("never keeps a session id the client arrived with", async () => {
    // A validly signed cookie for an id the attacker chose, planted before sign-in.
    const planted = "planted-session-id-0123456789abcd";
    const client = server.client();
    const login = await client.request("POST", "/api/users/login", {
      json: { username: alice.username, password: alice.password },
      headers: { Cookie: sessionCookie(planted, sessionSecret(server)) },
    });
    expect(login.body.status).toBe("success");
    const issued = sessionIdOfValue(onlySetCookie(login).value);
    expect(issued).not.toBe(planted);
    expect(issued).toMatch(SESSION_ID);
    expect(await sessionRow(server, planted)).toBeUndefined();
    expect((await sessionRow(server, issued))!.session_user_id).toBe(alice.id);
  });

  test("replaces the session of a client that signs in again, deleting the old row", async () => {
    const client = server.client();
    await client.login(alice.username, alice.password);
    const first = sessionIdOf(client);
    await client.login(alice.username, alice.password);
    const second = sessionIdOf(client);
    expect(second).not.toBe(first);
    expect(await sessionRow(server, first)).toBeUndefined();
    expect((await sessionRow(server, second))!.session_user_id).toBe(alice.id);
  });

  test("signing in as someone else from a signed-in client ends the first session", async () => {
    const client = server.client();
    await client.login(alice.username, alice.password);
    const first = sessionIdOf(client);
    await client.login(bob.username, bob.password);
    expect(await sessionRow(server, first)).toBeUndefined();
    expect((await client.get("/api/users/login")).body.body.user.username).toBe("bob");
    const mine = (await sessionRows(server)).filter((row) => row.session_id === sessionIdOf(client));
    expect(mine).toHaveLength(1);
  });

  test("a failed login from a signed-in client leaves its session in place", async () => {
    const client = server.client();
    await client.login(alice.username, alice.password);
    const id = sessionIdOf(client);
    const failed = await client.post("/api/users/login", { username: "alice", password: "wrong" });
    expect(failed.body).toEqual(INVALID_CREDENTIALS);
    // The live session is renewed like on any other request.
    expect(sessionIdOfValue(onlySetCookie(failed).value)).toBe(id);
    expect((await client.get("/api/users/login")).body.body.user.username).toBe("alice");
  });

  test("other clients' sessions of the same user are untouched", async () => {
    const one = server.client();
    const two = server.client();
    await one.login(alice.username, alice.password);
    await two.login(alice.username, alice.password);
    expect(sessionIdOf(one)).not.toBe(sessionIdOf(two));
    expect((await one.get("/api/users/login")).body.body.user.username).toBe("alice");
    expect((await two.get("/api/users/login")).body.body.user.username).toBe("alice");
  });
});

describe("POST /api/users/login — the read-only account", () => {
  test("signs in as admin's identity, marked read-only", async () => {
    const client = server.client();
    const login = await client.post("/api/users/login", { username: "admin-ro", password: RO_PASSWORD });
    expect(login.status).toBe(200);
    expect(login.text).toBe(
      `{"status":"success","body":{"id":"${admin.id}","email":"admin@mail.test","username":"admin",` +
        `"passwordVersion":1,"isReadOnly":true,"authenticatedAs":"admin-ro"}}`
    );
    const row = (await sessionRow(server, sessionIdOf(client)))!;
    expect(row).toMatchObject({
      session_user_id: admin.id,
      session_username: "admin",
      session_email: "admin@mail.test",
      session_is_read_only: true,
      session_authenticated_as: "admin-ro",
      session_password_version: 1,
    });
  });

  test("signs in by its email address too", async () => {
    const { body } = await server.client().post("/api/users/login", {
      email: "admin-ro@mail.test",
      password: RO_PASSWORD,
    });
    expect(body.body).toMatchObject({ username: "admin", isReadOnly: true, authenticatedAs: "admin-ro" });
  });

  test("does not accept admin's password, nor admin the read-only one", async () => {
    const one = await server.client().post("/api/users/login", { username: "admin-ro", password: ADMIN_PASSWORD });
    expect(one.body).toEqual(INVALID_CREDENTIALS);
    const two = await server.client().post("/api/users/login", { username: "admin", password: RO_PASSWORD });
    expect(two.body).toEqual(INVALID_CREDENTIALS);
  });

  test("never hands out admin's pending reset token", async () => {
    await server.db.query("UPDATE users SET token = 'admin-reset', expiry = NOW() + INTERVAL '1 hour' WHERE username = 'admin'");
    try {
      const readOnly = await server.client().post("/api/users/login", { username: "admin-ro", password: RO_PASSWORD });
      expect(Object.keys(readOnly.body.body)).toEqual([
        "id",
        "email",
        "username",
        "passwordVersion",
        "isReadOnly",
        "authenticatedAs",
      ]);
      const asAdmin = await server.client().post("/api/users/login", { username: "admin", password: ADMIN_PASSWORD });
      expect(asAdmin.body.body.token).toBe("admin-reset");
    } finally {
      await server.db.query("UPDATE users SET token = NULL, expiry = NULL WHERE username = 'admin'");
    }
  });

  test("carries admin's credential generation, not its own", async () => {
    await server.db.query("UPDATE users SET password_version = 5 WHERE username = 'admin-ro'");
    try {
      const client = server.client();
      const { body } = await client.login("admin-ro", RO_PASSWORD);
      expect(body.body.passwordVersion).toBe(1);
      expect((await client.get("/api/users/login")).body.body.user.authenticatedAs).toBe("admin-ro");
    } finally {
      await server.db.query("UPDATE users SET password_version = 1 WHERE username = 'admin-ro'");
    }
  });

  test("does not exist on a server started without ADMIN_RO_PASSWORD", async () => {
    expect(await dev.db.query("SELECT 1 FROM users WHERE username = 'admin-ro'")).toEqual([]);
    const { body } = await dev.client().post("/api/users/login", { username: "admin-ro", password: RO_PASSWORD });
    expect(body).toEqual(INVALID_CREDENTIALS);
  });
});

describe("DELETE /api/users/login", () => {
  test("ends the session: the row is gone and the old cookie is anonymous", async () => {
    const client = server.client();
    await client.login(alice.username, alice.password);
    const id = sessionIdOf(client);
    const logout = await client.delete("/api/users/login");
    expect(logout.status).toBe(200);
    expect(logout.text).toBe('{"status":"success"}');
    // No cookie is set or cleared: the browser keeps a cookie that names nothing.
    expect(logout.setCookies).toEqual([]);
    expect(await sessionRow(server, id)).toBeUndefined();

    expect(sessionIdOf(client)).toBe(id);
    const after = await client.get("/api/users/login");
    expect(after.text).toBe(ANONYMOUS_LOGIN_TEXT);
    expect(after.setCookies).toEqual([]);
    expect(await sessionRow(server, id)).toBeUndefined();
  });

  test("answers success without a session, and writes nothing", async () => {
    const before = await sessionCount(server);
    const { status, text, setCookies } = await server.client().delete("/api/users/login");
    expect(status).toBe(200);
    expect(text).toBe('{"status":"success"}');
    expect(setCookies).toEqual([]);
    expect(await sessionCount(server)).toBe(before);
  });

  test("ends only the session it was sent with", async () => {
    const one = server.client();
    const two = server.client();
    await one.login(alice.username, alice.password);
    await two.login(alice.username, alice.password);
    await one.delete("/api/users/login");
    expect(await sessionRow(server, sessionIdOf(one))).toBeUndefined();
    expect((await two.get("/api/users/login")).body.body.user.username).toBe("alice");
  });

  test("answers with a trailing slash and in any letter case", async () => {
    for (const path of ["/api/users/login/", "/API/Users/LOGIN"]) {
      const client = server.client();
      await client.login(alice.username, alice.password);
      const { status, body } = await client.delete(path);
      expect({ path, status, body }).toEqual({ path, status: 200, body: { status: "success" } });
      expect(await sessionRow(server, sessionIdOf(client))).toBeUndefined();
    }
  });

  test("signing in again after signing out works", async () => {
    const client = server.client();
    await client.login(alice.username, alice.password);
    await client.delete("/api/users/login");
    await client.login(alice.username, alice.password);
    expect((await client.get("/api/users/login")).body.body.user.username).toBe("alice");
  });
});
