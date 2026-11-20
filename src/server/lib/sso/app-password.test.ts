import { afterAll, beforeEach, describe, expect, it, spyOn } from "bun:test";
import { fakeIssuerEnv, jsonResponse, startFakeIssuer } from "test-helpers";
import * as usersRepo from "../postgres/repositories/users";
import type { UserModel } from "../postgres/models";
import { initializeSso, resetSso, type Sso } from ".";
import { checkAppPassword } from "./app-password";

const issuer = startFakeIssuer();
const findAccount = spyOn(usersRepo, "findUserByAuthSub");

afterAll(() => {
  issuer.stop();
  findAccount.mockRestore();
  resetSso();
});

const T0 = Date.UTC(2026, 10, 18, 12, 0, 0);
const APP_PASSWORD = "abcd-efgh-ijkl-mnop";
const IP = "203.0.113.7";

const ACCOUNT = {
  user_id: "u1",
  username: "alice",
  email: "alice@example.com",
  auth_sub: "usr_alice",
  password_version: 3,
  updated: new Date(T0 - 3_600_000).toISOString(),
} as UserModel;

const active = (scope = "imap smtp", sub = "usr_alice") =>
  jsonResponse(200, { active: true, sub, username: "alice", role: "user", scope, jti: "apw_1" });

let sso: Sso;
/** Both this module's and the provider client's clock. */
let clock = T0;

beforeEach(() => {
  issuer.reset();
  findAccount.mockReset();
  findAccount.mockResolvedValue(ACCOUNT);
  clock = T0;
  sso = initializeSso(fakeIssuerEnv(issuer), { fetch: issuer.fetch, now: () => clock })!;
});

const check = (username = "alice", scope: "imap" | "smtp" = "imap", ip = IP) =>
  checkAppPassword(sso, username, APP_PASSWORD, scope, ip, clock);

describe("checkAppPassword", () => {
  it("accepts an active app password for the linked account, stamping its generation", async () => {
    issuer.introspect = () => active();
    const result = await check();
    expect(result.status).toBe("accepted");
    if (result.status !== "accepted") return;
    expect(result.passwordVersion).toBe(3);
    expect({ ...result.user }).toEqual({
      id: "u1",
      username: "alice",
      email: "alice@example.com",
      passwordVersion: 3,
    });
    const [call] = issuer.seen("/oauth/introspect");
    expect(call?.form.get("token")).toBe(APP_PASSWORD);
    expect(call?.headers.get("x-real-ip")).toBe(IP);
    expect(findAccount.mock.calls).toEqual([["usr_alice"]]);
  });

  it("checks the scope of the protocol asking", async () => {
    issuer.introspect = () => active("smtp");
    expect((await check("alice", "imap")).status).toBe("rejected");
    expect((await check("alice", "smtp")).status).toBe("accepted");
  });

  it("refuses an app password presented under another account's username", async () => {
    issuer.introspect = () => active();
    for (const username of ["bob", "Alice", "alice@alice.example.com"]) {
      expect((await check(username)).status).toBe("rejected");
    }
  });

  it("refuses an inactive answer, and an owner no account is linked to", async () => {
    expect((await check()).status).toBe("rejected");
    expect(findAccount).not.toHaveBeenCalled();

    issuer.introspect = () => active();
    findAccount.mockResolvedValue(null);
    expect((await check()).status).toBe("rejected");
  });

  it("is unavailable, not refused, while the provider cannot answer", async () => {
    issuer.down = true;
    expect((await check()).status).toBe("unavailable");
    issuer.down = false;
    issuer.introspect = () => jsonResponse(502, {});
    expect((await check()).status).toBe("unavailable");
    issuer.introspect = () => jsonResponse(401, { error: "invalid_client" });
    expect((await check()).status).toBe("unavailable");
    expect(findAccount).not.toHaveBeenCalled();
  });

  it("asks again, past the cache, for an account changed moments ago", async () => {
    issuer.introspect = () => active();
    expect((await check()).status).toBe("accepted");
    expect(issuer.seen("/oauth/introspect")).toHaveLength(1);

    // Revoked through an event another copy received: this copy still holds
    // the cached answer, and only the account row says something changed.
    issuer.introspect = () => jsonResponse(200, { active: false });
    findAccount.mockResolvedValue({ ...ACCOUNT, updated: new Date(T0 - 1_000).toISOString() } as UserModel);
    expect((await check()).status).toBe("rejected");
    expect(issuer.seen("/oauth/introspect")).toHaveLength(2);
  });

  it("asks again for an account changed while a stale answer could still be served", async () => {
    issuer.introspect = () => active();
    expect((await check()).status).toBe("accepted");

    // Past the client's fresh minute, with the provider unreachable, the
    // client would serve that answer stale for up to ten minutes. The account
    // changed after it was cached (an event another copy received), so it is
    // asked for afresh, and with nobody to answer, nothing is accepted.
    clock = T0 + 150_000;
    issuer.down = true;
    findAccount.mockResolvedValue({ ...ACCOUNT, updated: new Date(T0 + 30_000).toISOString() } as UserModel);
    expect((await check()).status).toBe("unavailable");
  });

  it("serves the cached answer for an account that has not changed", async () => {
    issuer.introspect = () => active();
    await check();
    issuer.introspect = () => jsonResponse(200, { active: false });
    expect((await check()).status).toBe("accepted");
    expect(issuer.seen("/oauth/introspect")).toHaveLength(1);
  });

  it("names the client's address only when it is one", async () => {
    issuer.introspect = () => active();
    await check("alice", "imap", "::ffff:198.51.100.4");
    expect(issuer.seen("/oauth/introspect")[0]?.headers.get("x-real-ip")).toBe("198.51.100.4");
    sso.client.forget({ sub: "usr_alice" });
    await check("alice", "imap", "unknown");
    expect(issuer.seen("/oauth/introspect")[1]?.headers.get("x-real-ip")).toBeNull();
  });

  it("lets a failed account lookup reject", async () => {
    issuer.introspect = () => active();
    findAccount.mockRejectedValue(new Error("connection lost"));
    await expect(check()).rejects.toThrow("connection lost");
  });
});
