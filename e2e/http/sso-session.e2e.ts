/**
 * What single sign-on does to a session after it has started: the re-check
 * with the provider every ten minutes, the sessions a change of sign-in mode
 * ends, and signing out.
 */
import { afterAll, beforeAll, beforeEach, describe, expect, test } from "bun:test";
import {
  ADMIN_PASSWORD,
  createUser,
  startServer,
  waitFor,
  type ApiClient,
  type TestServer,
} from "../harness";
import {
  ISSUER,
  formEncode,
  jsonReply,
  person,
  signIn,
  startIssuer,
  type FakeIssuer,
  type Identity,
} from "../harness/issuer";

const MINUTE = 60 * 1000;
const NOT_LOGGED_IN = { status: "failed", message: "Request user is not logged in." };

const alice = person("alice");

interface SessionRow {
  session_id: string;
  session_username: string;
  auth_sid: string | null;
  auth_refresh_token: string | null;
  auth_recheck_at: Date | null;
}

const sessionRows = (server: TestServer) =>
  server.db.query<SessionRow>(
    "SELECT session_id, session_username, auth_sid, auth_refresh_token, auth_recheck_at FROM sessions ORDER BY updated"
  );

/** Moves every session's re-check to `minutesAgo` minutes in the past. */
const makeRecheckDue = (server: TestServer, minutesAgo = 1) =>
  server.db.query("UPDATE sessions SET auth_recheck_at = now() - make_interval(mins => $1::int)", [minutesAgo]);

/** A client signed in through single sign-on, and its session row. */
const signedIn = async (server: TestServer, issuer: FakeIssuer, who: Identity = alice) => {
  const client = server.client();
  const { done } = await signIn(client, issuer, { who });
  expect(done.headers.get("location")).toBe("/");
  const rows = await sessionRows(server);
  return { client, row: rows[rows.length - 1]! };
};

const whoAmI = async (client: ApiClient) => (await client.get("/api/users/login")).body.body.user?.username;

/** Asserts the client's session is over: anonymous everywhere, and its row gone. */
const expectSessionEnded = async (server: TestServer, client: ApiClient, sessionId: string) => {
  const me = await client.get("/api/users/login");
  expect(me.status).toBe(200);
  expect(me.body.status).toBe("success");
  expect(me.body.message).toBe("Not logged in.");
  expect(me.body.body.user).toBeUndefined();
  // The request that ended the session sets no cookie, not even one to clear it.
  expect(me.setCookies).toEqual([]);
  const mails = await client.get("/api/mails/accounts");
  expect({ status: mails.status, body: mails.body }).toEqual({ status: 401, body: NOT_LOGGED_IN });
  expect(await server.db.query("SELECT 1 FROM sessions WHERE session_id = $1", [sessionId])).toEqual([]);
};

describe("the ten-minute re-check of a single sign-on session", () => {
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

  beforeEach(async () => {
    await server.db.query("DELETE FROM sessions");
    issuer.resetReplies();
    issuer.rotateRefreshTokens = false;
    issuer.up();
    issuer.clearCalls();
  });

  test("costs nothing while it is not due", async () => {
    const { client, row } = await signedIn(server, issuer);
    issuer.clearCalls();
    for (const path of ["/api/users/login", "/api/mails/accounts", "/api/ping", "/"]) {
      expect((await client.get(path)).status).toBe(200);
    }
    expect(issuer.calls).toEqual([]);
    const [after] = await sessionRows(server);
    expect(after).toEqual(row);
  });

  test("when due, refreshes the token and moves the re-check ten minutes on", async () => {
    const { client, row } = await signedIn(server, issuer);
    await makeRecheckDue(server);
    issuer.clearCalls();

    const before = Date.now();
    expect(await whoAmI(client)).toBe("alice");
    const after = Date.now();

    expect(issuer.calls.map((call) => `${call.method} ${call.path}`)).toEqual(["POST /oauth/token"]);
    const [refresh] = issuer.calls;
    expect([...refresh!.form]).toEqual([
      ["grant_type", "refresh_token"],
      ["refresh_token", row.auth_refresh_token!],
    ]);
    const credentials = `${formEncode(issuer.clientId)}:${formEncode(issuer.clientSecret)}`;
    expect(refresh!.headers.get("authorization")).toBe(`Basic ${Buffer.from(credentials).toString("base64")}`);
    expect(refresh!.headers.get("content-type")).toBe("application/x-www-form-urlencoded");
    expect(refresh!.headers.get("accept")).toBe("application/json");

    const [now] = await sessionRows(server);
    expect(now!.session_id).toBe(row.session_id);
    expect(now!.auth_sid).toBe("ses_alice");
    // Not rotated: the same refresh token stays.
    expect(now!.auth_refresh_token).toBe(row.auth_refresh_token);
    expect(now!.auth_recheck_at!.getTime()).toBeGreaterThanOrEqual(before + 10 * MINUTE);
    expect(now!.auth_recheck_at!.getTime()).toBeLessThanOrEqual(after + 10 * MINUTE);

    // And the next request asks nothing.
    issuer.clearCalls();
    expect(await whoAmI(client)).toBe("alice");
    expect(issuer.calls).toEqual([]);
  });

  test("is made by whichever request comes first, whatever it asks for", async () => {
    for (const path of ["/api/ping", "/", "/api/mails/accounts", "/api/auth/login", "/api/nowhere"]) {
      await server.db.query("DELETE FROM sessions");
      const { client } = await signedIn(server, issuer);
      await makeRecheckDue(server);
      issuer.clearCalls();
      await client.get(path);
      expect({ path, calls: issuer.seen("/oauth/token").length }).toEqual({ path, calls: 1 });
      expect(await whoAmI(client)).toBe("alice");
    }
  });

  test("stores the refresh token the provider rotated to", async () => {
    issuer.rotateRefreshTokens = true;
    const { client, row } = await signedIn(server, issuer);
    await makeRecheckDue(server);
    expect(await whoAmI(client)).toBe("alice");

    const [rotated] = await sessionRows(server);
    expect(rotated!.auth_refresh_token).not.toBe(row.auth_refresh_token);
    expect(issuer.refreshTokens.get(rotated!.auth_refresh_token!)).toEqual({ who: alice, revoked: false });

    // The next re-check presents the new one.
    await makeRecheckDue(server);
    issuer.clearCalls();
    expect(await whoAmI(client)).toBe("alice");
    expect(issuer.seen("/oauth/token")[0]!.form.get("refresh_token")).toBe(rotated!.auth_refresh_token);
    const [again] = await sessionRows(server);
    expect(again!.auth_refresh_token).not.toBe(rotated!.auth_refresh_token);
  });

  test("ignores a rotated refresh token that is not a string", async () => {
    const { client, row } = await signedIn(server, issuer);
    await makeRecheckDue(server);
    issuer.replyOnce("/oauth/token", () =>
      jsonReply(200, { access_token: issuer.accessToken(alice), refresh_token: 12345 })
    );
    expect(await whoAmI(client)).toBe("alice");
    expect((await sessionRows(server))[0]!.auth_refresh_token).toBe(row.auth_refresh_token);
  });

  test("ends the session when the provider refuses the refresh token", async () => {
    const { client, row } = await signedIn(server, issuer);
    issuer.refreshTokens.get(row.auth_refresh_token!)!.revoked = true;
    await makeRecheckDue(server);
    await expectSessionEnded(server, client, row.session_id);
  });

  test("ends the session on `invalid_grant` whatever status carries it", async () => {
    for (const status of [400, 401, 403]) {
      const { client, row } = await signedIn(server, issuer);
      await makeRecheckDue(server);
      issuer.replyOnce("/oauth/token", () => jsonReply(status, { error: "invalid_grant" }));
      await expectSessionEnded(server, client, row.session_id);
    }
  });

  test("ends only that session: another session of the same person re-checks on its own", async () => {
    const first = await signedIn(server, issuer);
    const second = await signedIn(server, issuer);
    issuer.refreshTokens.get(first.row.auth_refresh_token!)!.revoked = true;
    await makeRecheckDue(server);
    await expectSessionEnded(server, first.client, first.row.session_id);
    expect(await whoAmI(second.client)).toBe("alice");
    expect((await sessionRows(server)).map((row) => row.session_id)).toEqual([second.row.session_id]);
  });

  test("ends a due session that has no refresh token, without asking the provider", async () => {
    const client = server.client();
    await signIn(client, issuer, { who: alice, grant: { refreshToken: null } });
    const [row] = await sessionRows(server);
    expect(row!.auth_refresh_token).toBeNull();
    // Until the re-check is due it is a session like any other.
    expect(await whoAmI(client)).toBe("alice");

    await makeRecheckDue(server);
    issuer.clearCalls();
    await expectSessionEnded(server, client, row!.session_id);
    expect(issuer.calls).toEqual([]);
  });

  test("keeps serving while the provider is unreachable, and tries again on every request", async () => {
    const { client, row } = await signedIn(server, issuer);
    await makeRecheckDue(server, 30);
    const [due] = await sessionRows(server);
    await issuer.down();
    try {
      expect(await whoAmI(client)).toBe("alice");
      expect((await client.get("/api/mails/accounts")).status).toBe(200);
    } finally {
      issuer.up();
    }
    // Nothing about the re-check moved while the provider was away.
    const [kept] = await sessionRows(server);
    expect(kept!.auth_recheck_at).toEqual(due!.auth_recheck_at);
    expect(kept!.auth_refresh_token).toBe(row.auth_refresh_token);

    issuer.clearCalls();
    expect(await whoAmI(client)).toBe("alice");
    expect(issuer.seen("/oauth/token")).toHaveLength(1);
    const [rechecked] = await sessionRows(server);
    expect(rechecked!.auth_recheck_at!.getTime()).toBeGreaterThan(Date.now() + 9 * MINUTE);
  });

  test("keeps serving through a provider that fails, rate limits or answers nonsense", async () => {
    const answers: [string, () => Response][] = [
      ["500", () => jsonReply(500, { error: "server_error" })],
      ["503", () => new Response("Service Unavailable", { status: 503 })],
      ["429", () => jsonReply(429, { error: "slow_down" })],
      ["not JSON", () => new Response("OK", { status: 200 })],
      ["no access token", () => jsonReply(200, { token_type: "Bearer" })],
      // Not an outage, but not a refusal of this session either.
      ["invalid_client", () => jsonReply(401, { error: "invalid_client" })],
      ["invalid_request", () => jsonReply(400, { error: "invalid_request" })],
      ["a token that does not verify", () => jsonReply(200, { access_token: issuer.accessToken(alice, {}, { tamper: true }) })],
      ["a token for another audience", () => jsonReply(200, { access_token: issuer.accessToken(alice, { aud: "other" }) })],
      ["an expired token", () => jsonReply(200, { access_token: issuer.accessToken(alice, { exp: 1 }) })],
    ];
    const { client, row } = await signedIn(server, issuer);
    await makeRecheckDue(server, 59);
    const [due] = await sessionRows(server);
    for (const [label, answer] of answers) {
      issuer.clearCalls();
      issuer.replyOnce("/oauth/token", answer);
      expect({ label, user: await whoAmI(client) }).toEqual({ label, user: "alice" });
      expect(issuer.seen("/oauth/token")).toHaveLength(1);
      const [kept] = await sessionRows(server);
      expect(kept!.auth_recheck_at).toEqual(due!.auth_recheck_at);
      expect(kept!.auth_refresh_token).toBe(row.auth_refresh_token);
    }
  });

  test("ends the session once the re-check is more than an hour late", async () => {
    const { client, row } = await signedIn(server, issuer);
    await makeRecheckDue(server, 61);
    await issuer.down();
    try {
      await expectSessionEnded(server, client, row.session_id);
    } finally {
      issuer.up();
    }
  });

  test("ends it that late for any failure, not only an unreachable provider", async () => {
    for (const answer of [
      () => jsonReply(503, {}),
      () => jsonReply(401, { error: "invalid_client" }),
      () => jsonReply(200, { access_token: issuer.accessToken(alice, { aud: "other" }) }),
    ]) {
      const { client, row } = await signedIn(server, issuer);
      await makeRecheckDue(server, 61);
      issuer.replyOnce("/oauth/token", answer);
      await expectSessionEnded(server, client, row.session_id);
    }
  });

  test("an hour late is still not too late when the provider answers", async () => {
    const { client } = await signedIn(server, issuer);
    await makeRecheckDue(server, 600);
    expect(await whoAmI(client)).toBe("alice");
    const [row] = await sessionRows(server);
    expect(row!.auth_recheck_at!.getTime()).toBeGreaterThan(Date.now() + 9 * MINUTE);
  });

  // NOTE: the re-check only asks whether the refresh token is still good. It
  // does not compare the refreshed access token with the session: a token for
  // another person or provider session keeps the session as it was.
  test("does not look at who the refreshed token is for", async () => {
    const { client, row } = await signedIn(server, issuer);
    await makeRecheckDue(server);
    issuer.replyOnce("/oauth/token", () =>
      jsonReply(200, { access_token: issuer.accessToken(person("mallory", { sid: "ses_other" })) })
    );
    expect(await whoAmI(client)).toBe("alice");
    const [after] = await sessionRows(server);
    expect(after!.session_username).toBe("alice");
    expect(after!.auth_sid).toBe("ses_alice");
    expect(after!.session_id).toBe(row.session_id);
  });

  test("a session row that names a provider session but no re-check time is a password session", async () => {
    const { client, row } = await signedIn(server, issuer);
    await server.db.query("UPDATE sessions SET auth_recheck_at = NULL");
    // With password sign-in off, that ends it.
    await expectSessionEnded(server, client, row.session_id);
  });

  test("ends with its account, deleted or soft-deleted, before the provider is asked anything", async () => {
    const deleted = await signedIn(server, issuer, person("dora"));
    const softDeleted = await signedIn(server, issuer, person("emil"));
    await makeRecheckDue(server);
    issuer.clearCalls();

    await server.db.query("DELETE FROM users WHERE username = 'dora'");
    await server.db.query("UPDATE users SET is_deleted = TRUE WHERE username = 'emil'");
    await expectSessionEnded(server, deleted.client, deleted.row.session_id);
    await expectSessionEnded(server, softDeleted.client, softDeleted.row.session_id);
    expect(issuer.calls).toEqual([]);
  });
});

describe("sessions and the sign-in mode", () => {
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

  test("a password session lives next to single sign-on while AUTH_LOCAL_LOGIN keeps passwords", async () => {
    const client = server.client();
    await client.login("admin", ADMIN_PASSWORD);
    issuer.clearCalls();
    expect(await whoAmI(client)).toBe("admin");
    // It has nothing to re-check, however old it gets.
    expect(issuer.calls).toEqual([]);
    const [row] = await sessionRows(server);
    expect(row).toEqual({
      session_id: row!.session_id,
      session_username: "admin",
      auth_sid: null,
      auth_refresh_token: null,
      auth_recheck_at: null,
    });
    await client.delete("/api/users/login");
  });

  test("a password session ends at its next request once password sign-in is off", async () => {
    const bob = await createUser(server, "bob");
    const password = server.client();
    await password.login(bob.username, bob.password);
    const sso = await signedIn(server, issuer);
    const [passwordRow] = await sessionRows(server);
    expect(passwordRow!.session_username).toBe("bob");

    await server.restart({ AUTH_LOCAL_LOGIN: "false" });
    await expectSessionEnded(server, password, passwordRow!.session_id);
    // The single sign-on session carries on over the restart.
    expect(await whoAmI(sso.client)).toBe("alice");
    expect((await sessionRows(server)).map((row) => row.session_id)).toEqual([sso.row.session_id]);

    // Turning passwords back on does not bring the ended session back.
    await server.restart({ AUTH_LOCAL_LOGIN: "true" });
    expect(await whoAmI(password)).toBeUndefined();
    expect(await whoAmI(sso.client)).toBe("alice");
    await sso.client.delete("/api/users/login");
  }, 120_000);

  test("a single sign-on session ends at its next request once single sign-on is off", async () => {
    const bob = await createUser(server, "bert");
    const password = server.client();
    await password.login(bob.username, bob.password);
    const sso = await signedIn(server, issuer);
    issuer.clearCalls();

    await server.restart({ AUTH_ISSUER: "" });
    await expectSessionEnded(server, sso.client, sso.row.session_id);
    // Nothing is left to ask, so nothing is asked; the password session is untouched.
    expect(issuer.calls).toEqual([]);
    expect(await whoAmI(password)).toBe("bert");
    const signOut = await sso.client.delete("/api/users/login");
    expect(signOut.body).toEqual({ status: "success" });

    await server.restart({ AUTH_ISSUER: ISSUER });
    expect(await whoAmI(sso.client)).toBeUndefined();
    expect(await whoAmI(password)).toBe("bert");
    await password.delete("/api/users/login");
  }, 120_000);

  test("a due session is re-checked by a freshly started server, which first has to find the provider", async () => {
    const { client } = await signedIn(server, issuer);
    await makeRecheckDue(server);
    await server.restart();
    issuer.clearCalls();
    expect(await whoAmI(client)).toBe("alice");
    expect(issuer.calls.map((call) => `${call.method} ${call.path}`)).toEqual([
      "GET /.well-known/openid-configuration",
      "POST /oauth/token",
      "GET /.well-known/jwks.json",
    ]);
    await client.delete("/api/users/login");
  }, 120_000);

  test("a freshly started server that cannot reach the provider keeps a due session until the hour is up", async () => {
    const kept = await signedIn(server, issuer);
    const late = await signedIn(server, issuer, person("larry"));
    await makeRecheckDue(server, 30);
    await server.db.query(
      "UPDATE sessions SET auth_recheck_at = now() - interval '61 minutes' WHERE session_id = $1",
      [late.row.session_id]
    );
    await issuer.down();
    try {
      await server.restart();
      expect(await whoAmI(kept.client)).toBe("alice");
      await expectSessionEnded(server, late.client, late.row.session_id);
    } finally {
      issuer.up();
    }
  }, 120_000);
});

describe("DELETE /api/users/login", () => {
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
    issuer.resetReplies();
    issuer.up();
    issuer.clearCalls();
  });

  const LOGOUT_URL = `${ISSUER}/logout?client_id=mail&post_logout_redirect_uri=https%3A%2F%2Fapp.mail.test%2F`;

  test("ends the session here, revokes its refresh token, and says where to end the provider's", async () => {
    const { client, row } = await signedIn(server, issuer);
    issuer.clearCalls();

    const result = await client.delete("/api/users/login");
    expect(result.status).toBe(200);
    expect(result.body).toEqual({ status: "success", body: { logoutUrl: LOGOUT_URL } });
    expect(result.setCookies).toEqual([]);
    expect(await sessionRows(server)).toEqual([]);

    // The revocation is not waited for.
    const revoke = await waitFor(() => issuer.seen("/oauth/revoke")[0], { what: "the revocation call" });
    expect(revoke.method).toBe("POST");
    expect([...revoke.form]).toEqual([
      ["token", row.auth_refresh_token!],
      ["token_type_hint", "refresh_token"],
    ]);
    const credentials = `${formEncode(issuer.clientId)}:${formEncode(issuer.clientSecret)}`;
    expect(revoke.headers.get("authorization")).toBe(`Basic ${Buffer.from(credentials).toString("base64")}`);
    expect(revoke.headers.get("content-type")).toBe("application/x-www-form-urlencoded");
    expect(issuer.refreshTokens.get(row.auth_refresh_token!)!.revoked).toBe(true);
    // The end-session endpoint is the browser's to visit, not the server's.
    expect(issuer.seen("/logout")).toEqual([]);

    expect(await whoAmI(client)).toBeUndefined();
  });

  test("answers the same sign-out a second time, as for any anonymous caller", async () => {
    const { client } = await signedIn(server, issuer);
    await client.delete("/api/users/login");
    await waitFor(() => issuer.seen("/oauth/revoke")[0], { what: "the revocation call" });
    issuer.clearCalls();

    const again = await client.delete("/api/users/login");
    expect(again.body).toEqual({ status: "success" });
    const anonymous = await server.client().delete("/api/users/login");
    expect(anonymous.body).toEqual({ status: "success" });
    expect(issuer.calls).toEqual([]);
  });

  test("calls no revocation for a session without a refresh token", async () => {
    const client = server.client();
    await signIn(client, issuer, { who: alice, grant: { refreshToken: null } });
    issuer.clearCalls();
    const result = await client.delete("/api/users/login");
    expect(result.body).toEqual({ status: "success", body: { logoutUrl: LOGOUT_URL } });
    expect(await sessionRows(server)).toEqual([]);
    // One more round trip to the server, then still nothing was asked of the provider.
    await server.client().get("/api/ping");
    expect(issuer.calls).toEqual([]);
  });

  test("a password session signs out as it always did", async () => {
    const client = server.client();
    await client.login("admin", ADMIN_PASSWORD);
    issuer.clearCalls();
    const result = await client.delete("/api/users/login");
    expect(result.body).toEqual({ status: "success" });
    expect(await sessionRows(server)).toEqual([]);
    expect(issuer.calls).toEqual([]);
  });

  test("signs out when the provider is down: the logout URL is the one it already knows", async () => {
    const { client } = await signedIn(server, issuer);
    await issuer.down();
    try {
      const result = await client.delete("/api/users/login");
      expect(result.status).toBe(200);
      expect(result.body).toEqual({ status: "success", body: { logoutUrl: LOGOUT_URL } });
      expect(await sessionRows(server)).toEqual([]);
    } finally {
      issuer.up();
    }
    expect(await whoAmI(client)).toBeUndefined();
  });

  test("signs out when the provider refuses or fails the revocation", async () => {
    for (const answer of [
      () => jsonReply(503, {}),
      () => jsonReply(400, { error: "unsupported_token_type" }),
      () => jsonReply(401, { error: "invalid_client" }),
    ]) {
      const { client } = await signedIn(server, issuer);
      issuer.clearCalls();
      issuer.replyOnce("/oauth/revoke", answer);
      const result = await client.delete("/api/users/login");
      expect(result.body).toEqual({ status: "success", body: { logoutUrl: LOGOUT_URL } });
      await waitFor(() => issuer.seen("/oauth/revoke")[0], { what: "the revocation call" });
      expect(await sessionRows(server)).toEqual([]);
    }
  });

  test("a freshly started server that cannot reach the provider signs out without a logout URL", async () => {
    const { client } = await signedIn(server, issuer);
    await issuer.down();
    try {
      await server.restart();
      const result = await client.delete("/api/users/login");
      expect(result.status).toBe(200);
      expect(result.body).toEqual({ status: "success" });
      expect(await sessionRows(server)).toEqual([]);
    } finally {
      issuer.up();
    }
  }, 120_000);
});

describe("DELETE /api/users/login with other provider settings", () => {
  const start = async (overrides: Record<string, unknown>, env: Record<string, string | undefined> = {}) => {
    const issuer = await startIssuer();
    Object.assign(issuer.discoveryOverrides, overrides);
    const server = await startServer({ env: issuer.env(env) });
    return { issuer, server, stop: async () => void (await server.stop(), await issuer.stop()) };
  };

  test("the browser comes back to the origin of the redirect URI", async () => {
    const { issuer, server, stop } = await start(
      {},
      { AUTH_REDIRECT_URI: "http://localhost:8443/deep/path/callback?x=1" }
    );
    try {
      const { client } = await signedIn(server, issuer);
      const result = await client.delete("/api/users/login");
      expect(result.body.body.logoutUrl).toBe(
        `${ISSUER}/logout?client_id=mail&post_logout_redirect_uri=http%3A%2F%2Flocalhost%3A8443%2F`
      );
    } finally {
      await stop();
    }
  }, 90_000);

  test("parameters the end-session endpoint already carries are kept, and replaced where they clash", async () => {
    const { issuer, server, stop } = await start({
      end_session_endpoint: `${ISSUER}/logout?ui=mini%20one&client_id=old&client_id=older#bye`,
    });
    try {
      const { client } = await signedIn(server, issuer);
      const result = await client.delete("/api/users/login");
      expect(result.body.body.logoutUrl).toBe(
        `${ISSUER}/logout?ui=mini+one&client_id=mail&post_logout_redirect_uri=https%3A%2F%2Fapp.mail.test%2F#bye`
      );
    } finally {
      await stop();
    }
  }, 90_000);

  test("without an end-session endpoint there is no logout URL; the token is revoked all the same", async () => {
    const { issuer, server, stop } = await start({ end_session_endpoint: undefined });
    try {
      const { client, row } = await signedIn(server, issuer);
      const result = await client.delete("/api/users/login");
      expect(result.body).toEqual({ status: "success" });
      const revoke = await waitFor(() => issuer.seen("/oauth/revoke")[0], { what: "the revocation call" });
      expect(revoke.form.get("token")).toBe(row.auth_refresh_token);
      expect(await sessionRows(server)).toEqual([]);
    } finally {
      await stop();
    }
  }, 90_000);

  test("without a revocation endpoint nothing is revoked; the logout URL is given all the same", async () => {
    const { issuer, server, stop } = await start({ revocation_endpoint: undefined });
    try {
      const { client } = await signedIn(server, issuer);
      issuer.clearCalls();
      const result = await client.delete("/api/users/login");
      expect(result.body.body.logoutUrl).toBe(
        `${ISSUER}/logout?client_id=mail&post_logout_redirect_uri=https%3A%2F%2Fapp.mail.test%2F`
      );
      await server.client().get("/api/ping");
      expect(issuer.calls).toEqual([]);
      expect(await sessionRows(server)).toEqual([]);
    } finally {
      await stop();
    }
  }, 90_000);
});
