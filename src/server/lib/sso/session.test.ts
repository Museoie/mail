import { afterAll, beforeEach, describe, expect, it } from "bun:test";
import type { RuntimeSessionType, SignedUser, SsoSessionType } from "common";
import { fakeIssuerEnv, jsonResponse, startFakeIssuer } from "test-helpers";
import { HttpSession, signSessionCookie, type SessionStore } from "../http/session";
import { initializeSso, resetSso } from ".";
import {
  enforceSsoSession,
  MAX_RECHECK_DELAY_MS,
  RECHECK_INTERVAL_MS,
  ssoSessionFields,
} from "./session";

const issuer = startFakeIssuer();
afterAll(() => {
  issuer.stop();
  resetSso();
});

const T0 = Date.UTC(2026, 10, 18, 12, 0, 0);
const OPTIONS = { secret: "unit-test-secret", secure: false };
const ALICE = { id: "u1", username: "alice", email: "alice@example.com" } as SignedUser;

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

/** A stored session for alice, loaded the way a request loads it. */
const loadSession = async (auth?: SsoSessionType) => {
  const store = new MemoryStore();
  store.rows.set("sid1", {
    user: ALICE,
    cookie: { originalMaxAge: 1000, _expires: new Date(Date.now() + 1000) },
    auth,
  } as RuntimeSessionType);
  const cookie = `connect.sid=${encodeURIComponent(signSessionCookie("sid1", OPTIONS.secret))}`;
  return { store, session: await HttpSession.load(cookie, store, OPTIONS) };
};

const ssoAuth = (recheckAt: number, refreshToken: string | null = "rt_alice"): SsoSessionType => ({
  sid: "ses_alice",
  refreshToken,
  recheckAt: new Date(recheckAt),
});

const useSso = (localLogin = false) =>
  initializeSso(fakeIssuerEnv(issuer, { AUTH_LOCAL_LOGIN: String(localLogin) }), {
    fetch: issuer.fetch,
    now: () => T0,
  });

beforeEach(() => {
  issuer.reset();
  issuer.now = () => T0;
  resetSso();
});

describe("ssoSessionFields", () => {
  it("keeps the provider session and refresh token, due for a re-check in ten minutes", () => {
    const login = { claims: { sid: "ses_alice" }, refreshToken: "rt_alice" } as Parameters<
      typeof ssoSessionFields
    >[0];
    expect(ssoSessionFields(login, T0)).toEqual({
      sid: "ses_alice",
      refreshToken: "rt_alice",
      recheckAt: new Date(T0 + RECHECK_INTERVAL_MS),
    });
    expect(ssoSessionFields({ ...login, refreshToken: undefined }, T0).refreshToken).toBeNull();
  });
});

describe("enforceSsoSession", () => {
  it("leaves a session alone while its re-check is not due, without calling the provider", async () => {
    useSso();
    const auth = ssoAuth(T0 + 1);
    const { store, session } = await loadSession(auth);
    await enforceSsoSession(session, T0);
    expect(session.user?.id).toBe("u1");
    expect(session.auth).toEqual(auth);
    expect(issuer.calls).toEqual([]);
    expect(store.rows.has("sid1")).toBe(true);
  });

  it("refreshes a due session and moves its re-check ten minutes on", async () => {
    useSso();
    const { session } = await loadSession(ssoAuth(T0 - 1));
    await enforceSsoSession(session, T0);
    expect(session.user?.id).toBe("u1");
    expect(session.auth).toEqual({
      sid: "ses_alice",
      refreshToken: "rt_alice",
      recheckAt: new Date(T0 + RECHECK_INTERVAL_MS),
    });
    const [refresh] = issuer.seen("/oauth/token");
    expect(refresh?.form.get("grant_type")).toBe("refresh_token");
    expect(refresh?.form.get("refresh_token")).toBe("rt_alice");
  });

  it("stores a refresh token the provider rotated", async () => {
    useSso();
    issuer.token = () =>
      jsonResponse(200, { access_token: issuer.accessToken(), refresh_token: "rt_next" });
    const { session } = await loadSession(ssoAuth(T0 - 1));
    await enforceSsoSession(session, T0);
    expect(session.auth?.refreshToken).toBe("rt_next");
  });

  it("ends the session when the provider refuses the refresh token", async () => {
    useSso();
    issuer.token = () => jsonResponse(400, { error: "invalid_grant" });
    const { store, session } = await loadSession(ssoAuth(T0 - 1));
    await enforceSsoSession(session, T0);
    expect(session.user).toBeUndefined();
    expect(session.auth).toBeUndefined();
    expect(store.rows.has("sid1")).toBe(false);
    expect(await session.commit(false, () => {})).toBeUndefined();
  });

  it("serves the session while the provider is unreachable, up to an hour past the re-check", async () => {
    useSso();
    issuer.down = true;
    const auth = ssoAuth(T0 - MAX_RECHECK_DELAY_MS);
    const { store, session } = await loadSession(auth);
    await enforceSsoSession(session, T0);
    expect(session.user?.id).toBe("u1");
    expect(session.auth).toEqual(auth);
    expect(store.rows.has("sid1")).toBe(true);
  });

  it("serves the session through a provider failure, as it does an outage", async () => {
    useSso();
    issuer.token = () => jsonResponse(503, {});
    const { session } = await loadSession(ssoAuth(T0 - 1));
    await enforceSsoSession(session, T0);
    expect(session.user?.id).toBe("u1");

    issuer.token = () => jsonResponse(401, { error: "invalid_client" });
    await enforceSsoSession(session, T0);
    expect(session.user?.id).toBe("u1");
  });

  it("ends the session once the provider has been unreachable for over an hour past the re-check", async () => {
    useSso();
    issuer.down = true;
    const { store, session } = await loadSession(ssoAuth(T0 - MAX_RECHECK_DELAY_MS - 1));
    await enforceSsoSession(session, T0);
    expect(session.user).toBeUndefined();
    expect(store.rows.has("sid1")).toBe(false);
  });

  it("ends a due session that has no refresh token to re-check with", async () => {
    useSso();
    const { session } = await loadSession(ssoAuth(T0 - 1, null));
    await enforceSsoSession(session, T0);
    expect(session.user).toBeUndefined();
    expect(issuer.calls).toEqual([]);
  });

  it("ends a password session while password sign-in is off, and keeps it while it is on", async () => {
    useSso(false);
    const off = await loadSession();
    await enforceSsoSession(off.session, T0);
    expect(off.session.user).toBeUndefined();
    expect(off.store.rows.has("sid1")).toBe(false);

    useSso(true);
    const on = await loadSession();
    await enforceSsoSession(on.session, T0);
    expect(on.session.user?.id).toBe("u1");
  });

  it("with single sign-on off, keeps password sessions and ends the ones single sign-on started", async () => {
    const local = await loadSession();
    await enforceSsoSession(local.session, T0);
    expect(local.session.user?.id).toBe("u1");

    const sso = await loadSession(ssoAuth(T0 + RECHECK_INTERVAL_MS));
    await enforceSsoSession(sso.session, T0);
    expect(sso.session.user).toBeUndefined();
    expect(sso.store.rows.has("sid1")).toBe(false);
  });

  it("does nothing for an anonymous session", async () => {
    useSso();
    const store = new MemoryStore();
    const session = await HttpSession.load(null, store, OPTIONS);
    await enforceSsoSession(session, T0);
    expect(session.user).toBeUndefined();
    expect(issuer.calls).toEqual([]);
  });
});
