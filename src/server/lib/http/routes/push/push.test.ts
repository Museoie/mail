import { describe, it, expect, mock, beforeEach, afterAll } from "bun:test";
import type { ApiResponse } from "../route";
import type { HttpRequest } from "../../request";

// ── Mocks ─────────────────────────────────────────────────────────────────────

const mockGetPushPublicKey = mock(() => "test-vapid-public-key");
const mockStoreSubscription = mock(async () => null as unknown);
const mockRefreshSubscription = mock(async () => null as unknown);
const mockDeleteSubscriptionForUser = mock(async () => false as unknown);

// Include base set of server exports so this mock doesn't break test files
// that run after this one (Bun's mock.module is global within a coverage run).
mock.module("server", () => ({
  push: {
    getPushPublicKey: mockGetPushPublicKey,
    storeSubscription: mockStoreSubscription,
    refreshSubscription: mockRefreshSubscription,
    deleteSubscriptionForUser: mockDeleteSubscriptionForUser,
    decrementBadgeCount: mock(async () => {}),
  },
  // Base exports needed by users/mails test files
  getUser: mock(async () => null),
  setUserInfo: mock(async () => null),
  isValidEmail: mock(() => true),
  createToken: mock(async () => ({ id: "u1", username: "u", token: "tok" })),
  getSignedUser: mock(() => null),
  createAuthenticationMail: mock(() => ({})),
  sendMail: mock(async () => {}),
  startTimer: mock(() => {}),
  version: "0.0.0",
  getMailHeaders: mock(async () => []),
  toMailHeaderData: mock((row: unknown) => row),
  getAccounts: mock(async () => ({ received: [], sent: [] })),
  getMailBody: mock(async () => null),
  deleteMail: mock(async () => {}),
  markRead: mock(async () => {}),
  markSaved: mock(async () => {}),
  addressToUsername: mock((addr: string) => addr.split("@")[0]),
  searchMail: mock(async () => []),
  getDomain: mock(() => "example.com"),
  getAllowlistForUser: mock(async () => []),
  addAllowlistEntry: mock(async () => null),
  removeAllowlistEntry: mock(async () => false),
  markSpam: mock(async () => ({ found: false, changed: false })),
  getAttachment: mock(() => undefined),
  AUTH_ERROR_MESSAGE: "Authentication required",
  MailValidationError: class extends Error {},
  MailSendingError: class extends Error {},
  mailsTable: { queryOne: mock(async () => null) },
  SpamAllowlistModel: class {},
  logger: { error: mock(() => {}), info: mock(() => {}), warn: mock(() => {}) },
}));

// The stubs above reach the leaf modules through the barrel (`lib/logger.ts`
// included) — hand the real barrel back so the next file does not inherit them.
afterAll(() => {
  const realServer = (globalThis as Record<string, unknown>).__REAL_SERVER;
  if (realServer) mock.module("server", () => realServer);
});

// ── Helpers ───────────────────────────────────────────────────────────────────

const makeReq = (overrides: Record<string, unknown> = {}) =>
  ({
    session: { user: { id: "u1", username: "alice" } },
    params: {},
    body: {},
    ...overrides,
  }) as unknown as HttpRequest;

// ── get-public-key ────────────────────────────────────────────────────────────

describe("getPublicKeyRoute", () => {
  beforeEach(() => mockGetPushPublicKey.mockClear());

  it("returns the VAPID public key", async () => {
    const { getPublicKeyRoute } = await import("./get-public-key");
    const result = await getPublicKeyRoute.callback(makeReq());
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect((result as ApiResponse<unknown>).body).toBe("test-vapid-public-key");
    expect(mockGetPushPublicKey).toHaveBeenCalledTimes(1);
  });
});

// ── get-refresh ───────────────────────────────────────────────────────────────

describe("getRefreshRoute", () => {
  beforeEach(() => mockRefreshSubscription.mockClear());

  it("refreshes the caller's own subscription and returns success", async () => {
    const { getRefreshRoute } = await import("./get-refresh");
    mockRefreshSubscription.mockResolvedValueOnce(true);
    const req = makeReq({ params: { id: "sub1" } });
    const result = await getRefreshRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
    // Scoped to the session user, as the delete is.
    expect(mockRefreshSubscription).toHaveBeenCalledWith("sub1", "u1");
  });

  it("returns failed when the caller has no such subscription", async () => {
    const { getRefreshRoute } = await import("./get-refresh");
    mockRefreshSubscription.mockResolvedValueOnce(false);
    const req = makeReq({ params: { id: "nonexistent" } });
    const result = await getRefreshRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toMatch(/No subscription found/i);
  });

  it("refreshes nothing for a caller without a session", async () => {
    const { getRefreshRoute } = await import("./get-refresh");
    const req = makeReq({ params: { id: "sub1" }, session: {} });
    const result = await getRefreshRoute.callback(req);
    expect(result).toEqual({ status: "failed", message: "No subscription found" });
    expect(mockRefreshSubscription).not.toHaveBeenCalled();
  });

});

// ── post-subscribe ────────────────────────────────────────────────────────────

describe("postSubscribeRoute", () => {
  beforeEach(() => mockStoreSubscription.mockClear());

  it("returns success with subscription id when stored", async () => {
    const { postSubscribeRoute } = await import("./post-subscribe");
    const fakeSub = { endpoint: "https://fcm.example.com", keys: { p256dh: "abc", auth: "xyz" } };
    mockStoreSubscription.mockResolvedValueOnce({ _id: "sub42" });
    const req = makeReq({ body: { subscription: fakeSub } });
    const result = await postSubscribeRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect((result as ApiResponse<unknown>).body).toBe("sub42");
    expect(mockStoreSubscription).toHaveBeenCalledWith("u1", fakeSub);
  });

  const expectNoStore = async (subscription: unknown, reason: RegExp) => {
    const { postSubscribeRoute } = await import("./post-subscribe");
    const req = makeReq({ body: { subscription } });
    const result = await postSubscribeRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toMatch(reason);
    expect(mockStoreSubscription).not.toHaveBeenCalled();
  };

  it("rejects a non-https endpoint without writing", async () => {
    await expectNoStore(
      { endpoint: "http://fcm.example.com/s", keys: { p256dh: "abc", auth: "xyz" } },
      /https/,
    );
  });

  it("rejects a loopback endpoint without writing", async () => {
    await expectNoStore(
      { endpoint: "https://127.0.0.1:6379/s", keys: { p256dh: "abc", auth: "xyz" } },
      /not by IP/,
    );
  });

  it("rejects a link-local metadata endpoint without writing", async () => {
    await expectNoStore(
      { endpoint: "https://169.254.169.254/latest/meta-data/", keys: { p256dh: "abc", auth: "xyz" } },
      /not by IP/,
    );
  });

  it("rejects a malformed subscription without writing", async () => {
    await expectNoStore(undefined, /subscription must be an object/);
  });

  it("rejects missing keys without writing", async () => {
    await expectNoStore({ endpoint: "https://fcm.example.com/s" }, /keys must be an object/);
  });

  it("rejects a non-object body without writing", async () => {
    const { postSubscribeRoute } = await import("./post-subscribe");
    const req = makeReq({ body: "subscription=1" });
    const result = await postSubscribeRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect(mockStoreSubscription).not.toHaveBeenCalled();
  });

  it("returns failed when store returns null", async () => {
    const { postSubscribeRoute } = await import("./post-subscribe");
    const fakeSub = { endpoint: "https://fcm.example.com", keys: { p256dh: "abc", auth: "xyz" } };
    mockStoreSubscription.mockResolvedValueOnce(null);
    const req = makeReq({ body: { subscription: fakeSub } });
    const result = await postSubscribeRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toMatch(/Failed to store subscription/i);
  });
});

// ── delete-subscribe ──────────────────────────────────────────────────────────

describe("deleteSubscribeRoute", () => {
  beforeEach(() => mockDeleteSubscriptionForUser.mockClear());

  it("deletes the caller's own subscription (scoped by session user) and returns success", async () => {
    const { deleteSubscribeRoute } = await import("./delete-subscribe");
    mockDeleteSubscriptionForUser.mockResolvedValueOnce(true);
    const req = makeReq({ params: { id: "sub1" } });
    const result = await deleteSubscribeRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
    // Scoped to the session user id ("u1" from makeReq), not just the row id.
    expect(mockDeleteSubscriptionForUser).toHaveBeenCalledWith("sub1", "u1");
  });

  it("returns failed when no subscription matched the id for this user", async () => {
    const { deleteSubscribeRoute } = await import("./delete-subscribe");
    mockDeleteSubscriptionForUser.mockResolvedValueOnce(false);
    const req = makeReq({ params: { id: "nonexistent" } });
    const result = await deleteSubscribeRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toMatch(/No subscription found/i);
  });
});
