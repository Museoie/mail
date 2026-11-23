/**
 * `GET /api/auth/callback`: the session a completed sign-in starts, and the
 * code every failure lands on the sign-in page with. Which local account a
 * sign-in gets is `sso-accounts.e2e.ts`.
 */
import { afterAll, beforeAll, beforeEach, describe, expect, test } from "bun:test";
import crypto from "crypto";
import { ADMIN_PASSWORD, startServer, waitFor, type ApiClient, type ApiResult, type TestServer } from "../harness";
import {
  LOGIN_COOKIE,
  decodeLoginCookie,
  encodeLoginCookie,
  formEncode,
  jsonReply,
  person,
  signIn,
  startIssuer,
  startSignIn,
  type FakeIssuer,
  type GrantOptions,
  type LoginCookieState,
} from "../harness/issuer";

const CLEAR_LOGIN_COOKIE = "__Host-login=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0";
const SESSION_COOKIE = /^connect\.sid=s%3A([A-Za-z0-9_-]{32})\.[A-Za-z0-9%]+; Path=\/; Expires=[A-Z][a-z]{2}, \d{2} [A-Z][a-z]{2} \d{4} \d{2}:\d{2}:\d{2} GMT; HttpOnly; SameSite=Strict$/;
const TEN_MINUTES = 10 * 60 * 1000;

const alice = person("alice");
const nowS = () => Math.floor(Date.now() / 1000);

const sessionRows = (server: TestServer) => server.db.query("SELECT * FROM sessions ORDER BY updated");
const userRows = (server: TestServer) =>
  server.db.query("SELECT username, auth_sub FROM users WHERE username <> 'admin' ORDER BY username");

/** Asserts a callback answer is the redirect to the sign-in page with `code`, and nothing else. */
const expectFailure = (result: ApiResult, code: string) => {
  expect({ status: result.status, location: result.headers.get("location") }).toEqual({
    status: 302,
    location: `/sign-in?sso_error=${code}`,
  });
  expect(result.headers.get("cache-control")).toBe("no-store");
  expect(result.setCookies).toEqual([CLEAR_LOGIN_COOKIE]);
  expect(result.text).toBe("");
};

/** `GET /api/auth/callback` with exactly these parameters, and the client's cookies. */
const callback = (client: ApiClient, params: Record<string, string> | [string, string][]) =>
  client.get(`/api/auth/callback?${new URLSearchParams(params)}`);

describe("GET /api/auth/callback", () => {
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

  beforeEach(async () => {
    await server.db.query("DELETE FROM sessions");
    await server.db.query("DELETE FROM users WHERE username <> 'admin'");
    issuer.resetReplies();
    issuer.clearCalls();
    issuer.refreshTokens.clear();
  });

  /** A sign-in for alice that must fail with `code`, leaving no session and no account. */
  const expectSignInFailure = async (grant: GrantOptions, code: string) => {
    const { done } = await signIn(server.client(), issuer, { who: alice, grant });
    expectFailure(done, code);
    expect(await sessionRows(server)).toEqual([]);
    expect(await userRows(server)).toEqual([]);
  };

  /** Starts a sign-in, rewrites its login cookie, and finishes it for alice. */
  const signInWithCookie = async (
    rewrite: (saved: LoginCookieState) => unknown,
    grant: GrantOptions = {},
    client = server.client()
  ) => {
    const started = await startSignIn(client, "/mail/42");
    const forged = rewrite(decodeLoginCookie(started.loginCookie));
    client.cookies.set(LOGIN_COOKIE, typeof forged === "string" ? forged : encodeLoginCookie(forged));
    const code = issuer.authorize(started.authorizeUrl, alice, grant);
    return callback(client, { code, state: started.state });
  };

  describe("a completed sign-in", () => {
    test("starts a session for the account and returns to where the sign-in began", async () => {
      const client = server.client();
      const before = Date.now();
      const { done, loginCookie } = await signIn(client, issuer, { who: alice, returnTo: "/mail/42?view=thread#m3" });
      const after = Date.now();

      expect(done.status).toBe(302);
      expect(done.headers.get("location")).toBe("/mail/42?view=thread#m3");
      expect(done.headers.get("cache-control")).toBe("no-store");
      expect(done.text).toBe("");

      // The login cookie is deleted, then the session cookie is set.
      expect(done.setCookies).toHaveLength(2);
      expect(done.setCookies[0]).toBe(CLEAR_LOGIN_COOKIE);
      expect(done.setCookies[1]).toMatch(SESSION_COOKIE);
      expect(client.cookies.has(LOGIN_COOKIE)).toBe(false);

      const [user] = await server.db.query("SELECT * FROM users WHERE username = 'alice'");
      const rows = await sessionRows(server);
      expect(rows).toHaveLength(1);
      const [row] = rows;
      expect(row!.session_id).toBe(SESSION_COOKIE.exec(done.setCookies[1]!)![1]);
      expect(row!.session_user_id).toBe(user!.user_id);
      expect(row!.session_username).toBe("alice");
      expect(row!.session_email).toBe("alice@people.example");
      expect(row!.session_is_read_only).toBeNull();
      expect(row!.session_authenticated_as).toBeNull();
      expect(row!.session_password_version).toBe(1);
      expect(row!.auth_sid).toBe("ses_alice");
      expect([...issuer.refreshTokens.keys()]).toEqual([row!.auth_refresh_token]);
      const recheckAt = new Date(row!.auth_recheck_at).getTime();
      expect(recheckAt).toBeGreaterThanOrEqual(before + TEN_MINUTES);
      expect(recheckAt).toBeLessThanOrEqual(after + TEN_MINUTES);

      const me = await client.get("/api/users/login");
      expect(me.body).toEqual({
        status: "success",
        body: {
          user: { id: user!.user_id, email: "alice@people.example", username: "alice", passwordVersion: 1 },
          app: { version: expect.any(String) },
          sso: { enabled: true, localLogin: true },
        },
      });
      // Nothing from the provider reaches the browser.
      expect(me.text).not.toContain(row!.auth_refresh_token);
      expect(me.text).not.toContain("ses_alice");
      expect(loginCookie).not.toContain(row!.auth_refresh_token);

      const mails = await client.get("/api/mails/accounts");
      expect(mails.status).toBe(200);
    });

    test("exchanges the code at the token endpoint as a confidential client, then fetches the keys", async () => {
      const { code, loginCookie, done } = await signIn(server.client(), issuer, { who: alice });
      expect(done.headers.get("location")).toBe("/");

      const calls = issuer.calls.filter((call) => call.path !== "/.well-known/openid-configuration");
      expect(calls.map((call) => `${call.method} ${call.path}`).slice(0, 1)).toEqual(["POST /oauth/token"]);
      const [token] = calls;
      expect([...token!.form]).toEqual([
        ["grant_type", "authorization_code"],
        ["code", code],
        ["redirect_uri", "https://app.mail.test/api/auth/callback"],
        ["code_verifier", decodeLoginCookie(loginCookie).verifier!],
      ]);
      expect(token!.headers.get("content-type")).toBe("application/x-www-form-urlencoded");
      expect(token!.headers.get("accept")).toBe("application/json");
      // Each part of the credentials is form-encoded before Basic encoding.
      const credentials = `${formEncode(issuer.clientId)}:${formEncode(issuer.clientSecret)}`;
      expect(token!.headers.get("authorization")).toBe(`Basic ${Buffer.from(credentials).toString("base64")}`);
      expect(token!.headers.get("cookie")).toBeNull();

      // The key set is fetched at most once, however many sign-ins follow.
      await signIn(server.client(), issuer, { who: alice });
      const keyCalls = issuer.seen("/.well-known/jwks.json");
      expect(keyCalls.length).toBeLessThanOrEqual(1);
      for (const call of keyCalls) {
        expect(call.method).toBe("GET");
        expect(call.headers.get("accept")).toBe("application/json");
        expect(call.headers.get("authorization")).toBeNull();
      }
    });

    test("returns to the root when the sign-in named no path", async () => {
      const { done } = await signIn(server.client(), issuer, { who: alice });
      expect(done.headers.get("location")).toBe("/");
    });

    test("answers in any letter case and with a trailing slash", async () => {
      const client = server.client();
      const started = await startSignIn(client, "/mail");
      const code = issuer.authorize(started.authorizeUrl, alice);
      const done = await client.get(`/API/Auth/CALLBACK/?code=${code}&state=${started.state}`);
      expect(done.headers.get("location")).toBe("/mail");
    });

    test("ignores parameters it does not know, and takes the first of a repeated one", async () => {
      const client = server.client();
      const started = await startSignIn(client, "/mail");
      const code = issuer.authorize(started.authorizeUrl, alice);
      const done = await callback(client, [
        ["session_state", "abc"],
        ["code", code],
        ["iss", "https://someone-else.e2e.test"],
        ["state", started.state],
        ["code", "another"],
        ["state", "another"],
      ]);
      expect(done.headers.get("location")).toBe("/mail");
    });

    test("stores no refresh token when the provider issues none", async () => {
      const { done } = await signIn(server.client(), issuer, { who: alice, grant: { refreshToken: null } });
      expect(done.headers.get("location")).toBe("/");
      const [row] = await sessionRows(server);
      expect(row!.auth_sid).toBe("ses_alice");
      expect(row!.auth_refresh_token).toBeNull();
      expect(row!.auth_recheck_at).not.toBeNull();
    });

    test("ignores a refresh token that is not a string", async () => {
      const { done } = await signIn(server.client(), issuer, {
        who: alice,
        grant: { answer: (answer) => ({ ...answer, refresh_token: 12345 }) },
      });
      expect(done.headers.get("location")).toBe("/");
      expect((await sessionRows(server))[0]!.auth_refresh_token).toBeNull();
    });

    test("stamps the session with the account's credential generation", async () => {
      await signIn(server.client(), issuer, { who: alice });
      await server.db.query("UPDATE users SET password_version = 7 WHERE username = 'alice'");
      await server.db.query("DELETE FROM sessions");

      const client = server.client();
      await signIn(client, issuer, { who: alice });
      const [row] = await sessionRows(server);
      expect(row!.session_password_version).toBe(7);
      expect((await client.get("/api/users/login")).body.body.user.passwordVersion).toBe(7);
    });

    test("replaces the session the browser arrived with: its id is never carried across", async () => {
      const client = server.client();
      await client.login("admin", ADMIN_PASSWORD);
      const [passwordSession] = await sessionRows(server);
      expect(passwordSession!.session_username).toBe("admin");

      const { done } = await signIn(client, issuer, { who: alice });
      expect(done.headers.get("location")).toBe("/");
      const rows = await sessionRows(server);
      expect(rows).toHaveLength(1);
      expect(rows[0]!.session_username).toBe("alice");
      expect(rows[0]!.session_id).not.toBe(passwordSession!.session_id);
      expect((await client.get("/api/users/login")).body.body.user.username).toBe("alice");
    });

    test("each sign-in is its own session, and one provider session can back several", async () => {
      const first = server.client();
      const second = server.client();
      await signIn(first, issuer, { who: alice });
      await signIn(second, issuer, { who: alice });
      const rows = await sessionRows(server);
      expect(rows).toHaveLength(2);
      expect(rows.map((row) => row.auth_sid)).toEqual(["ses_alice", "ses_alice"]);
      expect(rows[0]!.session_id).not.toBe(rows[1]!.session_id);
      expect(rows[0]!.auth_refresh_token).not.toBe(rows[1]!.auth_refresh_token);
      expect((await first.get("/api/users/login")).body.body.user.username).toBe("alice");
      expect((await second.get("/api/users/login")).body.body.user.username).toBe("alice");
    });

    test("accepts tokens inside the thirty seconds of clock leeway", async () => {
      const { done } = await signIn(server.client(), issuer, {
        who: alice,
        grant: {
          access: { exp: nowS() - 15, iat: nowS() + 15 },
          id: { exp: nowS() - 15, iat: nowS() + 15 },
        },
      });
      expect(done.headers.get("location")).toBe("/");
    });

    test("accepts an audience given as a list that includes this app", async () => {
      const { done } = await signIn(server.client(), issuer, {
        who: alice,
        grant: {
          access: { aud: ["another-service", "mail"] },
          // With several audiences the ID token must name this client as the authorized party.
          id: { aud: ["mail", "another-client"], azp: "mail" },
        },
      });
      expect(done.headers.get("location")).toBe("/");

      const single = await signIn(server.client(), issuer, { who: alice, grant: { id: { aud: ["mail"] } } });
      expect(single.done.headers.get("location")).toBe("/");
    });

    test("reads the token type in any letter case, and ignores claims and fields it does not know", async () => {
      const { done } = await signIn(server.client(), issuer, {
        who: alice,
        grant: {
          accessMint: { header: { typ: "AT+JWT", x5t: "ignored" } },
          idMint: { header: { typ: "jwt" } },
          access: { scope: "openid profile email", extra: { nested: true } },
          id: { at_hash: "ignored", auth_time: undefined, amr: undefined },
          answer: (answer) => ({ ...answer, token_type: "bearer", scope: "openid", something: [1, 2] }),
        },
      });
      expect(done.headers.get("location")).toBe("/");
    });

    test("returns to the root when the login cookie's path was changed to somewhere else", async () => {
      for (const returnTo of ["https://evil.example/", "//evil.example", "/\\evil.example", "mail", 42, null]) {
        const done = await signInWithCookie((saved) => ({ ...saved, returnTo }));
        expect({ returnTo, location: done.headers.get("location") }).toEqual({ returnTo, location: "/" });
      }
      const kept = await signInWithCookie((saved) => ({ ...saved, returnTo: "/elsewhere?x=1" }));
      expect(kept.headers.get("location")).toBe("/elsewhere?x=1");
      const missing = await signInWithCookie(({ returnTo: _returnTo, ...saved }) => saved);
      expect(missing.headers.get("location")).toBe("/");
    });

    test("reads the login cookie wherever it is among the browser's cookies", async () => {
      const client = server.client();
      const started = await startSignIn(client);
      const code = issuer.authorize(started.authorizeUrl, alice);
      const done = await client.get(`/api/auth/callback?code=${code}&state=${started.state}`, {
        sendCookies: false,
        headers: { Cookie: `theme=dark;  ${LOGIN_COOKIE} = ${started.loginCookie} ; other=1; ${LOGIN_COOKIE}=ignored` },
      });
      expect(done.headers.get("location")).toBe("/");
    });

    test("holds a sign-in to the recency its login cookie asks for", async () => {
      // The server never asks for one itself, but the cookie is the browser's to edit.
      const recent = await signInWithCookie((saved) => ({ ...saved, maxAge: 3600, fresh: true }));
      expect(recent.headers.get("location")).toBe("/mail/42");

      const tooOld = await signInWithCookie((saved) => ({ ...saved, maxAge: 60 }), { id: { auth_time: nowS() - 600 } });
      expectFailure(tooOld, "invalid_token");
      const insideLeeway = await signInWithCookie((saved) => ({ ...saved, maxAge: 60 }), {
        id: { auth_time: nowS() - 75 },
      });
      expect(insideLeeway.headers.get("location")).toBe("/mail/42");
      const noAuthTime = await signInWithCookie((saved) => ({ ...saved, maxAge: 3600 }), {
        id: { auth_time: undefined },
      });
      expectFailure(noAuthTime, "invalid_token");
      const authTimeNotANumber = await signInWithCookie((saved) => ({ ...saved, fresh: true }), {
        id: { auth_time: String(nowS()) },
      });
      expectFailure(authTimeNotANumber, "invalid_token");
      const notFresh = await signInWithCookie((saved) => ({ ...saved, fresh: true }), {
        id: { auth_time: nowS() - 600 },
      });
      expectFailure(notFresh, "invalid_token");

      // Anything but a number or `true` asks for nothing.
      const ignored = await signInWithCookie((saved) => ({ ...saved, maxAge: "60", fresh: "true" }), {
        id: { auth_time: undefined },
      });
      expect(ignored.headers.get("location")).toBe("/mail/42");
    });
  });

  describe("`access_denied`", () => {
    test("when the provider sends the browser back with error=access_denied", async () => {
      const client = server.client();
      const started = await startSignIn(client);
      const done = await callback(client, {
        error: "access_denied",
        error_description: "The person said no",
        state: started.state,
      });
      expectFailure(done, "access_denied");
      expect(client.cookies.has(LOGIN_COOKIE)).toBe(false);
      // The provider is not asked anything.
      expect(issuer.seen("/oauth/token")).toEqual([]);
    });

    test("whatever the state says, and even next to a code", async () => {
      const client = server.client();
      const started = await startSignIn(client);
      const code = issuer.authorize(started.authorizeUrl, alice);
      expectFailure(await callback(client, { error: "access_denied", state: "not-the-state", code }), "access_denied");
      expect(issuer.seen("/oauth/token")).toEqual([]);

      const noState = server.client();
      await startSignIn(noState);
      expectFailure(await callback(noState, { error: "access_denied" }), "access_denied");
    });
  });

  describe("`login_failed`", () => {
    test("for any other error the provider sends back", async () => {
      for (const error of [
        "login_required",
        "server_error",
        "ACCESS_DENIED",
        "access_denied ",
        "",
        `access_denied${"x".repeat(80)}`,
      ]) {
        const client = server.client();
        const started = await startSignIn(client);
        const code = issuer.authorize(started.authorizeUrl, alice);
        const done = await callback(client, { error, state: started.state, code });
        expect({ error, location: done.headers.get("location") }).toEqual({
          error,
          location: "/sign-in?sso_error=login_failed",
        });
        expectFailure(done, "login_failed");
      }
      expect(issuer.seen("/oauth/token")).toEqual([]);
      expect(await sessionRows(server)).toEqual([]);
    });

    test("without a login cookie", async () => {
      const client = server.client();
      const started = await startSignIn(client);
      const code = issuer.authorize(started.authorizeUrl, alice);
      const done = await client.get(`/api/auth/callback?code=${code}&state=${started.state}`, { sendCookies: false });
      expectFailure(done, "login_failed");
      // Not even an error from the provider is believed without the cookie.
      const denied = await server.client().get("/api/auth/callback?error=access_denied");
      expectFailure(denied, "login_failed");
      const bare = await server.client().get("/api/auth/callback");
      expectFailure(bare, "login_failed");
      expect(issuer.seen("/oauth/token")).toEqual([]);
    });

    test("with a login cookie that is not what the server wrote", async () => {
      const forgeries: [string, (saved: LoginCookieState) => unknown][] = [
        ["not base64url", (saved) => `${encodeLoginCookie(saved)}==`],
        ["quoted", (saved) => `"${encodeLoginCookie(saved)}"`],
        ["empty", () => ""],
        ["an empty object", () => ({})],
        ["no state", ({ state: _state, ...saved }) => saved],
        ["no nonce", ({ nonce: _nonce, ...saved }) => saved],
        ["no verifier", ({ verifier: _verifier, ...saved }) => saved],
        ["no iat", ({ iat: _iat, ...saved }) => saved],
        ["iat as a string", (saved) => ({ ...saved, iat: String(saved.iat) })],
        ["a numeric state", (saved) => ({ ...saved, state: 12345 })],
      ];
      for (const [label, forge] of forgeries) {
        const done = await signInWithCookie(forge);
        expect({ label, location: done.headers.get("location") }).toEqual({
          label,
          location: "/sign-in?sso_error=login_failed",
        });
        expectFailure(done, "login_failed");
      }
      expect(issuer.seen("/oauth/token")).toEqual([]);
      expect(await sessionRows(server)).toEqual([]);
    });

    // NOTE: a login cookie that is base64url but does not decode to a JSON
    // object is reported as a bad token (`invalid_token`), not as a missing
    // login cookie (`login_failed`) like every other cookie the server did not
    // write: the cookie is parsed with the helper that parses token segments.
    test("a login cookie that is base64url but not a JSON object lands on `invalid_token` instead", async () => {
      const forgeries: [string, (saved: LoginCookieState) => unknown][] = [
        ["not JSON", () => Buffer.from("{ not json").toString("base64url")],
        ["a JSON array", (saved) => encodeLoginCookie([saved])],
        ["a JSON string", () => encodeLoginCookie("state")],
        ["JSON null", () => encodeLoginCookie(null)],
        ["bytes that are not UTF-8 JSON", () => Buffer.from([0xff, 0xfe, 0x00]).toString("base64url")],
      ];
      for (const [label, forge] of forgeries) {
        const done = await signInWithCookie(forge);
        expect({ label, location: done.headers.get("location") }).toEqual({
          label,
          location: "/sign-in?sso_error=invalid_token",
        });
        expectFailure(done, "invalid_token");
      }
      expect(issuer.seen("/oauth/token")).toEqual([]);
      expect(await sessionRows(server)).toEqual([]);
    });

    test("with a login cookie older than five minutes, or from the future", async () => {
      const expired = await signInWithCookie((saved) => ({ ...saved, iat: nowS() - 320 }));
      expectFailure(expired, "login_failed");
      const future = await signInWithCookie((saved) => ({ ...saved, iat: nowS() + 50 }));
      expectFailure(future, "login_failed");
      expect(issuer.seen("/oauth/token")).toEqual([]);

      // Inside both bounds it is still good.
      const old = await signInWithCookie((saved) => ({ ...saved, iat: nowS() - 280 }));
      expect(old.headers.get("location")).toBe("/mail/42");
      const ahead = await signInWithCookie((saved) => ({ ...saved, iat: nowS() + 15 }));
      expect(ahead.headers.get("location")).toBe("/mail/42");
    });

    test("when the state is not the one the sign-in started with", async () => {
      const other = await startSignIn(server.client());
      for (const state of ["not-the-state", "", other.state, undefined]) {
        const client = server.client();
        const started = await startSignIn(client);
        const code = issuer.authorize(started.authorizeUrl, alice);
        const params: Record<string, string> = state === undefined ? { code } : { code, state };
        const done = await callback(client, params);
        expectFailure(done, "login_failed");
        // A prefix or a different case of the right state is still wrong.
        const again = server.client();
        const restarted = await startSignIn(again);
        expectFailure(
          await callback(again, { code, state: restarted.state.slice(0, -1) }),
          "login_failed"
        );
      }
      expect(issuer.seen("/oauth/token")).toEqual([]);
      expect(await sessionRows(server)).toEqual([]);
    });

    test("without a code", async () => {
      for (const extra of [{}, { code: "" }] as Record<string, string>[]) {
        const client = server.client();
        const started = await startSignIn(client);
        expectFailure(await callback(client, { state: started.state, ...extra }), "login_failed");
      }
      expect(issuer.seen("/oauth/token")).toEqual([]);
    });

    test("when the provider refuses the code", async () => {
      const client = server.client();
      const started = await startSignIn(client);
      expectFailure(await callback(client, { code: "never-issued", state: started.state }), "login_failed");
      expect(issuer.seen("/oauth/token")).toHaveLength(1);
      expect(await sessionRows(server)).toEqual([]);
    });

    test("when a code is presented a second time", async () => {
      const client = server.client();
      const { code, state, loginCookie, done } = await signIn(client, issuer, { who: alice });
      expect(done.headers.get("location")).toBe("/");

      const replay = await server.client().get(`/api/auth/callback?code=${code}&state=${state}`, {
        headers: { Cookie: `${LOGIN_COOKIE}=${loginCookie}` },
      });
      expectFailure(replay, "login_failed");
      expect(await sessionRows(server)).toHaveLength(1);
    });

    test("when the code is redeemed with another sign-in's verifier", async () => {
      const other = decodeLoginCookie((await startSignIn(server.client())).loginCookie);
      const done = await signInWithCookie((saved) => ({ ...saved, verifier: other.verifier }));
      expectFailure(done, "login_failed");
      expect(issuer.seen("/oauth/token")).toHaveLength(1);
    });
  });

  describe("`invalid_token`", () => {
    test("for a token whose signature does not verify", async () => {
      await expectSignInFailure({ accessMint: { tamper: true } }, "invalid_token");
      await expectSignInFailure({ idMint: { tamper: true } }, "invalid_token");
    });

    test("for a token signed with a key the provider does not publish", async () => {
      await expectSignInFailure({ accessMint: { kid: "unpublished" } }, "invalid_token");
      await expectSignInFailure({ idMint: { kid: "unpublished" } }, "invalid_token");
      // The name of a published key over another key's signature is no better.
      await expectSignInFailure({ accessMint: { kid: "unpublished", header: { kid: "k1" } } }, "invalid_token");
    });

    test("for a token from another issuer", async () => {
      await expectSignInFailure({ access: { iss: "https://someone-else.e2e.test" } }, "invalid_token");
      await expectSignInFailure({ id: { iss: "https://someone-else.e2e.test" } }, "invalid_token");
      await expectSignInFailure({ access: { iss: `${issuer.issuer}/` } }, "invalid_token");
      await expectSignInFailure({ access: { iss: undefined } }, "invalid_token");
    });

    test("for a token meant for another audience", async () => {
      await expectSignInFailure({ access: { aud: "another-service" } }, "invalid_token");
      await expectSignInFailure({ access: { aud: ["another-service", "Mail"] } }, "invalid_token");
      await expectSignInFailure({ access: { aud: undefined } }, "invalid_token");
      await expectSignInFailure({ id: { aud: "another-client" } }, "invalid_token");
      await expectSignInFailure({ id: { aud: [] } }, "invalid_token");
    });

    test("for an ID token with several audiences that does not name this client as the authorized party", async () => {
      await expectSignInFailure({ id: { aud: ["mail", "another-client"] } }, "invalid_token");
      await expectSignInFailure({ id: { aud: ["mail", "another-client"], azp: "another-client" } }, "invalid_token");
    });

    test("for an ID token without the sign-in's nonce", async () => {
      await expectSignInFailure({ id: { nonce: "another-nonce" } }, "invalid_token");
      await expectSignInFailure({ id: { nonce: undefined } }, "invalid_token");
      await expectSignInFailure({ id: { nonce: 12345 } }, "invalid_token");
      // The nonce of another sign-in is not this one's.
      const other = decodeLoginCookie((await startSignIn(server.client())).loginCookie);
      await expectSignInFailure({ id: { nonce: other.nonce } }, "invalid_token");
    });

    test("for an expired token, or one issued in the future", async () => {
      await expectSignInFailure({ access: { exp: nowS() - 60 } }, "invalid_token");
      await expectSignInFailure({ id: { exp: nowS() - 60 } }, "invalid_token");
      await expectSignInFailure({ access: { iat: nowS() + 60 } }, "invalid_token");
      await expectSignInFailure({ id: { iat: nowS() + 60 } }, "invalid_token");
    });

    test("for a token without the times it must carry, or with times that are not numbers", async () => {
      await expectSignInFailure({ access: { exp: undefined } }, "invalid_token");
      await expectSignInFailure({ access: { iat: undefined } }, "invalid_token");
      await expectSignInFailure({ id: { exp: undefined } }, "invalid_token");
      await expectSignInFailure({ id: { iat: undefined } }, "invalid_token");
      await expectSignInFailure({ access: { exp: String(nowS() + 600) } }, "invalid_token");
      await expectSignInFailure({ access: { iat: String(nowS()) } }, "invalid_token");
    });

    test("for a token of the wrong type", async () => {
      // An ID token where the access token belongs, and the other way round.
      await expectSignInFailure({ accessMint: { header: { typ: "JWT" } } }, "invalid_token");
      await expectSignInFailure({ idMint: { header: { typ: "at+jwt" } } }, "invalid_token");
      await expectSignInFailure({ accessMint: { header: { typ: undefined } } }, "invalid_token");
      await expectSignInFailure({ accessMint: { header: { typ: "application/at+jwt" } } }, "invalid_token");
    });

    test("for a token that names another algorithm, a critical header, or no key", async () => {
      for (const alg of ["none", "HS256", "RS256", "ES256", "eddsa", "Ed25519", undefined]) {
        await expectSignInFailure({ accessMint: { header: { alg } } }, "invalid_token");
      }
      await expectSignInFailure({ idMint: { header: { alg: "HS256" } } }, "invalid_token");
      await expectSignInFailure({ accessMint: { header: { crit: ["exp"] } } }, "invalid_token");
      await expectSignInFailure({ accessMint: { header: { crit: null } } }, "invalid_token");
      await expectSignInFailure({ accessMint: { header: { kid: undefined } } }, "invalid_token");
      await expectSignInFailure({ accessMint: { header: { kid: 1 } } }, "invalid_token");
    });

    test("for a token that is not a JWT at all", async () => {
      const notJwts = [
        "opaque-access-token",
        "a.b",
        "a.b.c.d",
        "",
        "!!!.###.$$$",
        `${Buffer.from("[]").toString("base64url")}.${Buffer.from("{}").toString("base64url")}.AAAA`,
      ];
      for (const token of notJwts) {
        await expectSignInFailure({ answer: (answer) => ({ ...answer, access_token: token }) }, "invalid_token");
      }
      await expectSignInFailure({ answer: (answer) => ({ ...answer, id_token: "opaque" }) }, "invalid_token");
      // A signature of the wrong length, and padding, are refused before any key is used.
      await expectSignInFailure(
        { answer: (answer) => ({ ...answer, access_token: `${String(answer.access_token)}AA` }) },
        "invalid_token"
      );
      await expectSignInFailure(
        { answer: (answer) => ({ ...answer, access_token: `${String(answer.access_token)}=` }) },
        "invalid_token"
      );
    });

    test("for an access token without the claims a session is built from", async () => {
      for (const claim of ["sub", "sid", "username", "name", "role"]) {
        await expectSignInFailure({ access: { [claim]: undefined } }, "invalid_token");
        await expectSignInFailure({ access: { [claim]: 42 } }, "invalid_token");
      }
      await expectSignInFailure({ access: { role: null } }, "invalid_token");
    });

    test("for an ID token about someone other than the access token", async () => {
      await expectSignInFailure({ id: { sub: "usr_mallory" } }, "invalid_token");
      await expectSignInFailure({ id: { sub: undefined } }, "invalid_token");
    });

    test("a refused token leaves the keys in place for the next sign-in", async () => {
      await expectSignInFailure({ accessMint: { kid: "another-unpublished" } }, "invalid_token");
      const { done } = await signIn(server.client(), issuer, { who: alice });
      expect(done.headers.get("location")).toBe("/");
    });
  });

  describe("`unavailable`", () => {
    test("when the provider cannot be reached to exchange the code", async () => {
      const client = server.client();
      const started = await startSignIn(client);
      const code = issuer.authorize(started.authorizeUrl, alice);
      await issuer.down();
      try {
        expectFailure(await callback(client, { code, state: started.state }), "unavailable");
      } finally {
        issuer.up();
      }
      expect(await sessionRows(server)).toEqual([]);
      expect(await userRows(server)).toEqual([]);
      expect(client.cookies.has(LOGIN_COOKIE)).toBe(false);
    });

    test("when the token endpoint fails, is rate limiting, or answers no JSON object", async () => {
      const answers: [string, () => Response][] = [
        ["500", () => jsonReply(500, { error: "server_error" })],
        ["502 with invalid_grant", () => jsonReply(502, { error: "invalid_grant" })],
        ["503 text", () => new Response("Service Unavailable", { status: 503 })],
        ["429", () => jsonReply(429, { error: "slow_down" })],
        ["429 without a body", () => new Response(null, { status: 429 })],
        ["200 text", () => new Response("OK", { status: 200 })],
        ["200 array", () => jsonReply(200, ["access_token"])],
        ["400 text", () => new Response("Bad Request", { status: 400 })],
        ["302", () => new Response(null, { status: 302, headers: { Location: "/oauth/token2" } })],
      ];
      for (const [label, answer] of answers) {
        issuer.clearCalls();
        issuer.replyOnce("/oauth/token", answer);
        const { done } = await signIn(server.client(), issuer, { who: alice });
        expect({ label, location: done.headers.get("location") }).toEqual({
          label,
          location: "/sign-in?sso_error=unavailable",
        });
        expectFailure(done, "unavailable");
        expect(issuer.seen("/oauth/token")).toHaveLength(1);
      }
      expect(await sessionRows(server)).toEqual([]);
      expect(server.alarms.alarms).toEqual([]);
    });

    test("when the token endpoint answers without both tokens", async () => {
      for (const strip of [
        (answer: Record<string, unknown>) => ({ ...answer, access_token: undefined }),
        (answer: Record<string, unknown>) => ({ ...answer, id_token: undefined }),
        (answer: Record<string, unknown>) => ({ ...answer, access_token: 42 }),
        (answer: Record<string, unknown>) => ({ ...answer, id_token: null }),
        () => ({}),
      ]) {
        await expectSignInFailure({ answer: strip }, "unavailable");
      }
    });

    test("when the token endpoint does not answer within five seconds", async () => {
      const client = server.client();
      const started = await startSignIn(client);
      const code = issuer.authorize(started.authorizeUrl, alice);
      const release = issuer.hold("/oauth/token");
      try {
        const begun = Date.now();
        const done = await callback(client, { code, state: started.state });
        const elapsed = Date.now() - begun;
        expectFailure(done, "unavailable");
        expect(elapsed).toBeGreaterThanOrEqual(4_500);
        expect(elapsed).toBeLessThan(15_000);
      } finally {
        release();
      }
      expect(await sessionRows(server)).toEqual([]);
    }, 40_000);
  });

  describe("an existing session", () => {
    test("is left as it was by a sign-in that fails", async () => {
      const client = server.client();
      await client.login("admin", ADMIN_PASSWORD);
      const [before] = await sessionRows(server);

      const started = await startSignIn(client);
      const done = await callback(client, { error: "access_denied", state: started.state });
      expect(done.headers.get("location")).toBe("/sign-in?sso_error=access_denied");
      // The login cookie is deleted; the live session's cookie is renewed as on any request.
      expect(done.setCookies).toHaveLength(2);
      expect(done.setCookies[0]).toBe(CLEAR_LOGIN_COOKIE);
      expect(done.setCookies[1]).toMatch(SESSION_COOKIE);

      const rows = await sessionRows(server);
      expect(rows.map((row) => [row.session_id, row.session_username, row.auth_sid])).toEqual([
        [before!.session_id, "admin", null],
      ]);
      expect((await client.get("/api/users/login")).body.body.user.username).toBe("admin");
    });
  });
});

describe("GET /api/auth/callback when the provider's keys cannot be fetched", () => {
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

  test("lands on `unavailable` while no key set was ever fetched", async () => {
    issuer.replyOnce("/.well-known/jwks.json", () => jsonReply(503, {}));
    const failed = await signIn(server.client(), issuer, { who: alice });
    expectFailure(failed.done, "unavailable");
    expect(issuer.seen("/.well-known/jwks.json")).toHaveLength(1);
    expect(await sessionRows(server)).toEqual([]);
    // The code was spent, and the account is only made once the tokens verify.
    expect(await userRows(server)).toEqual([]);
  });

  // NOTE: the provider answers again here, but the server asks for the key set
  // at most once every thirty seconds, failed attempts included. So for up to
  // thirty seconds after one failed fetch, every sign-in (and every event) is
  // answered `unavailable` without the provider being asked.
  test("stays `unavailable` right after a failed key fetch, without asking the provider again", async () => {
    const stillFailing = await signIn(server.client(), issuer, { who: alice });
    expectFailure(stillFailing.done, "unavailable");
    expect(issuer.seen("/.well-known/jwks.json")).toHaveLength(1);
    expect(await sessionRows(server)).toEqual([]);
  });
});

describe("GET /api/auth/callback with a key set of other people's keys", () => {
  test("uses only Ed25519 signing keys, and skips entries it cannot read", async () => {
    const issuer = await startIssuer();
    const ed25519 = (kid: string) => {
      const { publicKey } = crypto.generateKeyPairSync("ed25519");
      return { ...publicKey.export({ format: "jwk" }), kid };
    };
    const rsa = crypto.generateKeyPairSync("rsa", { modulusLength: 2048 }).publicKey.export({ format: "jwk" });
    issuer.addKey("for-encryption");
    issuer.addKey("for-another-alg");
    const published = (await (await fetch(`${issuer.url}/.well-known/jwks.json`)).json()) as {
      keys: Record<string, unknown>[];
    };
    const byKid = (kid: string) => published.keys.find((key) => key.kid === kid)!;
    issuer.replyAlways("/.well-known/jwks.json", () =>
      jsonReply(200, {
        keys: [
          null,
          "a string",
          { ...rsa, kid: "rsa-key", alg: "RS256", use: "sig" },
          { kty: "OKP", crv: "X25519", x: ed25519("x").x, kid: "x25519" },
          { ...ed25519("no-kid"), kid: undefined },
          { kty: "OKP", crv: "Ed25519", kid: "no-x" },
          { kty: "OKP", crv: "Ed25519", kid: "short-x", x: "AAAA" },
          { ...byKid("for-encryption"), use: "enc" },
          { ...byKid("for-another-alg"), alg: "ES256" },
          // No `use` and no `alg` is fine.
          { kty: "OKP", crv: "Ed25519", x: byKid("k1").x, kid: "k1" },
        ],
      })
    );
    const server = await startServer({ env: issuer.env() });
    try {
      const accepted = await signIn(server.client(), issuer, { who: alice });
      expect(accepted.done.headers.get("location")).toBe("/");
      for (const kid of ["for-encryption", "for-another-alg"]) {
        const refused = await signIn(server.client(), issuer, { who: alice, grant: { accessMint: { kid } } });
        expect({ kid, location: refused.done.headers.get("location") }).toEqual({
          kid,
          location: "/sign-in?sso_error=invalid_token",
        });
      }
    } finally {
      await server.stop();
      await issuer.stop();
    }
  }, 90_000);

  test("a key set without a list of keys verifies nothing", async () => {
    const issuer = await startIssuer();
    issuer.replyAlways("/.well-known/jwks.json", () => jsonReply(200, { keys: "none" }));
    const server = await startServer({ env: issuer.env() });
    try {
      const { done } = await signIn(server.client(), issuer, { who: alice });
      expectFailure(done, "invalid_token");
    } finally {
      await server.stop();
      await issuer.stop();
    }
  }, 90_000);
});

describe("GET /api/auth/callback failures that are nobody's fault but ours or the provider's", () => {
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

  test("a token endpoint that refuses the client lands on `failed`, and alarms", async () => {
    issuer.replyOnce("/oauth/token", () => jsonReply(401, { error: "invalid_client" }));
    const { done } = await signIn(server.client(), issuer, { who: alice });
    expectFailure(done, "failed");
    expect(await sessionRows(server)).toEqual([]);

    const alarm = await waitFor(() => server.alarms.alarms[0], { what: "the sign-on alarm" });
    expect(alarm.title).toBe("🚨 Mail Server Error: Single sign-on failed");
    expect(alarm.description).toBe("**Error:** the token endpoint refused the request (invalid_client)");
  });

  test("so does any refusal that is not `invalid_grant`", async () => {
    const answers: [string, () => Response][] = [
      ["invalid_request", () => jsonReply(400, { error: "invalid_request" })],
      ["unauthorized_client", () => jsonReply(403, { error: "unauthorized_client" })],
      ["no error field", () => jsonReply(400, { message: "nope" })],
      ["404", () => jsonReply(404, {})],
      ["invalid_grant in another case", () => jsonReply(400, { error: "INVALID_GRANT" })],
    ];
    for (const [label, answer] of answers) {
      issuer.replyOnce("/oauth/token", answer);
      const { done } = await signIn(server.client(), issuer, { who: alice });
      expect({ label, location: done.headers.get("location") }).toEqual({
        label,
        location: "/sign-in?sso_error=failed",
      });
      expectFailure(done, "failed");
    }
    expect(await sessionRows(server)).toEqual([]);
  });

  test("`invalid_grant` is a refused code whatever status carries it", async () => {
    for (const status of [400, 401, 403, 429]) {
      issuer.replyOnce("/oauth/token", () => jsonReply(status, { error: "invalid_grant" }));
      const { done } = await signIn(server.client(), issuer, { who: alice });
      expectFailure(done, "login_failed");
    }
  });
});

describe("GET /api/auth/callback behind a proxy, in production", () => {
  let issuer: FakeIssuer;
  let server: TestServer;

  beforeAll(async () => {
    issuer = await startIssuer();
    server = await startServer({ env: issuer.env(), production: true });
  }, 90_000);
  afterAll(async () => {
    await server?.stop();
    await issuer?.stop();
  });

  test("the session cookie is Secure, and the login cookie is as everywhere", async () => {
    const client = server.client();
    const { started, done } = await signIn(client, issuer, { who: alice, returnTo: "/mail" });
    expect(started.setCookies).toHaveLength(1);
    expect(started.setCookies[0]).toMatch(
      /^__Host-login=[A-Za-z0-9_-]+; Path=\/; Secure; HttpOnly; SameSite=Lax; Max-Age=300$/
    );
    expect(done.headers.get("location")).toBe("/mail");
    expect(done.headers.get("strict-transport-security")).toBe("max-age=31536000; includeSubDomains");
    expect(done.setCookies).toHaveLength(2);
    expect(done.setCookies[0]).toBe(CLEAR_LOGIN_COOKIE);
    expect(done.setCookies[1]).toMatch(
      /^connect\.sid=s%3A[A-Za-z0-9_-]{32}\.[A-Za-z0-9%]+; Path=\/; Expires=[^;]+ GMT; HttpOnly; Secure; SameSite=Strict$/
    );
    expect((await client.get("/api/users/login")).body.body.user.username).toBe("alice");
  });

  test("over plain http the session is started but its cookie is withheld; the login cookie is not", async () => {
    await server.db.query("DELETE FROM sessions");
    const client = server.client({ forwarded: false });
    const { started, done } = await signIn(client, issuer, { who: alice });
    expect(started.setCookies).toHaveLength(1);
    expect(done.headers.get("location")).toBe("/");
    expect(done.setCookies).toEqual([CLEAR_LOGIN_COOKIE]);
    expect(await sessionRows(server)).toHaveLength(1);
    expect((await client.get("/api/users/login")).body.body.user).toBeUndefined();
  });
});
