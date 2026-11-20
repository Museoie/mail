/**
 * Single sign-on through the whole request pipeline: the `/api/auth/*`
 * routes, the session they start, the sign-in mode the browser reads, and the
 * password routes single sign-on turns off. The account rules and the event
 * effects have suites of their own; here the account directory and the event
 * handler are spied on, so no database is involved.
 */
import { afterAll, afterEach, beforeEach, describe, expect, it, spyOn } from "bun:test";
import type { RuntimeSessionType } from "common";
import { fakeIssuerEnv, jsonResponse, startFakeIssuer } from "test-helpers";
import { createHttpApp } from "../../index";
import { readCookie, SESSION_COOKIE_NAME, type SessionStore } from "../../session";
import { initializeSso, resetSso } from "../../../sso";
import { postgresAccountDirectory, type LocalAccount } from "../../../sso/accounts";
import * as events from "../../../sso/events";
import { LOCAL_SIGN_IN_OFF_MESSAGE } from "../users";

class MemoryStore implements SessionStore {
  readonly rows = new Map<string, RuntimeSessionType>();
  get = async (id: string) => this.rows.get(id) ?? null;
  set = async (id: string, session: RuntimeSessionType) => {
    this.rows.set(id, session);
  };
  destroy = async (id: string) => {
    this.rows.delete(id);
  };
}

const ALICE: LocalAccount = {
  userId: "u1",
  username: "alice",
  email: "alice@example.com",
  authSub: "usr_alice",
  passwordVersion: 2,
};

const issuer = startFakeIssuer();
const directory = {
  findBySub: spyOn(postgresAccountDirectory, "findBySub"),
  findByUsername: spyOn(postgresAccountDirectory, "findByUsername"),
  findUnlinkedByEmail: spyOn(postgresAccountDirectory, "findUnlinkedByEmail"),
  link: spyOn(postgresAccountDirectory, "link"),
  create: spyOn(postgresAccountDirectory, "create"),
  updateEmail: spyOn(postgresAccountDirectory, "updateEmail"),
};
const handleEvent = spyOn(events, "handleSsoEvent");

afterAll(() => {
  issuer.stop();
  Object.values(directory).forEach((spy) => spy.mockRestore());
  handleEvent.mockRestore();
  resetSso();
});

let store: MemoryStore;

beforeEach(() => {
  issuer.reset();
  store = new MemoryStore();
  Object.values(directory).forEach((spy) => spy.mockReset());
  directory.findBySub.mockResolvedValue(ALICE);
  handleEvent.mockReset();
  handleEvent.mockResolvedValue(undefined);
});

afterEach(() => resetSso());

const useSso = (overrides: Record<string, string> = {}) =>
  initializeSso(fakeIssuerEnv(issuer, overrides), { fetch: issuer.fetch });

const request = (path: string, init: RequestInit = {}) =>
  createHttpApp(store)(new Request(`http://localhost${path}`, init), "127.0.0.1");

const cookiesOf = (response: Response) => response.headers.getSetCookie();
const cookieValue = (response: Response, name: string) =>
  cookiesOf(response)
    .map((cookie) => readCookie(cookie.split(";")[0] ?? "", name))
    .find((value) => value !== undefined);

/** Starts a sign-in and answers the code exchange for it, as the provider would. */
const startSignIn = async (returnTo = "/mail/42", ids: object = {}, at: object = {}) => {
  const started = await request(`/api/auth/login?returnTo=${encodeURIComponent(returnTo)}`);
  const authorizeUrl = started.headers.get("location") ?? "";
  issuer.token = issuer.issueFor(authorizeUrl, ids, at);
  const state = new URL(authorizeUrl).searchParams.get("state");
  const loginCookie = `__Host-login=${cookieValue(started, "__Host-login")}`;
  return { started, authorizeUrl, state, loginCookie };
};

const signIn = async () => {
  const { state, loginCookie } = await startSignIn();
  const done = await request(`/api/auth/callback?code=c0de&state=${state}`, {
    headers: { cookie: loginCookie },
  });
  const sessionCookie = `${SESSION_COOKIE_NAME}=${encodeURIComponent(
    cookieValue(done, SESSION_COOKIE_NAME) ?? ""
  )}`;
  return { done, sessionCookie };
};

describe("with single sign-on off", () => {
  it("answers the /api/auth routes as unknown routes", async () => {
    for (const [method, path] of [
      ["GET", "/api/auth/login"],
      ["GET", "/api/auth/callback?code=c0de&state=s"],
      ["POST", "/api/auth/events"],
    ] as const) {
      const response = await request(path, { method });
      expect(response.status).toBe(404);
      expect(await response.json()).toEqual({ status: "failed", message: "Not found" });
    }
    expect(issuer.calls).toEqual([]);
  });

  it("tells the browser to sign in with a password", async () => {
    const response = await request("/api/users/login");
    expect((await response.json()).body.sso).toEqual({ enabled: false, localLogin: true });
  });

  it("signs out with the same answer as before", async () => {
    const response = await request("/api/users/login", { method: "DELETE" });
    expect(await response.json()).toEqual({ status: "success" });
  });
});

describe("GET /api/users/login with single sign-on on", () => {
  it("says single sign-on is on, and whether passwords still sign in", async () => {
    useSso();
    expect((await (await request("/api/users/login")).json()).body.sso).toEqual({
      enabled: true,
      localLogin: false,
    });
    useSso({ AUTH_LOCAL_LOGIN: "true" });
    expect((await (await request("/api/users/login")).json()).body.sso).toEqual({
      enabled: true,
      localLogin: true,
    });
  });
});

describe("GET /api/auth/login", () => {
  it("sends the browser to the provider with the login cookie", async () => {
    useSso();
    const { started, authorizeUrl } = await startSignIn("/mail/42");
    expect(started.status).toBe(302);
    const url = new URL(authorizeUrl);
    expect(url.origin + url.pathname).toBe("https://auth.example.test/oauth/authorize");
    expect(url.searchParams.get("client_id")).toBe("mail");
    expect(url.searchParams.get("redirect_uri")).toBe(
      "https://mail.example.test/api/auth/callback"
    );
    expect(cookiesOf(started).some((c) => c.startsWith("__Host-login="))).toBe(true);
    expect(started.headers.get("cache-control")).toBe("no-store");
  });

  it("lands on the sign-in page when the provider cannot be reached", async () => {
    useSso();
    issuer.down = true;
    const response = await request("/api/auth/login");
    expect(response.status).toBe(302);
    expect(response.headers.get("location")).toBe("/sign-in?sso_error=unavailable");
  });
});

describe("GET /api/auth/callback", () => {
  it("starts the app's own session for the account and returns where the sign-in started", async () => {
    useSso();
    const before = Date.now();
    const { done, sessionCookie } = await signIn();
    expect(done.status).toBe(302);
    expect(done.headers.get("location")).toBe("/mail/42");
    expect(cookiesOf(done).some((c) => /^__Host-login=;.*Max-Age=0/.test(c))).toBe(true);

    const [stored] = [...store.rows.values()];
    expect(store.rows.size).toBe(1);
    expect(stored?.user).toMatchObject({ id: "u1", username: "alice", passwordVersion: 2 });
    expect(stored?.auth?.sid).toBe("ses_alice");
    expect(stored?.auth?.refreshToken).toBe("rt_alice");
    expect(stored?.auth?.recheckAt.getTime()).toBeGreaterThanOrEqual(before + 10 * 60 * 1000);

    const me = await (await request("/api/users/login", { headers: { cookie: sessionCookie } })).json();
    expect(me.body.user).toMatchObject({ id: "u1", username: "alice" });
    // The refresh token stays on the server.
    expect(JSON.stringify(me)).not.toContain("rt_alice");
  });

  it("refuses an account the link rules refuse, without a session", async () => {
    useSso();
    directory.findBySub.mockResolvedValue(null);
    directory.findByUsername.mockResolvedValue({ ...ALICE, authSub: null });
    directory.findUnlinkedByEmail.mockResolvedValue([]);
    const { done } = await signIn();
    expect(done.headers.get("location")).toBe("/sign-in?sso_error=account_exists");
    expect(cookiesOf(done).some((c) => c.startsWith(`${SESSION_COOKIE_NAME}=`))).toBe(false);
    expect(cookiesOf(done).some((c) => /^__Host-login=;.*Max-Age=0/.test(c))).toBe(true);
    expect(store.rows.size).toBe(0);
  });

  it("names why a sign-in did not complete", async () => {
    useSso();
    const noCookie = await request("/api/auth/callback?code=c0de&state=s");
    expect(noCookie.headers.get("location")).toBe("/sign-in?sso_error=login_failed");

    const denied = await startSignIn();
    const deniedDone = await request(`/api/auth/callback?error=access_denied&state=${denied.state}`, {
      headers: { cookie: denied.loginCookie },
    });
    expect(deniedDone.headers.get("location")).toBe("/sign-in?sso_error=access_denied");

    const wrongCode = await startSignIn();
    const wrongDone = await request(`/api/auth/callback?code=other&state=${wrongCode.state}`, {
      headers: { cookie: wrongCode.loginCookie },
    });
    expect(wrongDone.headers.get("location")).toBe("/sign-in?sso_error=login_failed");

    const otherNonce = await startSignIn("/", { nonce: "another" });
    const otherDone = await request(`/api/auth/callback?code=c0de&state=${otherNonce.state}`, {
      headers: { cookie: otherNonce.loginCookie },
    });
    expect(otherDone.headers.get("location")).toBe("/sign-in?sso_error=invalid_token");

    const down = await startSignIn();
    issuer.down = true;
    const downDone = await request(`/api/auth/callback?code=c0de&state=${down.state}`, {
      headers: { cookie: down.loginCookie },
    });
    expect(downDone.headers.get("location")).toBe("/sign-in?sso_error=unavailable");
    expect(store.rows.size).toBe(0);
  });

  it("puts no code or token in the URL it lands on", async () => {
    useSso();
    directory.findBySub.mockRejectedValue(new Error("connection lost"));
    const { done } = await signIn();
    expect(done.headers.get("location")).toBe("/sign-in?sso_error=failed");
  });
});

describe("DELETE /api/users/login", () => {
  it("ends a single sign-on session here, revokes its token and returns the provider's logout URL", async () => {
    useSso();
    const { sessionCookie } = await signIn();
    const response = await request("/api/users/login", {
      method: "DELETE",
      headers: { cookie: sessionCookie },
    });
    const logoutUrl = new URL((await response.json()).body.logoutUrl);
    expect(logoutUrl.origin + logoutUrl.pathname).toBe("https://auth.example.test/logout");
    expect(logoutUrl.searchParams.get("client_id")).toBe("mail");
    expect(logoutUrl.searchParams.get("post_logout_redirect_uri")).toBe("https://mail.example.test/");
    expect(store.rows.size).toBe(0);
    // Revocation is not awaited; give it its round trip.
    for (let i = 0; i < 50 && issuer.seen("/oauth/revoke").length === 0; i++) {
      await Bun.sleep(10);
    }
    expect(issuer.seen("/oauth/revoke")[0]?.form.get("token")).toBe("rt_alice");
  });

  it("signs out even when the provider cannot be reached", async () => {
    useSso();
    const { sessionCookie } = await signIn();
    issuer.revoke = () => jsonResponse(503, {});
    const response = await request("/api/users/login", {
      method: "DELETE",
      headers: { cookie: sessionCookie },
    });
    expect((await response.json()).status).toBe("success");
    expect(store.rows.size).toBe(0);
  });
});

describe("POST /api/auth/events", () => {
  const post = (body: string, contentType = "application/secevent+jwt") =>
    request("/api/auth/events", {
      method: "POST",
      headers: { "content-type": contentType },
      body,
    });

  it("applies a verified event and acknowledges it, whatever content type it claims", async () => {
    useSso();
    const token = issuer.eventToken({ "user.disabled": { sub: "usr_alice" } });
    for (const contentType of ["application/secevent+jwt", "application/json", "text/plain"]) {
      const response = await post(token, contentType);
      expect(response.status).toBe(202);
    }
    expect(handleEvent.mock.calls.map(([event]) => event)).toEqual(
      Array(3).fill({ type: "user.disabled", jti: "evt_1", sub: "usr_alice" })
    );
  });

  it("acknowledges an event it does not act on", async () => {
    useSso();
    const response = await post(issuer.eventToken({ "invite.accepted": { invite: "inv_1", sub: "usr_bob" } }));
    expect(response.status).toBe(202);
  });

  it("refuses a body that is not a verified event with 400", async () => {
    useSso();
    expect((await post("not-a-token")).status).toBe(400);
    expect((await post("")).status).toBe(400);
    const forged = issuer.eventToken({ "user.disabled": { sub: "usr_alice" } }, { aud: "other" });
    expect((await post(forged)).status).toBe(400);
    expect(handleEvent).not.toHaveBeenCalled();
  });

  it("answers 503 while the provider's keys cannot be fetched, so it retries", async () => {
    useSso();
    issuer.down = true;
    const response = await post(issuer.eventToken({ "user.disabled": { sub: "usr_alice" } }));
    expect(response.status).toBe(503);
    expect(handleEvent).not.toHaveBeenCalled();
  });

  it("answers 500 when applying the event fails, so it retries", async () => {
    useSso();
    handleEvent.mockRejectedValue(new Error("connection lost"));
    const response = await post(issuer.eventToken({ "user.disabled": { sub: "usr_alice" } }));
    expect(response.status).toBe(500);
  });

  it("refuses an oversized body", async () => {
    useSso();
    expect((await post("e".repeat(64 * 1024 + 1))).status).toBe(413);
  });
});

describe("password sign-in with single sign-on on", () => {
  const passwordRoutes = [
    ["/api/users/login", { username: "alice", password: "pw" }],
    ["/api/users/token", { email: "alice@example.com" }],
    ["/api/users/set-info", { email: "alice@example.com", username: "alice", password: "pw" }],
  ] as const;

  const postJson = (path: string, body: unknown) =>
    request(path, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });

  it("is refused without AUTH_LOCAL_LOGIN", async () => {
    useSso();
    for (const [path, body] of passwordRoutes) {
      const response = await postJson(path, body);
      expect(response.status).toBe(403);
      expect(await response.json()).toEqual({ status: "failed", message: LOCAL_SIGN_IN_OFF_MESSAGE });
    }
  });

  it("reaches its routes with AUTH_LOCAL_LOGIN", async () => {
    useSso({ AUTH_LOCAL_LOGIN: "true" });
    const response = await postJson("/api/users/login", []);
    expect(await response.json()).toEqual({ status: "failed", message: "Invalid request body." });
  });
});
