/**
 * `GET /api/auth/login`: the redirect to the provider, the login cookie that
 * carries the sign-in between its two requests, and what happens when the
 * provider cannot be asked where to send the browser.
 */
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import crypto from "crypto";
import { ADMIN_PASSWORD, startServer, waitFor, type ApiResult, type TestServer } from "../harness";
import {
  ISSUER,
  LOGIN_COOKIE,
  decodeLoginCookie,
  jsonReply,
  startIssuer,
  startSignIn,
  type FakeIssuer,
} from "../harness/issuer";

const DISCOVERY = "/.well-known/openid-configuration";
const B64URL_32_BYTES = /^[A-Za-z0-9_-]{43}$/;

const loginCookieOf = (result: ApiResult) =>
  result.setCookies.find((cookie) => cookie.startsWith(`${LOGIN_COOKIE}=`));

/** The `returnTo` the server kept for a sign-in started with this query string. */
const keptReturnTo = async (server: TestServer, query: string) => {
  const result = await server.client().get(`/api/auth/login${query}`);
  expect(result.status).toBe(302);
  const cookie = loginCookieOf(result);
  expect(cookie).toBeDefined();
  return decodeLoginCookie(cookie!.split(";")[0]!.slice(LOGIN_COOKIE.length + 1)).returnTo;
};

describe("GET /api/auth/login", () => {
  let issuer: FakeIssuer;
  let server: TestServer;

  beforeAll(async () => {
    issuer = await startIssuer();
    server = await startServer({ env: issuer.env({ AUTH_LOCAL_LOGIN: "true" }) });
  }, 90_000);
  afterAll(async () => {
    await server?.stop();
    await issuer?.stop();
  });

  test("redirects to the provider's authorization endpoint with the code-flow parameters", async () => {
    const before = Math.floor(Date.now() / 1000);
    const result = await server.client().get("/api/auth/login");
    const after = Math.floor(Date.now() / 1000);

    expect(result.status).toBe(302);
    expect(result.text).toBe("");
    expect(result.headers.get("cache-control")).toBe("no-store");
    expect(result.headers.get("content-type")).toBeNull();
    expect(result.headers.get("x-frame-options")).toBe("DENY");

    const location = result.headers.get("location")!;
    const url = new URL(location);
    expect(url.origin + url.pathname).toBe(`${ISSUER}/oauth/authorize`);
    expect(url.hash).toBe("");
    expect([...url.searchParams.keys()]).toEqual([
      "response_type",
      "client_id",
      "redirect_uri",
      "scope",
      "state",
      "nonce",
      "code_challenge",
      "code_challenge_method",
    ]);
    const params = Object.fromEntries(url.searchParams);
    expect(params.response_type).toBe("code");
    expect(params.client_id).toBe("mail");
    expect(params.redirect_uri).toBe("https://app.mail.test/api/auth/callback");
    expect(params.scope).toBe("openid profile email");
    expect(params.code_challenge_method).toBe("S256");
    expect(params.state).toMatch(B64URL_32_BYTES);
    expect(params.nonce).toMatch(B64URL_32_BYTES);
    expect(params.code_challenge).toMatch(B64URL_32_BYTES);
    expect(new Set([params.state, params.nonce, params.code_challenge]).size).toBe(3);
    // The query is form-encoded: a space is a plus, reserved characters are escaped.
    expect(location).toContain("redirect_uri=https%3A%2F%2Fapp.mail.test%2Fapi%2Fauth%2Fcallback&scope=openid+profile+email&");

    // One cookie: the login cookie. No session is started.
    expect(result.setCookies).toHaveLength(1);
    const cookie = result.setCookies[0]!;
    const [pair, ...attributes] = cookie.split("; ");
    expect(attributes).toEqual(["Path=/", "Secure", "HttpOnly", "SameSite=Lax", "Max-Age=300"]);
    const value = pair!.slice(`${LOGIN_COOKIE}=`.length);
    expect(pair!.startsWith(`${LOGIN_COOKIE}=`)).toBe(true);
    expect(value).toMatch(/^[A-Za-z0-9_-]+$/);

    const saved = decodeLoginCookie(value);
    expect(Object.keys(saved)).toEqual(["state", "nonce", "verifier", "returnTo", "iat", "fresh"]);
    expect(saved.state).toBe(params.state);
    expect(saved.nonce).toBe(params.nonce);
    expect(saved.verifier).toMatch(B64URL_32_BYTES);
    expect(crypto.createHash("sha256").update(saved.verifier!).digest("base64url")).toBe(params.code_challenge);
    expect(saved.returnTo).toBe("/");
    expect(saved.fresh).toBe(false);
    expect(Number.isInteger(saved.iat)).toBe(true);
    expect(saved.iat).toBeGreaterThanOrEqual(before);
    expect(saved.iat).toBeLessThanOrEqual(after);
    // The cookie is exactly that JSON, base64url without padding.
    expect(Buffer.from(JSON.stringify(saved)).toString("base64url")).toBe(value);

    expect(await server.db.query("SELECT 1 FROM sessions")).toEqual([]);
  });

  test("every sign-in gets its own state, nonce and verifier", async () => {
    const first = await startSignIn(server.client());
    const second = await startSignIn(server.client());
    for (const field of ["state", "nonce", "verifier"] as const) {
      expect(decodeLoginCookie(first.loginCookie)[field]).not.toBe(decodeLoginCookie(second.loginCookie)[field]);
    }
    expect(first.authorizeUrl.searchParams.get("code_challenge")).not.toBe(
      second.authorizeUrl.searchParams.get("code_challenge")
    );
  });

  test("asks the provider for its discovery document once, with no credentials", async () => {
    await startSignIn(server.client());
    await startSignIn(server.client());
    const calls = issuer.seen(DISCOVERY);
    expect(calls).toHaveLength(1);
    expect(calls[0]!.method).toBe("GET");
    expect(calls[0]!.headers.get("accept")).toBe("application/json");
    expect(calls[0]!.headers.get("authorization")).toBeNull();
    expect(calls[0]!.headers.get("cookie")).toBeNull();
    // Starting a sign-in calls nothing else.
    expect(issuer.calls.every((call) => call.path === DISCOVERY)).toBe(true);
  });

  test("keeps a local path to return to, query and fragment included", async () => {
    for (const path of [
      "/mail/42",
      "/",
      "/mail/42?view=thread&x=1#message-3",
      "/a/b/../c",
      "/search?q=a%20b",
      "/%E2%9C%93",
      "/mail/é",
      "/x//y",
      "/@user:1",
      `/${"a".repeat(2047)}`,
    ]) {
      expect({ path, kept: await keptReturnTo(server, `?returnTo=${encodeURIComponent(path)}`) }).toEqual({
        path,
        kept: path,
      });
    }
  });

  test("replaces anything that is not a local path with the root", async () => {
    for (const value of [
      "https://evil.example/",
      "http://app.mail.test/mail",
      "//evil.example/mail",
      "///evil.example",
      "/\\evil.example",
      "/mail\\42",
      "\\/evil.example",
      "mail/42",
      "",
      " /mail",
      "/mail 42",
      "/mail\t42",
      "/mail\n42",
      "/mail\u0000",
      "/mail\u007f",
      "javascript:alert(1)",
      `/${"a".repeat(2048)}`,
    ]) {
      expect({ value, kept: await keptReturnTo(server, `?returnTo=${encodeURIComponent(value)}`) }).toEqual({
        value,
        kept: "/",
      });
    }
    // A plus in a query string is a space.
    expect(await keptReturnTo(server, "?returnTo=/mail+42")).toBe("/");
    // No parameter at all, or one under another name.
    expect(await keptReturnTo(server, "")).toBe("/");
    expect(await keptReturnTo(server, "?return_to=%2Fmail")).toBe("/");
    expect(await keptReturnTo(server, "?returnto=%2Fmail")).toBe("/");
  });

  test("takes the first of a repeated returnTo", async () => {
    expect(await keptReturnTo(server, "?returnTo=%2Ffirst&returnTo=%2Fsecond")).toBe("/first");
    expect(await keptReturnTo(server, "?returnTo=https%3A%2F%2Fevil.example&returnTo=%2Fsecond")).toBe("/");
    expect(await keptReturnTo(server, "?returnTo=%2Ffirst&returnTo=https%3A%2F%2Fevil.example")).toBe("/first");
    expect(await keptReturnTo(server, "?other=1&returnTo=%2Fmail&state=x&code=y")).toBe("/mail");
  });

  test("answers in any letter case, with a trailing slash, and to HEAD", async () => {
    for (const path of ["/api/auth/login/", "/API/AUTH/LOGIN", "/Api/Auth/Login/?returnTo=%2Fmail"]) {
      const result = await server.client().get(path);
      expect({ path, status: result.status }).toEqual({ path, status: 302 });
      expect(new URL(result.headers.get("location")!).origin).toBe(ISSUER);
      expect(loginCookieOf(result)).toBeDefined();
    }
    const head = await server.client().head("/api/auth/login");
    expect(head.status).toBe(302);
    expect(new URL(head.headers.get("location")!).origin).toBe(ISSUER);
    expect(loginCookieOf(head)).toBeDefined();
  });

  test("ignores a login cookie the browser already holds, and replaces it", async () => {
    const client = server.client();
    const first = await startSignIn(client, "/first");
    const second = await startSignIn(client, "/second");
    expect(second.loginCookie).not.toBe(first.loginCookie);
    expect(decodeLoginCookie(second.loginCookie).returnTo).toBe("/second");
    expect(client.cookies.get(LOGIN_COOKIE)).toBe(second.loginCookie);
  });

  test("starts a sign-in for a browser that is signed in already, leaving its session alone", async () => {
    const client = server.client();
    await client.login("admin", ADMIN_PASSWORD);
    const [before] = await server.db.query("SELECT session_id FROM sessions");

    const result = await client.get("/api/auth/login?returnTo=%2Fmail");
    expect(result.status).toBe(302);
    expect(new URL(result.headers.get("location")!).origin).toBe(ISSUER);
    // The login cookie, then the session cookie every request of a live session renews.
    expect(result.setCookies.map((cookie) => cookie.split("=")[0])).toEqual([LOGIN_COOKIE, "connect.sid"]);

    const rows = await server.db.query("SELECT session_id, session_username, auth_sid FROM sessions");
    expect(rows).toEqual([{ session_id: before!.session_id, session_username: "admin", auth_sid: null }]);
    expect((await client.get("/api/users/login")).body.body.user.username).toBe("admin");
    await client.delete("/api/users/login");
  });
});

describe("GET /api/auth/login when the provider cannot say where to go", () => {
  let issuer: FakeIssuer;
  let server: TestServer;

  beforeAll(async () => {
    issuer = await startIssuer();
    server = await startServer({ env: issuer.env() });
  }, 90_000);
  afterAll(async () => {
    await server?.stop();
    await issuer?.stop();
  });

  const expectUnavailable = (result: ApiResult) => {
    expect(result.status).toBe(302);
    expect(result.headers.get("location")).toBe("/sign-in?sso_error=unavailable");
    expect(result.headers.get("cache-control")).toBe("no-store");
    expect(result.setCookies).toEqual([]);
    expect(result.text).toBe("");
  };

  test("an unreachable provider lands on the sign-in page with `unavailable`", async () => {
    await issuer.down();
    try {
      expectUnavailable(await server.client().get("/api/auth/login?returnTo=%2Fmail"));
      // And again: nothing is remembered about the failure.
      expectUnavailable(await server.client().get("/api/auth/login"));
    } finally {
      issuer.up();
    }
  });

  test("so does a provider that fails, or answers something that is not a discovery document", async () => {
    const answers: [string, () => Response][] = [
      ["500", () => jsonReply(500, { error: "server_error" })],
      ["503", () => new Response("Service Unavailable", { status: 503 })],
      ["404", () => jsonReply(404, { error: "not_found" })],
      ["401", () => jsonReply(401, { error: "unauthorized" })],
      ["302", () => new Response(null, { status: 302, headers: { Location: "/elsewhere" } })],
      ["text", () => new Response("<html>It works!</html>", { status: 200 })],
      ["empty", () => new Response("", { status: 200 })],
      ["array", () => jsonReply(200, [])],
      ["string", () => jsonReply(200, "a string")],
      ["null", () => jsonReply(200, null)],
    ];
    for (const [label, answer] of answers) {
      issuer.clearCalls();
      issuer.replyOnce(DISCOVERY, answer);
      const result = await server.client().get("/api/auth/login");
      expect({ label, location: result.headers.get("location") }).toEqual({
        label,
        location: "/sign-in?sso_error=unavailable",
      });
      expectUnavailable(result);
      // A redirect is not followed: one call, to the discovery document only.
      expect(issuer.calls.map((call) => call.path)).toEqual([DISCOVERY]);
    }
    expect(server.alarms.alarms).toEqual([]);
  });

  test("a provider that does not answer within five seconds counts as unreachable", async () => {
    const release = issuer.hold(DISCOVERY);
    try {
      const started = Date.now();
      const result = await server.client().get("/api/auth/login");
      const elapsed = Date.now() - started;
      expectUnavailable(result);
      expect(elapsed).toBeGreaterThanOrEqual(4_500);
      expect(elapsed).toBeLessThan(15_000);
    } finally {
      release();
    }
  }, 40_000);

  test("the next sign-in asks again and goes through once the provider answers", async () => {
    issuer.clearCalls();
    const { authorizeUrl } = await startSignIn(server.client());
    expect(authorizeUrl.origin).toBe(ISSUER);
    expect(issuer.seen(DISCOVERY)).toHaveLength(1);
  });

  test("with the document in hand, a sign-in starts while the provider is down", async () => {
    await startSignIn(server.client());
    await issuer.down();
    try {
      const { authorizeUrl } = await startSignIn(server.client());
      expect(authorizeUrl.origin).toBe(ISSUER);
    } finally {
      issuer.up();
    }
  });
});

describe("GET /api/auth/login with a discovery document that cannot be used", () => {
  const start = async (overrides: Record<string, unknown>) => {
    const issuer = await startIssuer();
    Object.assign(issuer.discoveryOverrides, overrides);
    const server = await startServer({ env: issuer.env() });
    return { issuer, server, stop: async () => void (await server.stop(), await issuer.stop()) };
  };

  test("a document naming another issuer lands on `failed`, and alarms", async () => {
    const { issuer, server, stop } = await start({ issuer: "https://someone-else.e2e.test" });
    try {
      const result = await server.client().get("/api/auth/login?returnTo=%2Fmail");
      expect(result.status).toBe(302);
      expect(result.headers.get("location")).toBe("/sign-in?sso_error=failed");
      expect(result.headers.get("cache-control")).toBe("no-store");
      expect(result.setCookies).toEqual([]);

      const alarm = await waitFor(() => server.alarms.alarms[0], { what: "the sign-on alarm" });
      expect(alarm.title).toBe("🚨 Mail Server Error: Single sign-on failed");
      expect(alarm.description).toBe("**Error:** the discovery document names a different issuer");

      // The refused document is not kept: the next sign-in fetches it again.
      delete issuer.discoveryOverrides.issuer;
      const { authorizeUrl } = await startSignIn(server.client());
      expect(authorizeUrl.origin).toBe(ISSUER);
      expect(issuer.seen(DISCOVERY)).toHaveLength(2);
    } finally {
      await stop();
    }
  }, 90_000);

  test("the issuer must match to the letter: a trailing slash is another issuer", async () => {
    const { server, stop } = await start({ issuer: `${ISSUER}/` });
    try {
      const result = await server.client().get("/api/auth/login");
      expect(result.headers.get("location")).toBe("/sign-in?sso_error=failed");
    } finally {
      await stop();
    }
  }, 90_000);

  for (const field of ["authorization_endpoint", "token_endpoint", "jwks_uri"]) {
    test(`a document without ${field} lands on \`failed\``, async () => {
      const { server, stop } = await start({ [field]: undefined });
      try {
        const result = await server.client().get("/api/auth/login");
        expect(result.headers.get("location")).toBe("/sign-in?sso_error=failed");
        expect(result.setCookies).toEqual([]);
        const alarm = await waitFor(() => server.alarms.alarms[0], { what: "the sign-on alarm" });
        expect(alarm.description).toBe(`**Error:** the discovery document has no ${field}`);
      } finally {
        await stop();
      }
    }, 90_000);
  }

  test("a document without the optional endpoints is enough to start a sign-in", async () => {
    const { server, stop } = await start({
      revocation_endpoint: undefined,
      introspection_endpoint: undefined,
      end_session_endpoint: undefined,
    });
    try {
      const { authorizeUrl } = await startSignIn(server.client());
      expect(authorizeUrl.origin).toBe(ISSUER);
    } finally {
      await stop();
    }
  }, 90_000);

  test("an authorization endpoint that is not a URL lands on `failed`", async () => {
    const { server, stop } = await start({ authorization_endpoint: "/oauth/authorize" });
    try {
      const result = await server.client().get("/api/auth/login");
      expect(result.headers.get("location")).toBe("/sign-in?sso_error=failed");
    } finally {
      await stop();
    }
  }, 90_000);

  test("parameters the authorization endpoint already carries are kept, and replaced where they clash", async () => {
    const { server, stop } = await start({
      authorization_endpoint: `${ISSUER}/oauth/authorize?tenant=a%20b&scope=legacy&x=1&scope=again#top`,
    });
    try {
      const result = await server.client().get("/api/auth/login");
      const location = result.headers.get("location")!;
      const url = new URL(location);
      expect(url.hash).toBe("#top");
      expect([...url.searchParams]).toEqual([
        ["tenant", "a b"],
        ["scope", "openid profile email"],
        ["x", "1"],
        ["response_type", "code"],
        ["client_id", "mail"],
        ["redirect_uri", "https://app.mail.test/api/auth/callback"],
        ["state", expect.any(String)],
        ["nonce", expect.any(String)],
        ["code_challenge", expect.any(String)],
        ["code_challenge_method", "S256"],
      ]);
      expect(location.startsWith(`${ISSUER}/oauth/authorize?tenant=a+b&scope=openid+profile+email&x=1&response_type=code&`)).toBe(true);
    } finally {
      await stop();
    }
  }, 90_000);
});

describe("GET /api/auth/login with settings that need escaping", () => {
  test("the client id and redirect URI are form-encoded into the redirect", async () => {
    const issuer = await startIssuer({ clientId: "mail app/1+é&=?" });
    const server = await startServer({
      env: issuer.env({ AUTH_REDIRECT_URI: "https://app.mail.test/api/auth/callback?from=sso&x=a b" }),
    });
    try {
      const result = await server.client().get("/api/auth/login");
      const location = result.headers.get("location")!;
      const url = new URL(location);
      expect(url.searchParams.get("client_id")).toBe("mail app/1+é&=?");
      expect(url.searchParams.get("redirect_uri")).toBe("https://app.mail.test/api/auth/callback?from=sso&x=a b");
      expect(location).toContain(
        "client_id=mail+app%2F1%2B%C3%A9%26%3D%3F&redirect_uri=https%3A%2F%2Fapp.mail.test%2Fapi%2Fauth%2Fcallback%3Ffrom%3Dsso%26x%3Da+b&"
      );
    } finally {
      await server.stop();
      await issuer.stop();
    }
  }, 90_000);
});
