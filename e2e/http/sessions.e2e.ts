import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import {
  ADMIN_PASSWORD,
  adminUser,
  createUser,
  rawHttp,
  waitFor,
  type ApiClient,
  type TestServer,
  type TestUser,
} from "../harness";
import { startServer } from "./core-helpers";
import {
  ANONYMOUS_LOGIN_TEXT,
  INTERNAL_ERROR,
  NOT_LOGGED_IN,
  SESSION_ID,
  expectSessionCookie,
  isAbout,
  isAboutSevenDaysAhead,
  onlySetCookie,
  parseSetCookie,
  rawRequest,
  sessionCookie,
  sessionCount,
  sessionIdOf,
  sessionIdOfValue,
  sessionRow,
  sessionSecret,
  signSessionId,
} from "./core-helpers";

const HOUR = 3_600_000;

/** Development posture: the published fallback secret, plain cookies. */
let dev: TestServer;
/** Production posture: a private secret, Secure cookies, forwarding headers believed. */
let prod: TestServer;
let alice: TestUser;

beforeAll(async () => {
  dev = await startServer();
  prod = await startServer({ production: true });
  alice = await createUser(dev, "alice");
  await createUser(prod, "alice");
}, 120_000);

afterAll(async () => {
  await dev?.stop();
  await prod?.stop();
});

const signedIn = async (server: TestServer = dev, username = "alice"): Promise<{ client: ApiClient; id: string }> => {
  const client = server.client();
  await client.login(username, username === "admin" ? ADMIN_PASSWORD : `password-of-${username}`);
  return { client, id: sessionIdOf(client) };
};

const whoAmI = async (client: ApiClient, cookie?: string) => {
  const result = await client.get(
    "/api/users/login",
    cookie === undefined ? undefined : { headers: { Cookie: cookie }, sendCookies: false }
  );
  return { result, user: result.body?.body?.user as Record<string, unknown> | undefined };
};

describe("a live session is renewed on every request", () => {
  test("the cookie comes back with a new expiry and the row moves with it", async () => {
    const { client, id } = await signedIn();
    const sent = client.cookies.get("connect.sid");
    // An hour of life left, last touched a day ago.
    await dev.db.query(
      "UPDATE sessions SET cookie_expires = NOW() + INTERVAL '1 hour', cookie_max_age = 1, updated = NOW() - INTERVAL '1 day' WHERE session_id = $1",
      [id]
    );

    const { result, user } = await whoAmI(client);
    expect(user?.username).toBe("alice");
    const cookie = onlySetCookie(result);
    expectSessionCookie(cookie, { secure: false });
    // The same id and the same signature: only the expiry changes.
    expect(cookie.value).toBe(sent!);

    const row = (await sessionRow(dev, id))!;
    expect(isAboutSevenDaysAhead(row.cookie_expires!)).toBe(true);
    expect(Math.floor(row.cookie_expires!.getTime() / 1000)).toBe(Date.parse(cookie.attributes.expires!) / 1000);
    const maxAge = Number(row.cookie_max_age);
    expect(maxAge).toBeGreaterThan(7 * 24 * HOUR - 5_000);
    expect(maxAge).toBeLessThanOrEqual(7 * 24 * HOUR);
    expect(Math.abs(row.updated.getTime() - Date.now())).toBeLessThan(15_000);
    // Everything else about the row stays.
    expect(row).toMatchObject({
      session_user_id: alice.id,
      session_username: "alice",
      session_email: alice.email,
      session_password_version: 1,
      cookie_original_max_age: "604800000",
      cookie_http_only: true,
      cookie_path: "/",
      cookie_secure: "false",
      cookie_same_site: '"strict"',
    });
  });

  test("whatever the request is: an API route, a 404, a failed POST, a static file, HEAD, a 304", async () => {
    const { client, id } = await signedIn();
    const etag = (await client.get("/api/ping")).headers.get("etag")!;
    const requests: [string, string, { headers?: Record<string, string>; json?: unknown }?][] = [
      ["GET", "/api/ping"],
      ["HEAD", "/api/ping"],
      ["GET", "/api/health"],
      ["GET", "/api/nothing-here"],
      ["POST", "/api/users/login", { json: { username: 5, password: 5 } }],
      ["POST", "/api/users/login", { headers: { "Content-Type": "application/json" } }],
      ["GET", "/"],
      ["GET", "/manifest.json"],
      ["GET", "/some/client/route"],
      ["GET", "/.well-known/autoconfig/mail/config-v1.1.xml"],
      ["POST", "/not-an-api-path"],
      ["GET", "/api/ping", { headers: { "If-None-Match": etag } }],
    ];
    for (const [method, path, options] of requests) {
      const result = await client.request(method, path, options);
      const label = `${method} ${path} → ${result.status}`;
      expect({ label, cookies: result.setCookies.length }).toEqual({ label, cookies: 1 });
      expect(sessionIdOfValue(parseSetCookie(result.setCookies[0]!).value)).toBe(id);
    }
    const notModified = await client.get("/api/ping", { headers: { "If-None-Match": etag } });
    expect(notModified.status).toBe(304);
  });

  test("a body the server refuses to parse answers before any session is read", async () => {
    const { client, id } = await signedIn();
    await dev.db.query("UPDATE sessions SET updated = NOW() - INTERVAL '1 day' WHERE session_id = $1", [id]);
    const result = await client.request("POST", "/api/users/login", {
      body: "{not json",
      headers: { "Content-Type": "application/json" },
    });
    expect(result.status).toBe(400);
    expect(result.setCookies).toEqual([]);
    const row = (await sessionRow(dev, id))!;
    expect(Date.now() - row.updated.getTime()).toBeGreaterThan(23 * HOUR);
  });

  test("the row's own maximum age decides how far the expiry moves", async () => {
    const { client, id } = await signedIn();
    await dev.db.query("UPDATE sessions SET cookie_original_max_age = $2 WHERE session_id = $1", [id, HOUR]);
    const { result } = await whoAmI(client);
    const cookie = onlySetCookie(result);
    expect(isAbout(cookie.attributes.expires!, HOUR)).toBe(true);
    const row = (await sessionRow(dev, id))!;
    expect(isAbout(row.cookie_expires!, HOUR)).toBe(true);
    expect(row.cookie_original_max_age).toBe(String(HOUR));
    expect(Number(row.cookie_max_age)).toBeGreaterThan(HOUR - 5_000);
    expect(Number(row.cookie_max_age)).toBeLessThanOrEqual(HOUR);
  });

  test("a row without a maximum age, or with zero, is renewed for seven days", async () => {
    for (const value of [null, 0]) {
      const { client, id } = await signedIn();
      await dev.db.query(
        "UPDATE sessions SET cookie_original_max_age = $2, cookie_expires = NOW() + INTERVAL '1 hour' WHERE session_id = $1",
        [id, value]
      );
      const { result, user } = await whoAmI(client);
      expect(user?.username).toBe("alice");
      expect(isAboutSevenDaysAhead(onlySetCookie(result).attributes.expires!)).toBe(true);
      const row = (await sessionRow(dev, id))!;
      expect(isAboutSevenDaysAhead(row.cookie_expires!)).toBe(true);
      // The column is written back as no maximum age at all.
      expect(row.cookie_original_max_age).toBeNull();
    }
  });

  test("the identity comes from the session row, not from the users table", async () => {
    const { client, id } = await signedIn();
    await dev.db.query(
      "UPDATE sessions SET session_username = 'renamed', session_email = 'renamed@elsewhere.example' WHERE session_id = $1",
      [id]
    );
    const { user } = await whoAmI(client);
    expect(user).toEqual({ id: alice.id, email: "renamed@elsewhere.example", username: "renamed", passwordVersion: 1 });
    await dev.db.query("UPDATE users SET email = 'moved@elsewhere.example' WHERE user_id = $1", [alice.id]);
    try {
      expect((await whoAmI(client)).user?.email).toBe("renamed@elsewhere.example");
    } finally {
      await dev.db.query("UPDATE users SET email = $2 WHERE user_id = $1", [alice.id, alice.email]);
    }
  });

  test("the read-only flag and its attribution come from the row", async () => {
    const { client, id } = await signedIn();
    await dev.db.query(
      "UPDATE sessions SET session_is_read_only = TRUE, session_authenticated_as = 'someone' WHERE session_id = $1",
      [id]
    );
    expect((await whoAmI(client)).result.text).toContain(
      `"username":"alice","passwordVersion":1,"isReadOnly":true,"authenticatedAs":"someone"}`
    );

    await dev.db.query("UPDATE sessions SET session_authenticated_as = NULL WHERE session_id = $1", [id]);
    expect((await whoAmI(client)).result.text).toContain(`"username":"alice","passwordVersion":1,"isReadOnly":true}`);

    // Without the flag the attribution is not read at all.
    await dev.db.query(
      "UPDATE sessions SET session_is_read_only = FALSE, session_authenticated_as = 'someone' WHERE session_id = $1",
      [id]
    );
    expect((await whoAmI(client)).user).toEqual({ id: alice.id, email: alice.email, username: "alice", passwordVersion: 1 });
    // The renewal writes the row's flags back as they were read.
    const row = (await sessionRow(dev, id))!;
    expect(row.session_is_read_only).toBeNull();
    expect(row.session_authenticated_as).toBeNull();
  });
});

describe("the cookie's attributes are the ones stored with the session", () => {
  const renewedWith = async (column: string, value: unknown) => {
    const { client, id } = await signedIn();
    await dev.db.query(`UPDATE sessions SET ${column} = $2 WHERE session_id = $1`, [id, value]);
    const { result, user } = await whoAmI(client);
    return { result, user, id, client };
  };

  test.each([
    ['"strict"', "SameSite=Strict"],
    ['"lax"', "SameSite=Lax"],
    ['"none"', "SameSite=None"],
    ["true", "SameSite=Strict"],
  ])("cookie_same_site %s → %s", async (stored, attribute) => {
    const { result, user, id } = await renewedWith("cookie_same_site", stored);
    expect(user?.username).toBe("alice");
    expect(result.setCookies[0]!.endsWith(`; HttpOnly; ${attribute}`)).toBe(true);
    expect((await sessionRow(dev, id))!.cookie_same_site).toBe(stored);
  });

  test.each([["false"], [null], ['"other"']])("cookie_same_site %p → no SameSite attribute", async (stored) => {
    const { result, user } = await renewedWith("cookie_same_site", stored);
    expect(user?.username).toBe("alice");
    expect(onlySetCookie(result).attributeNames).toEqual(["path", "expires", "httponly"]);
  });

  test("cookie_http_only false or NULL → no HttpOnly attribute", async () => {
    for (const stored of [false, null]) {
      const { result, user } = await renewedWith("cookie_http_only", stored);
      expect(user?.username).toBe("alice");
      expect(onlySetCookie(result).attributeNames).toEqual(["path", "expires", "samesite"]);
    }
  });

  test("cookie_path is sent as stored, and left out when empty or NULL", async () => {
    const custom = await renewedWith("cookie_path", "/api");
    expect(onlySetCookie(custom.result).attributes.path).toBe("/api");
    for (const stored of ["", null]) {
      const { result } = await renewedWith("cookie_path", stored);
      expect(onlySetCookie(result).attributeNames).toEqual(["expires", "httponly", "samesite"]);
    }
  });

  test("cookie_domain is kept in the row but never sent", async () => {
    const { result, id } = await renewedWith("cookie_domain", "mail.test");
    expect(onlySetCookie(result).attributeNames).toEqual(["path", "expires", "httponly", "samesite"]);
    expect((await sessionRow(dev, id))!.cookie_domain).toBe("mail.test");
  });

  test("cookie_signed is kept as stored", async () => {
    const { id } = await renewedWith("cookie_signed", true);
    expect((await sessionRow(dev, id))!.cookie_signed).toBe(true);
  });

  test("a session stored as Secure gets no cookie back over plain HTTP, but stays signed in and is renewed", async () => {
    const { client, id } = await signedIn();
    await dev.db.query(
      "UPDATE sessions SET cookie_secure = 'true', cookie_expires = NOW() + INTERVAL '1 hour' WHERE session_id = $1",
      [id]
    );
    const { result, user } = await whoAmI(client);
    expect(user?.username).toBe("alice");
    expect(result.setCookies).toEqual([]);
    const row = (await sessionRow(dev, id))!;
    expect(isAboutSevenDaysAhead(row.cookie_expires!)).toBe(true);
    expect(row.cookie_secure).toBe("true");
  });

  test("cookie_secure NULL reads as not secure and is left NULL", async () => {
    const { result, id } = await renewedWith("cookie_secure", null);
    expect(onlySetCookie(result).attributeNames).toEqual(["path", "expires", "httponly", "samesite"]);
    expect((await sessionRow(dev, id))!.cookie_secure).toBeNull();
  });

  // NOTE: the two columns hold JSON text, and the store parses them with
  // JSON.parse without guarding it. A value that is not JSON makes every
  // request carrying that cookie a 500, until the row expires.
  test.each([["cookie_secure"], ["cookie_same_site"]])(
    "%s that is not JSON makes requests with that cookie fail with 500",
    async (column) => {
      const { client, id } = await signedIn();
      await dev.db.query(`UPDATE sessions SET ${column} = 'nonsense' WHERE session_id = $1`, [id]);
      const api = await client.get("/api/users/login");
      expect(api.status).toBe(500);
      expect(api.body).toEqual(INTERNAL_ERROR);
      expect(api.setCookies).toEqual([]);
      const page = await client.get("/");
      expect(page.status).toBe(500);
      expect(page.text).toBe("Internal server error");
      expect(page.headers.get("content-type")).toBe("text/plain; charset=utf-8");
      // The row is neither renewed nor removed.
      expect(await sessionRow(dev, id)).toBeDefined();
      await dev.db.query("DELETE FROM sessions WHERE session_id = $1", [id]);
    }
  );
});

describe("a cookie that names no session reads as anonymous", () => {
  const expectAnonymous = async (server: TestServer, cookie: string) => {
    const before = await sessionCount(server);
    const { result } = await whoAmI(server.client(), cookie);
    expect({ cookie, status: result.status, text: result.text, setCookies: result.setCookies }).toEqual({
      cookie,
      status: 200,
      text: ANONYMOUS_LOGIN_TEXT,
      setCookies: [],
    });
    expect(await sessionCount(server)).toBe(before);
  };

  test("a tampered signature", async () => {
    const { client, id } = await signedIn();
    const value = decodeURIComponent(client.cookies.get("connect.sid")!);
    const last = value.slice(-1);
    const flipped = value.slice(0, -1) + (last === "A" ? "B" : "A");
    await expectAnonymous(dev, `connect.sid=${encodeURIComponent(flipped)}`);
    await expectAnonymous(dev, `connect.sid=${encodeURIComponent(`${value}A`)}`);
    await expectAnonymous(dev, `connect.sid=${encodeURIComponent(value.slice(0, -1))}`);
    await expectAnonymous(dev, `connect.sid=${encodeURIComponent(`${value}=`)}`);
    // The session itself is untouched.
    expect((await whoAmI(client)).user?.username).toBe("alice");
    expect(await sessionRow(dev, id)).toBeDefined();
  });

  test("a tampered id under a real signature", async () => {
    const { client } = await signedIn();
    const value = decodeURIComponent(client.cookies.get("connect.sid")!);
    const signature = value.slice(value.lastIndexOf("."));
    const other = await signedIn();
    await expectAnonymous(dev, `connect.sid=${encodeURIComponent(`s:${other.id}${signature}`)}`);
  });

  test("a signature made with another key", async () => {
    const { id } = await signedIn();
    await expectAnonymous(dev, sessionCookie(id, "not-the-secret"));
    await expectAnonymous(dev, sessionCookie(id, "Secret"));
    await expectAnonymous(dev, sessionCookie(id, ""));
  });

  test("an unsigned value", async () => {
    const { id } = await signedIn();
    const signed = signSessionId(id, sessionSecret(dev));
    await expectAnonymous(dev, `connect.sid=${id}`);
    await expectAnonymous(dev, `connect.sid=${encodeURIComponent(`s:${id}`)}`);
    await expectAnonymous(dev, `connect.sid=${encodeURIComponent(`s:${id}.`)}`);
    // Signed, but without the "s:" prefix that marks it as signed.
    await expectAnonymous(dev, `connect.sid=${encodeURIComponent(signed.slice(2))}`);
    await expectAnonymous(dev, `connect.sid=${encodeURIComponent(`S:${signed.slice(2)}`)}`);
  });

  test("a validly signed id that has no row", async () => {
    await expectAnonymous(dev, sessionCookie("no-such-session-0123456789abcdef", sessionSecret(dev)));
    await expectAnonymous(dev, sessionCookie("", sessionSecret(dev)));
    await expectAnonymous(dev, sessionCookie("x", sessionSecret(dev)));
    await expectAnonymous(dev, sessionCookie("id with spaces; and = signs", sessionSecret(dev)));
    await expectAnonymous(dev, sessionCookie("ünïcödé-🔑", sessionSecret(dev)));
    await expectAnonymous(dev, sessionCookie("a".repeat(3000), sessionSecret(dev)));
  });

  test("garbage", async () => {
    for (const cookie of [
      "connect.sid=",
      "connect.sid",
      "connect.sid=%",
      "connect.sid=%E0%A4%A",
      "connect.sid=s%3A",
      "connect.sid=s%3A.",
      "connect.sid=s:",
      "connect.sid=.",
      'connect.sid=""',
      'connect.sid="',
      "connect.sid=;;;",
      "=",
      ";",
      "connect.sid==s%3Aa.b",
      `connect.sid=${"z".repeat(4000)}`,
      "connect.sid=s%3A%00.%00",
      "connect.sid=null",
      "connect.sid=undefined",
    ]) {
      await expectAnonymous(dev, cookie);
    }
  });

  test("the cookie of another server, signed with another secret", async () => {
    const { client } = await signedIn(prod);
    await expectAnonymous(dev, `connect.sid=${client.cookies.get("connect.sid")}`);
  });

  test("a cookie under another name", async () => {
    const { client } = await signedIn();
    const value = client.cookies.get("connect.sid")!;
    for (const name of ["Connect.sid", "CONNECT.SID", "connect_sid", "xconnect.sid", "connect.sidx", "sid"]) {
      await expectAnonymous(dev, `${name}=${value}`);
    }
  });
});

describe("how the Cookie header is read", () => {
  let value: string;
  let id: string;
  beforeAll(async () => {
    const session = await signedIn();
    value = session.client.cookies.get("connect.sid")!;
    id = session.id;
  });

  const user = async (cookie: string) => (await whoAmI(dev.client(), cookie)).user?.username;

  test("among other cookies, wherever it stands", async () => {
    expect(await user(`theme=dark; connect.sid=${value}; lang=en`)).toBe("alice");
    expect(await user(`connect.sid=${value}; theme=dark`)).toBe("alice");
    expect(await user(`a=1;connect.sid=${value};b=2`)).toBe("alice");
    expect(await user(`a; b; =c; connect.sid=${value}`)).toBe("alice");
  });

  test("with spaces around the name and the value", async () => {
    expect(await user(`theme=dark;   connect.sid = ${value}  ; lang=en`)).toBe("alice");
  });

  test("in double quotes", async () => {
    expect(await user(`connect.sid="${value}"`)).toBe("alice");
  });

  test("not percent-encoded at all", async () => {
    const decoded = decodeURIComponent(value);
    expect(decoded).toBe(signSessionId(id, sessionSecret(dev)));
    expect(await user(`connect.sid=${decoded}`)).toBe("alice");
  });

  test("percent-encoded more than it needs to be", async () => {
    const everyByte = [...new TextEncoder().encode(decodeURIComponent(value))]
      .map((byte) => `%${byte.toString(16).padStart(2, "0").toUpperCase()}`)
      .join("");
    expect(await user(`connect.sid=${everyByte}`)).toBe("alice");
    expect(await user(`connect.sid=${everyByte.toLowerCase()}`)).toBe("alice");
  });

  test("encoded twice it is not a signed value any more", async () => {
    expect(await user(`connect.sid=${encodeURIComponent(value)}`)).toBeUndefined();
  });

  test("when it is repeated, the first one counts", async () => {
    expect(await user(`connect.sid=${value}; connect.sid=garbage`)).toBe("alice");
    expect(await user(`connect.sid=garbage; connect.sid=${value}`)).toBeUndefined();
    expect(await user(`connect.sid=; connect.sid=${value}`)).toBeUndefined();
  });

  test("when the header itself is repeated", async () => {
    const ask = async (cookieLines: string[]) => {
      const head =
        "GET /api/users/login HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n" +
        cookieLines.map((line) => `Cookie: ${line}\r\n`).join("") +
        "\r\n";
      const response = await rawHttp(dev.ports.http, head);
      expect(response.status).toBe(200);
      return JSON.parse(response.body).body.user?.username as string | undefined;
    };
    expect(await ask([`connect.sid=${value}`])).toBe("alice");
    const firstLine = await ask([`connect.sid=${value}`, "theme=dark"]);
    const secondLine = await ask(["theme=dark", `connect.sid=${value}`]);
    expect({ firstLine, secondLine }).toEqual({ firstLine: "alice", secondLine: "alice" });
  });
});

describe("a session row that no longer counts", () => {
  test("an expired row reads as anonymous and is deleted", async () => {
    const { client, id } = await signedIn();
    await dev.db.query("UPDATE sessions SET cookie_expires = NOW() - INTERVAL '1 second' WHERE session_id = $1", [id]);
    const { result } = await whoAmI(client);
    expect(result.text).toBe(ANONYMOUS_LOGIN_TEXT);
    expect(result.setCookies).toEqual([]);
    await waitFor(async () => !(await sessionRow(dev, id)), { what: "the expired row to be deleted" });
    // Nothing takes its place.
    expect(await sessionRow(dev, id)).toBeUndefined();
  });

  test("a row without an expiry counts as expired", async () => {
    const { client, id } = await signedIn();
    await dev.db.query("UPDATE sessions SET cookie_expires = NULL WHERE session_id = $1", [id]);
    expect((await whoAmI(client)).result.text).toBe(ANONYMOUS_LOGIN_TEXT);
    await waitFor(async () => !(await sessionRow(dev, id)), { what: "the row to be deleted" });
  });

  test("a row about to expire still counts", async () => {
    const { client, id } = await signedIn();
    await dev.db.query("UPDATE sessions SET cookie_expires = NOW() + INTERVAL '30 seconds' WHERE session_id = $1", [id]);
    expect((await whoAmI(client)).user?.username).toBe("alice");
    expect(isAboutSevenDaysAhead((await sessionRow(dev, id))!.cookie_expires!)).toBe(true);
  });

  test("a row stamped with an older credential generation is rejected and deleted", async () => {
    const bob = await createUser(dev, "bob");
    const { client, id } = await signedIn(dev, "bob");
    await dev.db.query("UPDATE users SET password_version = 2 WHERE user_id = $1", [bob.id]);
    const { result } = await whoAmI(client);
    expect(result.text).toBe(ANONYMOUS_LOGIN_TEXT);
    expect(result.setCookies).toEqual([]);
    // Deleted before the response, not behind it.
    expect(await sessionRow(dev, id)).toBeUndefined();

    // A session issued now carries the new generation and is accepted.
    const fresh = await signedIn(dev, "bob");
    expect((await sessionRow(dev, fresh.id))!.session_password_version).toBe(2);
    expect((await whoAmI(fresh.client)).user?.passwordVersion).toBe(2);
  });

  test("an unstamped row is generation 1, and so is an unstamped user", async () => {
    const carol = await createUser(dev, "carol");
    const one = await signedIn(dev, "carol");
    await dev.db.query("UPDATE sessions SET session_password_version = NULL WHERE session_id = $1", [one.id]);
    expect((await whoAmI(one.client)).user).toEqual({
      id: carol.id,
      email: carol.email,
      username: "carol",
      passwordVersion: 1,
    });
    // The renewal stamps the generation it read.
    expect((await sessionRow(dev, one.id))!.session_password_version).toBe(1);

    await dev.db.query("UPDATE users SET password_version = NULL WHERE user_id = $1", [carol.id]);
    expect((await whoAmI(one.client)).user?.username).toBe("carol");

    await dev.db.query("UPDATE sessions SET session_password_version = NULL WHERE session_id = $1", [one.id]);
    await dev.db.query("UPDATE users SET password_version = 2 WHERE user_id = $1", [carol.id]);
    expect((await whoAmI(one.client)).user).toBeUndefined();
    expect(await sessionRow(dev, one.id)).toBeUndefined();
  });

  test("a row stamped ahead of the user's generation is accepted", async () => {
    await createUser(dev, "dave");
    const { client, id } = await signedIn(dev, "dave");
    await dev.db.query("UPDATE sessions SET session_password_version = 9 WHERE session_id = $1", [id]);
    expect((await whoAmI(client)).user?.passwordVersion).toBe(9);
  });

  test("a session whose user row is deleted is rejected and deleted", async () => {
    const erin = await createUser(dev, "erin");
    const { client, id } = await signedIn(dev, "erin");
    const other = await signedIn(dev, "alice");
    await dev.db.query("DELETE FROM users WHERE user_id = $1", [erin.id]);

    const { result } = await whoAmI(client);
    expect(result.text).toBe(ANONYMOUS_LOGIN_TEXT);
    expect(result.setCookies).toEqual([]);
    // Deleted before the response, not behind it.
    expect(await sessionRow(dev, id)).toBeUndefined();
    // Signed out everywhere, not only on this route.
    const gated = await client.get("/api/mails/accounts");
    expect({ status: gated.status, body: gated.body }).toEqual({ status: 401, body: NOT_LOGGED_IN });
    // Another user's session is left alone.
    expect((await whoAmI(other.client)).user?.username).toBe("alice");
  });

  test("a session whose user is soft-deleted is rejected and deleted, and does not come back with the user", async () => {
    const frank = await createUser(dev, "frank");
    const { client, id } = await signedIn(dev, "frank");
    await dev.db.query("UPDATE users SET is_deleted = TRUE WHERE user_id = $1", [frank.id]);

    const { result } = await whoAmI(client);
    expect(result.text).toBe(ANONYMOUS_LOGIN_TEXT);
    expect(result.setCookies).toEqual([]);
    expect(await sessionRow(dev, id)).toBeUndefined();

    // Restoring the account restores no session: signing in again does.
    await dev.db.query("UPDATE users SET is_deleted = FALSE WHERE user_id = $1", [frank.id]);
    expect((await whoAmI(client)).user).toBeUndefined();
    const again = await signedIn(dev, "frank");
    expect((await whoAmI(again.client)).user?.username).toBe("frank");
  });

  test("a read-only session ends with admin's row, which it is a view onto", async () => {
    const ro = await startServer({ env: { ADMIN_RO_PASSWORD: "read-only-password-e2e" } });
    try {
      const client = ro.client();
      await client.login("admin-ro", "read-only-password-e2e");
      const id = sessionIdOf(client);
      expect((await whoAmI(client)).user?.username).toBe("admin");

      await ro.db.query("UPDATE users SET is_deleted = TRUE WHERE username = 'admin'");
      expect((await whoAmI(client)).user).toBeUndefined();
      expect(await sessionRow(ro, id)).toBeUndefined();
    } finally {
      await ro.stop();
    }
  }, 90_000);
});

describe("a request without a session", () => {
  test("gets no cookie and writes no row, whatever it asks for", async () => {
    const before = await sessionCount(dev);
    const client = dev.client();
    const requests: [string, string, unknown?][] = [
      ["GET", "/api/users/login"],
      ["GET", "/api/ping"],
      ["GET", "/api/health"],
      ["GET", "/api/push/public-key"],
      ["GET", "/api/nothing-here"],
      ["GET", "/"],
      ["GET", "/manifest.json"],
      ["DELETE", "/api/users/login"],
      ["POST", "/api/users/login", { username: 1, password: 2 }],
      ["POST", "/api/push/subscribe", {}],
      ["POST", "/nowhere"],
    ];
    for (const [method, path, json] of requests) {
      const result = await client.request(method, path, json === undefined ? {} : { json });
      expect({ path, setCookies: result.setCookies }).toEqual({ path, setCookies: [] });
    }
    expect(client.cookies.size).toBe(0);
    expect(await sessionCount(dev)).toBe(before);
  });
});

describe("the Secure cookie in production", () => {
  const login = (client: ApiClient, headers: Record<string, string> = {}) =>
    client.request("POST", "/api/users/login", {
      json: { username: "alice", password: "password-of-alice" },
      headers,
    });

  test("is set when a trusted proxy says the request arrived over HTTPS", async () => {
    const result = await login(prod.client());
    expect(result.body.status).toBe("success");
    expectSessionCookie(onlySetCookie(result), { secure: true });
  });

  test("is withheld from a request that arrived over plain HTTP, though the session exists", async () => {
    const before = await sessionCount(prod);
    const result = await login(prod.client({ forwarded: false }));
    expect(result.status).toBe(200);
    expect(result.body.status).toBe("success");
    expect(result.setCookies).toEqual([]);
    expect(await sessionCount(prod)).toBe(before + 1);
    const [row] = await prod.db.query("SELECT * FROM sessions ORDER BY updated DESC LIMIT 1");
    expect(row).toMatchObject({ session_username: "alice", cookie_secure: "true", cookie_same_site: '"strict"' });
    expect(row!.session_id).toMatch(SESSION_ID);
  });

  test("is withheld when the forwarded protocol is anything but https first", async () => {
    for (const proto of ["http", "http, https", "HTTPS", "https2", "wss", "on"]) {
      const result = await login(prod.client(), { "X-Forwarded-Proto": proto });
      expect({ proto, status: result.body.status, cookies: result.setCookies.length }).toEqual({
        proto,
        status: "success",
        cookies: 0,
      });
    }
  });

  test("is set when https is the first of several forwarded protocols", async () => {
    for (const proto of ["https, http", "https,http", "https ,http"]) {
      const result = await login(prod.client(), { "X-Forwarded-Proto": proto });
      expect({ proto, cookies: result.setCookies.length }).toEqual({ proto, cookies: 1 });
    }
  });

  test("a live session keeps working over plain HTTP, renewed but with no cookie sent back", async () => {
    const { client, id } = await signedIn(prod);
    await prod.db.query("UPDATE sessions SET cookie_expires = NOW() + INTERVAL '1 hour' WHERE session_id = $1", [id]);
    const plain = prod.client({ forwarded: false });
    const result = await plain.get("/api/users/login", {
      headers: { Cookie: `connect.sid=${client.cookies.get("connect.sid")}` },
    });
    expect(result.body.body.user.username).toBe("alice");
    expect(result.setCookies).toEqual([]);
    expect(isAboutSevenDaysAhead((await sessionRow(prod, id))!.cookie_expires!)).toBe(true);
  });

  test("development ignores the forwarded protocol and always sets a plain cookie", async () => {
    const variants: Record<string, string>[] = [{}, { "X-Forwarded-Proto": "https" }, { "X-Forwarded-Proto": "http" }];
    for (const headers of variants) {
      const result = await dev.client().request("POST", "/api/users/login", {
        json: { username: "alice", password: "password-of-alice" },
        headers,
      });
      expectSessionCookie(onlySetCookie(result), { secure: false });
    }
  });
});

describe("the signing secret", () => {
  test("is a fixed, published value when SECRET is unset outside production", async () => {
    expect(dev.env.SECRET).toBeUndefined();
    const { client, id } = await signedIn();
    expect(decodeURIComponent(client.cookies.get("connect.sid")!)).toBe(signSessionId(id, "secret"));
  });

  test("is SECRET as given, surrounding whitespace included", async () => {
    const secret = "  a secret with spaces around it 0123456789  ";
    const spaced = await startServer({ env: { SECRET: secret } });
    try {
      const client = spaced.client();
      await client.login("admin", ADMIN_PASSWORD);
      const id = sessionIdOf(client);
      expect(decodeURIComponent(client.cookies.get("connect.sid")!)).toBe(signSessionId(id, secret));

      const who = async (cookie: string) =>
        (await spaced.client().get("/api/users/login", { headers: { Cookie: cookie } })).body.body.user?.username;
      expect(await who(sessionCookie(id, secret))).toBe("admin");
      expect(await who(sessionCookie(id, secret.trim()))).toBeUndefined();
      expect(await who(sessionCookie(id, "secret"))).toBeUndefined();
    } finally {
      await spaced.stop();
    }
  }, 90_000);

  test("an empty SECRET outside production is the published value too", async () => {
    const empty = await startServer({ env: { SECRET: "" } });
    try {
      const client = empty.client();
      await client.login("admin", ADMIN_PASSWORD);
      expect(decodeURIComponent(client.cookies.get("connect.sid")!)).toBe(signSessionId(sessionIdOf(client), "secret"));
    } finally {
      await empty.stop();
    }
  }, 90_000);

  test("a whitespace-only SECRET outside production is the published value as well", async () => {
    const blank = await startServer({ env: { SECRET: "   " } });
    try {
      const client = blank.client();
      await client.login("admin", ADMIN_PASSWORD);
      expect(decodeURIComponent(client.cookies.get("connect.sid")!)).toBe(signSessionId(sessionIdOf(client), "secret"));
    } finally {
      await blank.stop();
    }
  }, 90_000);

  test("sessions survive a restart with the same secret and not one with another", async () => {
    const server = await startServer({ env: { SECRET: "the-first-secret-0123456789abcdefgh" } });
    try {
      const admin = await adminUser(server);
      const client = server.client();
      await client.login("admin", ADMIN_PASSWORD);
      const id = sessionIdOf(client);

      await server.restart();
      expect((await client.get("/api/users/login")).body.body.user.id).toBe(admin.id);

      await server.restart({ SECRET: "the-second-secret-0123456789abcdefg" });
      const after = await client.get("/api/users/login");
      expect(after.text).toBe(ANONYMOUS_LOGIN_TEXT);
      // The row is still there; only the signature stopped verifying.
      expect(await sessionRow(server, id)).toBeDefined();
      const resigned = await server.client().get("/api/users/login", {
        headers: { Cookie: sessionCookie(id, "the-second-secret-0123456789abcdefg") },
      });
      expect(resigned.body.body.user.id).toBe(admin.id);
    } finally {
      await server.stop();
    }
  }, 120_000);
});

describe("raw requests", () => {
  test("a session cookie works over a bare connection", async () => {
    const { client, id } = await signedIn();
    const response = await rawHttp(
      dev.ports.http,
      rawRequest("GET", "/api/users/login", { Cookie: `connect.sid=${client.cookies.get("connect.sid")}` })
    );
    expect(response.status).toBe(200);
    expect(JSON.parse(response.body).body.user.username).toBe("alice");
    expect(sessionIdOfValue(parseSetCookie(response.headers["set-cookie"]!).value)).toBe(id);
  });
});
