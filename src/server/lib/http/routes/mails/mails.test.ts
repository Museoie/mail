/**
 * Tests for mails route handlers:
 *   get-headers, get-unified-headers, get-accounts, get-body, delete, post-mark,
 *   get-search, get-domain, get-allowlist, post-allowlist,
 *   delete-allowlist, post-send, post-spam-mark, get-attachment
 */
import { describe, it, expect, mock, spyOn, afterAll, beforeEach } from "bun:test";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import type { ApiResponse } from "../route";
import * as domainRepo from "../../../postgres/repositories/domains";
import {
  ALLOWLIST_COUNT_MAX,
  ALLOWLIST_PATTERN_MAX_BYTES,
} from "../../../postgres/models/spam_allowlist";
import { MAX_ATTACHMENTS_PER_MAIL } from "../../upload";
import type { HttpRequest } from "../../request";

// ── Shared mocks for "server" barrel ─────────────────────────────────────────

const mockGetMailHeaders = mock(async () => []);
// Records the envelope predicate each row is shown with.
const mockToMailHeaderData = mock((row: unknown, _isReaders: (address: string) => boolean) => row);
const mockGetMailHeadersDelta = mock(async () => ({ as_of: "", headers: [], expunged_ids: [] }));
const mockGetUnifiedMailHeaders = mock(async () => []);
const mockGetUnifiedMailHeadersDelta = mock(async () => ({ as_of: "", headers: [], expunged_ids: [] }));
const mockGetAccounts = mock(async () => ({ received: [], sent: [] }));
const mockGetMailBody = mock(async () => null as unknown);
// Matches the real `deleteMail(): Promise<boolean>` — a stub resolving
// `undefined` could never surface the true/false distinction the route reads.
const mockDeleteMail = mock(async () => true);
const mockMarkRead = mock(async () => true);
const mockMarkSaved = mock(async () => true);
const mockDecrementBadgeCount = mock(async () => {});
const mockNotifyFlagChange = mock(async () => {});
const mockGetUser = mock(async () => null as unknown);

// New mocks for additional routes
const mockSearchMail = mock(async () => []);
const mockGetDomain = mock(() => "example.com");
const mockGetAllowlistForUser = mock(async () => []);
const mockAddAllowlistEntry = mock(async () => null as unknown);
const mockRemoveAllowlistEntry = mock(async () => false as unknown);
const mockSendMail = mock(async () => {});
const mockMarkSpam = mock(async () => ({ found: false, changed: false }) as unknown);
const mockGetAttachmentFile = mock(async () => undefined as Bun.BunFile | undefined);
const mockGetMailHeaderPage = mock(async (..._args: unknown[]) => ({
  batches: oneBatch<unknown>([]),
  next: null as { date: string; id: string } | null,
}));
const mockGetUnifiedMailHeaderPage = mock(async (..._args: unknown[]) => ({
  batches: oneBatch<unknown>([]),
  next: null as { date: string; id: string } | null,
}));
const mockMailsTableQueryOne = mock(async () => null as unknown);
const AUTH_ERROR_MESSAGE = "Authentication required";

class MockMailValidationError extends Error {}
class MockMailSendingError extends Error {}

async function* oneBatch<R>(rows: R[]): AsyncGenerator<R[]> {
  yield rows;
}

const apiResult = async (result: unknown): Promise<unknown> =>
  result instanceof Response ? result.json() : result;

mock.module("server", () => ({
  getMailHeaders: mockGetMailHeaders,
  getMailHeaderPage: mockGetMailHeaderPage,
  getMailHeadersDelta: mockGetMailHeadersDelta,
  getUnifiedMailHeaders: mockGetUnifiedMailHeaders,
  getUnifiedMailHeaderPage: mockGetUnifiedMailHeaderPage,
  getUnifiedMailHeadersDelta: mockGetUnifiedMailHeadersDelta,
  toMailHeaderData: mockToMailHeaderData,
  getAccounts: mockGetAccounts,
  getMailBody: mockGetMailBody,
  deleteMail: mockDeleteMail,
  markRead: mockMarkRead,
  markSaved: mockMarkSaved,
  push: {
    decrementBadgeCount: mockDecrementBadgeCount,
    notifyFlagChange: mockNotifyFlagChange,
    // Other push.* methods are not exercised by mails routes; included so the
    // shared barrel mock is shape-compatible for any test file that lands
    // after this one in Bun's global mock.module hoist order.
    getPushPublicKey: mock(() => "vapid-public-key"),
    storeSubscription: mock(async () => null),
    refreshSubscription: mock(async () => null),
  },
  getUser: mockGetUser,
  logger: { error: mock(() => {}), info: mock(() => {}), warn: mock(() => {}) },
  searchMail: mockSearchMail,
  getDomain: mockGetDomain,
  getAllowlistForUser: mockGetAllowlistForUser,
  addAllowlistEntry: mockAddAllowlistEntry,
  // Real ceilings, for the same reason as the read-only constants below — the
  // route interpolates them into the refusal messages this file asserts on.
  ALLOWLIST_COUNT_MAX,
  ALLOWLIST_PATTERN_MAX_BYTES,
  removeAllowlistEntry: mockRemoveAllowlistEntry,
  sendMail: mockSendMail,
  markSpam: mockMarkSpam,
  getAttachmentFile: mockGetAttachmentFile,
  AUTH_ERROR_MESSAGE,
  MailValidationError: MockMailValidationError,
  MailSendingError: MockMailSendingError,
  mailsTable: { queryOne: mockMailsTableQueryOne },
  SpamAllowlistModel: class {},
  // Users/token route exports
  setUserInfo: mock(async () => null),
  isValidEmail: mock(() => true),
  createToken: mock(async () => ({ id: "u1", username: "u", token: "tok" })),
  getSignedUser: mock(() => null),
  createAuthenticationMail: mock(() => ({})),
  startTimer: mock(() => {}),
  version: "0.0.0",
}));

/**
 * The role a person holds on a domain, spied (restored in afterAll) so no
 * database is reached. By default the session user owns every domain, which
 * is what the per-account routes needed before domains were resources.
 */
const ownerOf = (userId: string, name: string): domainRepo.DomainAccess => ({
  domainId: `d-${name}`,
  name,
  ownerId: userId,
  ownerUsername: "alice",
  role: "owner",
  verifiedAt: null,
});
const mockGetDomainAccess = spyOn(domainRepo, "getDomainAccess").mockImplementation(
  async (userId, name) => ownerOf(userId, name)
);
const mockListDomainAccess = spyOn(domainRepo, "listDomainAccess").mockResolvedValue([]);
const mockListFormerDomains = spyOn(domainRepo, "listFormerDomains").mockResolvedValue([]);

// Mock logger used directly in post-mark / post-spam-mark
const mockLoggerError = mock(() => {});
const mockLoggerWarn = mock(() => {});
mock.module("../../../logger", () => ({
  logger: { debug: mock(() => {}), info: mock(() => {}), warn: mockLoggerWarn, error: mockLoggerError },
}));

// `mock.module` is process-global with no unmock API — hand the real barrel
// and logger back so the next file in the same run does not inherit the stubs.
afterAll(() => {
  const realServer = (globalThis as Record<string, unknown>).__REAL_SERVER as
    | Record<string, unknown>
    | undefined;
  mockGetDomainAccess.mockRestore();
  mockListDomainAccess.mockRestore();
  mockListFormerDomains.mockRestore();
  if (!realServer) return;
  mock.module("server", () => realServer);
  mock.module("../../../logger", () => ({ logger: realServer.logger, default: realServer.logger }));
});

// Mock deep imports used by post-spam-mark.ts (its trainWithEmail + getMailById
// reach into specific server submodules, not the "server" barrel).
const mockGetMailById = mock(async () => null as unknown);
const mockFindViewableMail = mock(
  async (..._args: unknown[]) => null as { ownerId: string; domains: string[] } | null
);
const mockCanViewAttachment = mock(async (..._args: unknown[]) => false);
mock.module("server/lib/postgres/repositories/mails", () => ({
  getMailById: mockGetMailById,
  findViewableMail: mockFindViewableMail,
  canViewAttachment: mockCanViewAttachment,
}));

const mockTrainWithEmail = mock(async () => undefined);
mock.module("server/lib/spam/classifier", () => ({
  trainWithEmail: mockTrainWithEmail,
}));

// ── Helper factories ──────────────────────────────────────────────────────────

const makeUser = (username = "alice", id = "u1") => ({ id, username });

const MAIL_ID = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
const MISSING_ID = "9c8b7a65-4d3e-42f1-8a09-1b2c3d4e5f60";
const OTHER_ID = "5e4d3c2b-1a09-4f8e-b7d6-c5b4a3928170";

const makeReq = (overrides: Record<string, unknown> = {}) =>
  ({
    method: "GET",
    session: { user: makeUser() },
    params: {},
    query: {},
    body: {},
    ...overrides,
  }) as unknown as HttpRequest;

// ── get-headers route ─────────────────────────────────────────────────────────

describe("getHeadersRoute", () => {
  beforeEach(() => {
    mockGetMailHeaders.mockClear();
    mockGetMailHeadersDelta.mockClear();
  });

  it("returns success with mail headers for valid user", async () => {
    const { getHeadersRoute } = await import("./get-headers");

    const fakeMails = [{ id: "m1", subject: "Hello" }];
    mockGetMailHeaders.mockResolvedValueOnce(oneBatch(fakeMails) as never);

    const req = makeReq({
      method: "GET",
      session: { user: makeUser("alice") },
      params: { account: "alice@example.com" },
      query: {},
    });

    const result = await apiResult(await getHeadersRoute.callback(req));
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect((result as ApiResponse<unknown>).body).toEqual(fakeMails);
  });

  it("reads the user's own rows, unfiltered, for a domain they own", async () => {
    const { getHeadersRoute } = await import("./get-headers");

    mockGetMailHeaders.mockResolvedValueOnce(oneBatch([]) as never);

    const req = makeReq({
      method: "GET",
      session: { user: makeUser("alice") },
      params: { account: "Info@Example.com" },
      query: {},
    });

    const result = await apiResult(await getHeadersRoute.callback(req));
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect(mockGetDomainAccess).toHaveBeenCalledWith("u1", "example.com");
    const [store, , options] = mockGetMailHeaders.mock.calls[0] as unknown as [
      { id: string },
      string,
      { domains?: string[] },
    ];
    expect(store.id).toBe("u1");
    expect(options.domains).toBeUndefined();
  });

  it("reads a viewed domain from its owner's rows, filtered to that domain", async () => {
    const { getHeadersRoute } = await import("./get-headers");

    mockGetMailHeaders.mockResolvedValueOnce(oneBatch([]) as never);
    mockGetDomainAccess.mockResolvedValueOnce({
      ...ownerOf("owner-1", "example.com"),
      role: "viewer",
    });

    const req = makeReq({
      method: "GET",
      session: { user: makeUser("alice") },
      params: { account: "info@example.com" },
      query: {},
    });

    const result = await apiResult(await getHeadersRoute.callback(req));
    expect((result as ApiResponse<unknown>).status).toBe("success");
    const [store, , options] = mockGetMailHeaders.mock.calls[0] as unknown as [
      { id: string },
      string,
      { domains?: string[] },
    ];
    expect(store.id).toBe("owner-1");
    expect(options.domains).toEqual(["example.com"]);
  });

  it("shows an owner's rows with the envelope recipients on their own and former domains", async () => {
    const { getHeadersRoute } = await import("./get-headers");

    mockGetMailHeaders.mockResolvedValueOnce(oneBatch([{ mail_id: "m1" }]) as never);
    mockListDomainAccess.mockResolvedValueOnce([
      ownerOf("u1", "alice.example.com"),
      ownerOf("u1", "example.org"),
      { ...ownerOf("owner-2", "example.net"), role: "viewer" },
    ]);
    mockListFormerDomains.mockResolvedValueOnce(["old.example"]);
    mockToMailHeaderData.mockClear();

    const req = makeReq({
      method: "GET",
      session: { user: makeUser("alice") },
      params: { account: "info@example.org" },
      query: {},
    });

    const result = await apiResult(await getHeadersRoute.callback(req));
    expect((result as ApiResponse<unknown>).status).toBe("success");
    const isReaders = mockToMailHeaderData.mock.calls[0][1];
    expect(isReaders("Info@Example.org")).toBe(true);
    expect(isReaders("me@alice.example.com")).toBe(true);
    expect(isReaders("kept@old.example")).toBe(true);
    // Another user's domain, even one alice views, is someone else's Bcc.
    expect(isReaders("bcc@example.net")).toBe(false);
    expect(isReaders("bcc@bob.example.com")).toBe(false);
  });

  it("shows a viewer only the envelope recipients on the domain they view", async () => {
    const { getHeadersRoute } = await import("./get-headers");

    mockGetMailHeaders.mockResolvedValueOnce(oneBatch([{ mail_id: "m1" }]) as never);
    mockGetDomainAccess.mockResolvedValueOnce({
      ...ownerOf("owner-1", "example.com"),
      role: "viewer",
    });
    mockToMailHeaderData.mockClear();

    const req = makeReq({
      method: "GET",
      session: { user: makeUser("alice") },
      params: { account: "info@example.com" },
      query: {},
    });

    await getHeadersRoute.callback(req);
    const isReaders = mockToMailHeaderData.mock.calls[0][1];
    expect(isReaders("info@example.com")).toBe(true);
    // The owner's other domains are not the viewer's to see.
    expect(isReaders("me@owner.example.com")).toBe(false);
  });

  it("reads the user's own rows for a domain they hold no role on (one they used to own)", async () => {
    const { getHeadersRoute } = await import("./get-headers");

    mockGetMailHeaders.mockResolvedValueOnce(oneBatch([]) as never);
    mockGetDomainAccess.mockResolvedValueOnce(null);

    const req = makeReq({
      method: "GET",
      session: { user: makeUser("alice") },
      params: { account: "info@example.net" },
      query: {},
    });

    const result = await apiResult(await getHeadersRoute.callback(req));
    expect((result as ApiResponse<unknown>).status).toBe("success");
    const [store, , options] = mockGetMailHeaders.mock.calls[0] as unknown as [
      { id: string },
      string,
      { domains?: string[] },
    ];
    // Only ever the user's own rows: nobody else's mail without a role.
    expect(store.id).toBe("u1");
    expect(options.domains).toBeUndefined();
  });

  it("returns failed for an account that is not an address", async () => {
    const { getHeadersRoute } = await import("./get-headers");

    const req = makeReq({
      method: "GET",
      session: { user: makeUser("alice") },
      params: { account: "no-at-sign" },
      query: {},
    });

    const result = await apiResult(await getHeadersRoute.callback(req));
    expect((result as ApiResponse<unknown>).status).toBe("failed");
  });

  it("passes query options (sent/new/saved) to getMailHeaders", async () => {
    const { getHeadersRoute } = await import("./get-headers");

    mockGetMailHeaders.mockResolvedValueOnce([]);

    const req = makeReq({
      method: "GET",
      session: { user: makeUser("alice") },
      params: { account: "alice@example.com" },
      query: { sent: "1" },
    });

    await getHeadersRoute.callback(req);

    const callArgs = mockGetMailHeaders.mock.calls[0];
    expect(callArgs[2]).toMatchObject({ sent: true, saved: false });
  });

  it("passes the ?spam= flag to getMailHeaders (per-account spam folder)", async () => {
    const { getHeadersRoute } = await import("./get-headers");

    mockGetMailHeaders.mockResolvedValueOnce([]);

    const req = makeReq({
      method: "GET",
      session: { user: makeUser("alice") },
      params: { account: "alice@example.com" },
      query: { spam: "1" },
    });

    await getHeadersRoute.callback(req);

    const callArgs = mockGetMailHeaders.mock.calls[0];
    expect(callArgs[2]).toMatchObject({ spam: true, sent: false });
  });

  it("without ?since= falls back to the full-list path (backwards-compat)", async () => {
    const { getHeadersRoute } = await import("./get-headers");

    mockGetMailHeaders.mockResolvedValueOnce(oneBatch([{ id: "m1" }]) as never);

    const req = makeReq({
      method: "GET",
      session: { user: makeUser("alice") },
      params: { account: "alice@example.com" },
      query: {},
    });

    const result = await apiResult(await getHeadersRoute.callback(req));
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect(mockGetMailHeaders).toHaveBeenCalledTimes(1);
    expect(mockGetMailHeadersDelta).not.toHaveBeenCalled();
  });

  it("with a valid ?since= returns the delta shape via getMailHeadersDelta", async () => {
    const { getHeadersRoute } = await import("./get-headers");

    const delta = { as_of: "2024-02-01T00:00:00.000Z", headers: [{ id: "m9" }], expunged_ids: ["x"] };
    mockGetMailHeadersDelta.mockResolvedValueOnce(delta as never);

    const since = "2024-01-01T00:00:00.000Z";
    const req = makeReq({
      method: "GET",
      session: { user: makeUser("alice") },
      params: { account: "alice@example.com" },
      query: { since },
    });

    const result = await apiResult(await getHeadersRoute.callback(req));
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect((result as ApiResponse<unknown>).body).toEqual(delta);
    expect(mockGetMailHeaders).not.toHaveBeenCalled();
    const callArgs = mockGetMailHeadersDelta.mock.calls[0];
    expect(callArgs[3]).toBe(since);
  });

  it("rejects a malformed ?since= at the boundary instead of full-fetching or hitting SQL", async () => {
    const { getHeadersRoute } = await import("./get-headers");


    const req = makeReq({
      method: "GET",
      session: { user: makeUser("alice") },
      params: { account: "alice@example.com" },
      query: { since: "not-a-date" },
    });

    const result = await apiResult(await getHeadersRoute.callback(req));
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect(mockGetMailHeadersDelta).not.toHaveBeenCalled();
    expect(mockGetMailHeaders).not.toHaveBeenCalled();
  });
});

// ── get-unified-headers route ─────────────────────────────────────────────────
describe("getUnifiedHeadersRoute", () => {
  beforeEach(() => {
    mockGetUnifiedMailHeaders.mockClear();
    mockGetUnifiedMailHeadersDelta.mockClear();
    mockGetAccounts.mockClear();
  });

  const receivedAccounts = {
    received: [{ key: "a@example.com" }, { key: "b@example.com" }],
    sent: [],
    spam: [],
  };

  it("returns success with unified headers for valid user", async () => {
    const { getUnifiedHeadersRoute } = await import("./get-unified-headers");

    const fakeMails = [{ id: "m1", subject: "Hello" }];
    mockGetAccounts.mockResolvedValueOnce(receivedAccounts as never);
    mockGetUnifiedMailHeaders.mockResolvedValueOnce(oneBatch(fakeMails) as never);

    const req = makeReq({
      method: "GET",
      session: { user: makeUser("alice") },
      params: {},
      query: {},
    });

    const result = await apiResult(await getUnifiedHeadersRoute.callback(req));
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect((result as ApiResponse<unknown>).body).toEqual(fakeMails);
  });

  it("passes the received account keys as the address set", async () => {
    const { getUnifiedHeadersRoute } = await import("./get-unified-headers");

    mockGetAccounts.mockResolvedValueOnce(receivedAccounts as never);
    mockGetUnifiedMailHeaders.mockResolvedValueOnce([] as never);

    const req = makeReq({
      method: "GET",
      session: { user: makeUser("alice") },
      params: {},
      query: {},
    });

    await getUnifiedHeadersRoute.callback(req);

    expect(mockGetAccounts).toHaveBeenCalledTimes(1);
    const callArgs = mockGetUnifiedMailHeaders.mock.calls[0];
    expect(callArgs[1]).toEqual(["a@example.com", "b@example.com"]);
  });

  it("passes an empty address set when the user has no received accounts", async () => {
    const { getUnifiedHeadersRoute } = await import("./get-unified-headers");

    mockGetAccounts.mockResolvedValueOnce({ received: [], sent: [], spam: [] } as never);
    mockGetUnifiedMailHeaders.mockResolvedValueOnce([] as never);

    const req = makeReq({
      method: "GET",
      session: { user: makeUser("alice") },
      params: {},
      query: {},
    });

    await getUnifiedHeadersRoute.callback(req);

    const callArgs = mockGetUnifiedMailHeaders.mock.calls[0];
    expect(callArgs[1]).toEqual([]);
  });

  it("without ?since= falls back to the full-list path (backwards-compat)", async () => {
    const { getUnifiedHeadersRoute } = await import("./get-unified-headers");

    mockGetAccounts.mockResolvedValueOnce(receivedAccounts as never);
    mockGetUnifiedMailHeaders.mockResolvedValueOnce(oneBatch([{ id: "m1" }]) as never);

    const req = makeReq({
      method: "GET",
      session: { user: makeUser("alice") },
      params: {},
      query: {},
    });

    const result = await apiResult(await getUnifiedHeadersRoute.callback(req));
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect(mockGetUnifiedMailHeaders).toHaveBeenCalledTimes(1);
    expect(mockGetUnifiedMailHeadersDelta).not.toHaveBeenCalled();
  });

  it("with a valid ?since= returns the delta shape via getUnifiedMailHeadersDelta", async () => {
    const { getUnifiedHeadersRoute } = await import("./get-unified-headers");

    const delta = { as_of: "2024-02-01T00:00:00.000Z", headers: [{ id: "m9" }], expunged_ids: ["x"] };
    mockGetAccounts.mockResolvedValueOnce(receivedAccounts as never);
    mockGetUnifiedMailHeadersDelta.mockResolvedValueOnce(delta as never);

    const since = "2024-01-01T00:00:00.000Z";
    const req = makeReq({
      method: "GET",
      session: { user: makeUser("alice") },
      params: {},
      query: { since },
    });

    const result = await apiResult(await getUnifiedHeadersRoute.callback(req));
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect((result as ApiResponse<unknown>).body).toEqual(delta);
    expect(mockGetUnifiedMailHeaders).not.toHaveBeenCalled();
    const callArgs = mockGetUnifiedMailHeadersDelta.mock.calls[0];
    expect(callArgs[1]).toEqual(["a@example.com", "b@example.com"]);
    expect(callArgs[2]).toBe(since);
  });

  it("rejects a malformed ?since= at the boundary instead of full-fetching or hitting SQL", async () => {
    const { getUnifiedHeadersRoute } = await import("./get-unified-headers");

    mockGetAccounts.mockResolvedValueOnce(receivedAccounts as never);

    const req = makeReq({
      method: "GET",
      session: { user: makeUser("alice") },
      params: {},
      query: { since: "not-a-date" },
    });

    const result = await apiResult(await getUnifiedHeadersRoute.callback(req));
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect(mockGetUnifiedMailHeadersDelta).not.toHaveBeenCalled();
    expect(mockGetUnifiedMailHeaders).not.toHaveBeenCalled();
  });
});

// ── get-accounts route ────────────────────────────────────────────────────────

describe("getAccountsRoute", () => {
  beforeEach(() => mockGetAccounts.mockClear());

  it("returns received and sent accounts", async () => {
    const { getAccountsRoute } = await import("./get-accounts");

    const fakeAccounts = {
      received: [{ address: "alice@example.com" }],
      sent: [{ address: "bob@example.com" }],
    };
    mockGetAccounts.mockResolvedValueOnce(fakeAccounts as never);

    const req = makeReq({ session: { user: makeUser("alice") } });

    const result = await getAccountsRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect((result as ApiResponse<unknown>).body).toMatchObject(fakeAccounts);
  });

  it("calls getAccounts with the session user", async () => {
    const { getAccountsRoute } = await import("./get-accounts");

    mockGetAccounts.mockResolvedValueOnce({ received: [], sent: [] } as never);
    const user = makeUser("bob");
    const req = makeReq({ session: { user } });

    await getAccountsRoute.callback(req);

    expect(mockGetAccounts).toHaveBeenCalledWith(user);
  });
});

// ── get-body route ────────────────────────────────────────────────────────────

describe("getBodyRoute", () => {
  beforeEach(() => {
    mockGetMailBody.mockClear();
    mockMarkRead.mockClear();
    mockNotifyFlagChange.mockClear();
  });

  it("returns mail body when found", async () => {
    const { getBodyRoute } = await import("./get-body");

    const fakeMail = { id: "m1", html: "<p>Hello</p>" };
    mockGetMailBody.mockResolvedValueOnce(fakeMail);

    const req = makeReq({ params: { id: MAIL_ID } });

    const result = await getBodyRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect((result as ApiResponse<unknown>).body).toEqual(fakeMail);
  });

  it("returns failed when mail is not found", async () => {
    const { getBodyRoute } = await import("./get-body");

    mockGetMailBody.mockResolvedValueOnce(null);

    const req = makeReq({ params: { id: MISSING_ID } });

    const result = await getBodyRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toBeTruthy();
  });

  it("marks the mail read when the body is served, so IMAP sees \\Seen", async () => {
    const { getBodyRoute } = await import("./get-body");

    const fakeMail = { id: "m1", html: "<p>Hello</p>" };
    mockGetMailBody.mockResolvedValueOnce(fakeMail);

    const req = makeReq({ params: { id: MAIL_ID } });
    const result = await getBodyRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect(mockMarkRead).toHaveBeenCalledTimes(1);
    expect(mockMarkRead).toHaveBeenCalledWith("u1", MAIL_ID);
  });

  it("notifies IDLE sessions after marking read on body fetch", async () => {
    const { getBodyRoute } = await import("./get-body");

    const fakeMail = { id: "m1", html: "<p>Hello</p>" };
    mockGetMailBody.mockResolvedValueOnce(fakeMail);
    mockMarkRead.mockResolvedValueOnce(true);

    const req = makeReq({ params: { id: MAIL_ID } });
    await getBodyRoute.callback(req);
    await new Promise((resolve) => setImmediate(resolve));
    expect(mockNotifyFlagChange).toHaveBeenCalledWith(["alice"], [MAIL_ID]);
  });

  it("still serves the body when the mark fails", async () => {
    const { getBodyRoute } = await import("./get-body");

    const fakeMail = { id: "m1", html: "<p>Hello</p>" };
    mockGetMailBody.mockResolvedValueOnce(fakeMail);
    mockMarkRead.mockRejectedValueOnce(new Error("db down"));

    const req = makeReq({ params: { id: MAIL_ID } });
    const result = await getBodyRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect((result as ApiResponse<unknown>).body).toEqual(fakeMail);
  });

  it("serves a mail of a viewed domain from its owner's rows, without marking it", async () => {
    const { getBodyRoute } = await import("./get-body");

    const fakeMail = { id: "m1", html: "<p>Hello</p>" };
    mockGetMailBody.mockImplementation((async (userId: string) =>
      userId === "owner-1" ? fakeMail : null) as never);
    mockFindViewableMail.mockResolvedValueOnce({ ownerId: "owner-1", domains: ["example.com"] });

    const result = await getBodyRoute.callback(makeReq({ params: { id: MAIL_ID } }));
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect((result as ApiResponse<unknown>).body).toEqual(fakeMail);
    expect(mockFindViewableMail).toHaveBeenCalledWith("u1", MAIL_ID);
    // The read state is the owner's, not the viewer's to change.
    expect(mockMarkRead).not.toHaveBeenCalled();
    mockGetMailBody.mockImplementation(async () => null);
  });

  it("refuses a mail that is neither the user's nor of a domain they view", async () => {
    const { getBodyRoute } = await import("./get-body");

    mockGetMailBody.mockResolvedValue(null);
    mockFindViewableMail.mockResolvedValueOnce(null);

    const result = await getBodyRoute.callback(makeReq({ params: { id: MAIL_ID } }));
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect(mockMarkRead).not.toHaveBeenCalled();
  });
});

// ── delete mail route ─────────────────────────────────────────────────────────

describe("deleteMailRoute", () => {
  beforeEach(() => {
    mockGetMailBody.mockClear();
    mockDeleteMail.mockClear();
  });

  it("deletes mail and returns success when mail belongs to user", async () => {
    const { deleteMailRoute } = await import("./delete");

    mockGetMailBody.mockResolvedValueOnce({ id: "m1" });

    const req = makeReq({ method: "DELETE", params: { id: MAIL_ID } });

    const result = await deleteMailRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect(mockDeleteMail).toHaveBeenCalledWith("u1", MAIL_ID);
  });

  it("returns failed when mail not found (or belongs to another user)", async () => {
    const { deleteMailRoute } = await import("./delete");

    mockGetMailBody.mockResolvedValueOnce(null);

    const req = makeReq({ method: "DELETE", params: { id: OTHER_ID } });

    const result = await deleteMailRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect(mockDeleteMail).not.toHaveBeenCalled();
  });
});

// ── post-mark route ───────────────────────────────────────────────────────────

describe("postMarkMailRoute", () => {
  beforeEach(() => {
    mockGetMailBody.mockClear();
    mockMarkRead.mockClear();
    mockMarkSaved.mockClear();
    mockDecrementBadgeCount.mockClear();
    mockNotifyFlagChange.mockClear();
  });

  it("marks mail as read and returns success", async () => {
    const { postMarkMailRoute } = await import("./post-mark");

    mockGetMailBody.mockResolvedValueOnce({ id: "m1" });

    const req = makeReq({
      method: "POST",
      body: { mail_id: MAIL_ID, read: true },
    });

    const result = await postMarkMailRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect(mockMarkRead).toHaveBeenCalledWith("u1", MAIL_ID);
  });

  it("notifies IDLE sessions of the flag change after marking read", async () => {
    const { postMarkMailRoute } = await import("./post-mark");

    mockGetMailBody.mockResolvedValueOnce({ id: "m1" });
    mockMarkRead.mockResolvedValueOnce(true);

    const req = makeReq({
      method: "POST",
      body: { mail_id: MAIL_ID, read: true },
    });

    await postMarkMailRoute.callback(req);
    // Flush the fire-and-forget notification.
    await new Promise((resolve) => setImmediate(resolve));
    expect(mockNotifyFlagChange).toHaveBeenCalledWith(["alice"], [MAIL_ID]);
  });

  it("notifies IDLE sessions of the flag change after marking saved", async () => {
    const { postMarkMailRoute } = await import("./post-mark");

    mockGetMailBody.mockResolvedValueOnce({ id: "m1" });
    mockMarkSaved.mockResolvedValueOnce(true);

    const req = makeReq({
      method: "POST",
      body: { mail_id: MAIL_ID, save: true },
    });

    await postMarkMailRoute.callback(req);
    await new Promise((resolve) => setImmediate(resolve));
    expect(mockNotifyFlagChange).toHaveBeenCalledWith(["alice"], [MAIL_ID]);
  });

  it("marks mail as saved when save=true", async () => {
    const { postMarkMailRoute } = await import("./post-mark");

    mockGetMailBody.mockResolvedValueOnce({ id: "m1" });

    const req = makeReq({
      method: "POST",
      body: { mail_id: MAIL_ID, save: true },
    });

    const result = await postMarkMailRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect(mockMarkSaved).toHaveBeenCalledWith("u1", MAIL_ID, true);
  });

  it("marks mail as unsaved when save=false", async () => {
    const { postMarkMailRoute } = await import("./post-mark");

    mockGetMailBody.mockResolvedValueOnce({ id: "m1" });

    const req = makeReq({
      method: "POST",
      body: { mail_id: MAIL_ID, save: false },
    });

    const result = await postMarkMailRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect(mockMarkSaved).toHaveBeenCalledWith("u1", MAIL_ID, false);
  });

  it("returns failed when mail not found", async () => {
    const { postMarkMailRoute } = await import("./post-mark");

    mockGetMailBody.mockResolvedValueOnce(null);

    const req = makeReq({
      method: "POST",
      body: { mail_id: MISSING_ID, read: true },
    });

    const result = await postMarkMailRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect(mockMarkRead).not.toHaveBeenCalled();
  });

  it("rejects a body with no mail_id before touching the repository (#790)", async () => {
    const { postMarkMailRoute } = await import("./post-mark");

    const req = makeReq({ method: "POST", body: { read: true } });

    const result = await postMarkMailRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    // An undefined mail_id is dropped from the SQL predicate at every layer
    // below, so the ownership lookup must not run at all — it would return an
    // arbitrary mail of the caller's and wave the request through.
    expect(mockGetMailBody).not.toHaveBeenCalled();
    expect(mockMarkRead).not.toHaveBeenCalled();
    expect(mockMarkSaved).not.toHaveBeenCalled();
  });

  it("rejects save=true / save=false with no mail_id (#790)", async () => {
    const { postMarkMailRoute } = await import("./post-mark");

    for (const save of [true, false]) {
      mockGetMailBody.mockClear();
      mockMarkSaved.mockClear();

      const req = makeReq({ method: "POST", body: { save } });
      const result = await postMarkMailRoute.callback(req);

      expect((result as ApiResponse<unknown>).status).toBe("failed");
      expect(mockGetMailBody).not.toHaveBeenCalled();
      expect(mockMarkSaved).not.toHaveBeenCalled();
    }
  });

  it("rejects a non-string or empty mail_id (#790)", async () => {
    const { postMarkMailRoute } = await import("./post-mark");

    for (const mail_id of [12345, { id: "m1" }, ["m1"], null, ""]) {
      mockGetMailBody.mockClear();

      const req = makeReq({ method: "POST", body: { mail_id, read: true } });
      const result = await postMarkMailRoute.callback(req);

      expect((result as ApiResponse<unknown>).status).toBe("failed");
      expect(mockGetMailBody).not.toHaveBeenCalled();
    }
  });

  it("rejects a non-object body (#790)", async () => {
    const { postMarkMailRoute } = await import("./post-mark");

    for (const body of [undefined, null, "mail_id=m1", 42, ["m1"]]) {
      mockGetMailBody.mockClear();

      const req = makeReq({ method: "POST", body });
      const result = await postMarkMailRoute.callback(req);

      expect((result as ApiResponse<unknown>).status).toBe("failed");
      expect(mockGetMailBody).not.toHaveBeenCalled();
    }
  });

  it("reports failed when markRead could not write, and leaves the badge alone", async () => {
    const { postMarkMailRoute } = await import("./post-mark");

    mockGetMailBody.mockResolvedValueOnce({ id: "m1" });
    mockMarkRead.mockResolvedValueOnce(false);

    const req = makeReq({ method: "POST", body: { mail_id: MAIL_ID, read: true } });
    const result = await postMarkMailRoute.callback(req);

    expect((result as ApiResponse<unknown>).status).toBe("failed");
    // Decrementing for a mail that stayed unread would desync the badge.
    expect(mockDecrementBadgeCount).not.toHaveBeenCalled();
  });

  it("reports failed when markSaved could not write", async () => {
    const { postMarkMailRoute } = await import("./post-mark");

    mockGetMailBody.mockResolvedValueOnce({ id: "m1" });
    mockMarkSaved.mockResolvedValueOnce(false);

    const req = makeReq({ method: "POST", body: { mail_id: MAIL_ID, save: true } });
    const result = await postMarkMailRoute.callback(req);

    expect((result as ApiResponse<unknown>).status).toBe("failed");
  });

  it("returns success for a body with a valid mail_id and neither read nor save", async () => {
    const { postMarkMailRoute } = await import("./post-mark");

    mockGetMailBody.mockResolvedValueOnce({ id: "m1" });

    const req = makeReq({ method: "POST", body: { mail_id: MAIL_ID } });
    const result = await postMarkMailRoute.callback(req);

    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect(mockMarkRead).not.toHaveBeenCalled();
    expect(mockMarkSaved).not.toHaveBeenCalled();
  });

  it("does not call markRead when read is not explicitly true", async () => {
    const { postMarkMailRoute } = await import("./post-mark");

    mockGetMailBody.mockResolvedValueOnce({ id: "m1" });

    const req = makeReq({
      method: "POST",
      body: { mail_id: MAIL_ID },
    });

    await postMarkMailRoute.callback(req);
    expect(mockMarkRead).not.toHaveBeenCalled();
  });

  it("fires push.decrementBadgeCount for the session user when read=true (#511)", async () => {
    const { postMarkMailRoute } = await import("./post-mark");

    mockGetMailBody.mockResolvedValueOnce({ id: "m1" });

    const req = makeReq({
      method: "POST",
      body: { mail_id: MAIL_ID, read: true },
    });

    await postMarkMailRoute.callback(req);

    expect(mockDecrementBadgeCount).toHaveBeenCalledTimes(1);
    expect(mockDecrementBadgeCount).toHaveBeenCalledWith([
      expect.objectContaining({ id: "u1" }),
    ]);
  });

  it("swallows push.decrementBadgeCount rejection — still marks read + succeeds (#511)", async () => {
    const { postMarkMailRoute } = await import("./post-mark");

    mockLoggerError.mockClear();
    mockGetMailBody.mockResolvedValueOnce({ id: "m1" });
    mockDecrementBadgeCount.mockRejectedValueOnce(new Error("push transient"));

    const req = makeReq({
      method: "POST",
      body: { mail_id: MAIL_ID, read: true },
    });

    const result = await postMarkMailRoute.callback(req);

    // markRead still ran — the badge-decrement rejection is logged, not propagated.
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect(mockMarkRead).toHaveBeenCalledWith("u1", MAIL_ID);

    // Give the .catch(logger.error) microtask a chance to run.
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
    expect(mockLoggerError).toHaveBeenCalledTimes(1);

    // Reset for the next iteration.
    mockDecrementBadgeCount.mockReset();
    mockDecrementBadgeCount.mockResolvedValue(undefined);
  });
});

// ── get-search route ──────────────────────────────────────────────────────────

describe("getSearchRoute", () => {
  beforeEach(() => mockSearchMail.mockClear());

  it("returns search results for a given value", async () => {
    const { getSearchRoute } = await import("./get-search");
    const fakeMails = [{ id: "m1", subject: "Test" }];
    mockSearchMail.mockResolvedValueOnce(fakeMails as never);

    const req = makeReq({ params: { value: "Test" }, query: {} });
    const result = await getSearchRoute.callback(req);

    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect((result as ApiResponse<unknown>).body).toEqual(fakeMails);
    expect(mockSearchMail).toHaveBeenCalledTimes(1);
  });

  it("passes field query param to searchMail", async () => {
    const { getSearchRoute } = await import("./get-search");
    mockSearchMail.mockResolvedValueOnce([]);

    const req = makeReq({ params: { value: "hello" }, query: { field: "subject" } });
    await getSearchRoute.callback(req);

    expect(mockSearchMail).toHaveBeenCalledWith(
      expect.objectContaining({ id: "u1" }),
      "hello",
      "subject"
    );
  });

  it("handles percent characters without throwing URIError", async () => {
    const { getSearchRoute } = await import("./get-search");
    mockSearchMail.mockResolvedValueOnce([]);

    const req = makeReq({ params: { value: "100% off" }, query: {} });
    const result = await getSearchRoute.callback(req);

    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect(mockSearchMail).toHaveBeenCalledWith(
      expect.objectContaining({ id: "u1" }),
      "100% off",
      undefined
    );
  });
});

// ── get-domain route ──────────────────────────────────────────────────────────

describe("getDomainRoute", () => {
  beforeEach(() => mockGetDomain.mockClear());

  it("returns the configured domain", async () => {
    const { getDomainRoute } = await import("./get-domain");
    const result = await getDomainRoute.callback(makeReq());

    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect((result as ApiResponse<unknown>).body).toBe("example.com");
  });
});

// ── get-allowlist route ───────────────────────────────────────────────────────

describe("getSpamAllowlistRoute", () => {
  beforeEach(() => mockGetAllowlistForUser.mockClear());

  it("returns mapped allowlist entries", async () => {
    const { getSpamAllowlistRoute } = await import("./get-allowlist");
    const entries = [
      { allowlist_id: "a1", pattern: "*@spam.com", created_at: "2026-01-01" },
    ];
    mockGetAllowlistForUser.mockResolvedValueOnce(entries as never);

    const result = await getSpamAllowlistRoute.callback(makeReq());

    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect((result as ApiResponse<unknown>).body).toEqual([
      { id: "a1", pattern: "*@spam.com", createdAt: "2026-01-01" },
    ]);
  });

  it("returns empty array when no entries", async () => {
    const { getSpamAllowlistRoute } = await import("./get-allowlist");
    mockGetAllowlistForUser.mockResolvedValueOnce([]);

    const result = await getSpamAllowlistRoute.callback(makeReq());

    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect((result as ApiResponse<unknown>).body).toEqual([]);
  });
});

// ── post-allowlist route ──────────────────────────────────────────────────────

describe("postSpamAllowlistRoute", () => {
  beforeEach(() => mockAddAllowlistEntry.mockClear());

  it("rejects missing pattern", async () => {
    const { postSpamAllowlistRoute } = await import("./post-allowlist");
    const req = makeReq({ body: {} });
    const result = await postSpamAllowlistRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toMatch(/pattern is required/i);
  });

  it("rejects invalid pattern format", async () => {
    const { postSpamAllowlistRoute } = await import("./post-allowlist");
    const req = makeReq({ body: { pattern: "notavalidemail" } });
    const result = await postSpamAllowlistRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toMatch(/email address/i);
  });

  it("returns failed when entry already exists", async () => {
    const { postSpamAllowlistRoute } = await import("./post-allowlist");
    mockAddAllowlistEntry.mockResolvedValueOnce({ status: "exists" });
    const req = makeReq({ body: { pattern: "*@spam.com" } });
    const result = await postSpamAllowlistRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toMatch(/already exists/i);
  });

  it("refuses an over-long pattern with a message naming the byte ceiling", async () => {
    const { postSpamAllowlistRoute } = await import("./post-allowlist");
    mockAddAllowlistEntry.mockResolvedValueOnce({ status: "too_long" });
    const localPart = "a".repeat(ALLOWLIST_PATTERN_MAX_BYTES);
    const req = makeReq({ body: { pattern: `${localPart}@spam.com` } });
    const result = await postSpamAllowlistRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toBe(
      `Pattern must be ${ALLOWLIST_PATTERN_MAX_BYTES} bytes or fewer`
    );
  });

  it("refuses at the row ceiling with a message naming the limit", async () => {
    const { postSpamAllowlistRoute } = await import("./post-allowlist");
    mockAddAllowlistEntry.mockResolvedValueOnce({ status: "at_limit" });
    const req = makeReq({ body: { pattern: "bad@spam.com" } });
    const result = await postSpamAllowlistRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toBe(
      `Allowlist limit of ${ALLOWLIST_COUNT_MAX} entries reached`
    );
  });

  it("distinguishes the two refusals — a duplicate is not reported as a limit", async () => {
    const { postSpamAllowlistRoute } = await import("./post-allowlist");
    mockAddAllowlistEntry.mockResolvedValueOnce({ status: "exists" });
    const req = makeReq({ body: { pattern: "bad@spam.com" } });
    const result = await postSpamAllowlistRoute.callback(req);
    expect((result as ApiResponse<unknown>).message).not.toMatch(/limit/i);
  });

  it("returns success with new entry on exact email pattern", async () => {
    const { postSpamAllowlistRoute } = await import("./post-allowlist");
    mockAddAllowlistEntry.mockResolvedValueOnce({
      status: "created",
      entry: {
        allowlist_id: "a2",
        pattern: "bad@spam.com",
        created_at: "2026-04-01",
      },
    });
    const req = makeReq({ body: { pattern: "bad@spam.com" } });
    const result = await postSpamAllowlistRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect((result as ApiResponse<unknown>).body).toMatchObject({ id: "a2", pattern: "bad@spam.com" });
  });

  it("returns success with domain wildcard pattern", async () => {
    const { postSpamAllowlistRoute } = await import("./post-allowlist");
    mockAddAllowlistEntry.mockResolvedValueOnce({
      status: "created",
      entry: {
        allowlist_id: "a3",
        pattern: "*@domain.com",
        created_at: "2026-04-01",
      },
    });
    const req = makeReq({ body: { pattern: "*@domain.com" } });
    const result = await postSpamAllowlistRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect((result as ApiResponse<unknown>).body).toMatchObject({ pattern: "*@domain.com" });
  });
});

// ── delete-allowlist route ────────────────────────────────────────────────────

describe("deleteSpamAllowlistRoute", () => {
  beforeEach(() => mockRemoveAllowlistEntry.mockClear());

  it("returns failed when entry not found", async () => {
    const { deleteSpamAllowlistRoute } = await import("./delete-allowlist");
    mockRemoveAllowlistEntry.mockResolvedValueOnce(false);
    const req = makeReq({ params: { pattern: "*@spam.com" } });
    const result = await deleteSpamAllowlistRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toMatch(/not found/i);
  });

  it("returns success when entry removed", async () => {
    const { deleteSpamAllowlistRoute } = await import("./delete-allowlist");
    mockRemoveAllowlistEntry.mockResolvedValueOnce(true);
    const req = makeReq({ params: { pattern: "*@spam.com" } });
    const result = await deleteSpamAllowlistRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
  });

  it("handles patterns containing percent chars without throwing", async () => {
    const { deleteSpamAllowlistRoute } = await import("./delete-allowlist");
    mockRemoveAllowlistEntry.mockResolvedValueOnce(true);
    const req = makeReq({ params: { pattern: "100%match@spam.com" } });
    const result = await deleteSpamAllowlistRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
  });
});

// ── post-send route ───────────────────────────────────────────────────────────

describe("postSendMailRoute", () => {
  beforeEach(() => mockSendMail.mockClear());

  it("returns success when mail is sent", async () => {
    const { postSendMailRoute } = await import("./post-send");
    mockSendMail.mockResolvedValueOnce(undefined);
    const req = makeReq({
      body: { to: "test@example.com", subject: "Hi", text: "Hello" },
    });
    const result = await postSendMailRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
  });

  it("returns failed on MailValidationError", async () => {
    const { postSendMailRoute } = await import("./post-send");
    mockSendMail.mockRejectedValueOnce(new MockMailValidationError("bad mail"));
    const req = makeReq({ body: { to: "x@x.com", subject: "s", text: "t" } });
    const result = await postSendMailRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toBe("bad mail");
  });

  it("returns failed on MailSendingError", async () => {
    const { postSendMailRoute } = await import("./post-send");
    mockSendMail.mockRejectedValueOnce(new MockMailSendingError("send failed"));
    const req = makeReq({ body: { to: "x@x.com", subject: "s", text: "t" } });
    const result = await postSendMailRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toBe("send failed");
  });

  it("rethrows unknown errors", async () => {
    const { postSendMailRoute } = await import("./post-send");
    const unknownError = new Error("unexpected");
    mockSendMail.mockRejectedValueOnce(unknownError);
    const req = makeReq({ body: { to: "x@x.com", subject: "s", text: "t" } });
    await expect(postSendMailRoute.callback(req)).rejects.toThrow("unexpected");
  });
});

// ── post-spam-mark route ──────────────────────────────────────────────────────

describe("postMarkSpamMailRoute", () => {
  beforeEach(() => {
    mockMarkSpam.mockClear();
    mockGetMailById.mockClear();
    mockTrainWithEmail.mockClear();
    mockLoggerWarn.mockClear();
  });

  it("rejects when is_spam is not boolean", async () => {
    const { postMarkSpamMailRoute } = await import("./post-spam-mark");
    const req = makeReq({ body: { mail_id: MAIL_ID, is_spam: "yes" } });
    const result = await postMarkSpamMailRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toMatch(/boolean/i);
    expect(mockMarkSpam).not.toHaveBeenCalled();
  });

  it("returns failed when mail not found or no permission (cross-user gate)", async () => {
    const { postMarkSpamMailRoute } = await import("./post-spam-mark");
    mockMarkSpam.mockResolvedValueOnce({ found: false, changed: false });
    const req = makeReq({ body: { mail_id: MAIL_ID, is_spam: true } });
    const result = await postMarkSpamMailRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toMatch(/not found/i);
    // markSpam was reached with the session user_id — the ownership gate is at
    // the repo layer; assert it was passed.
    expect(mockMarkSpam).toHaveBeenCalledWith("u1", MAIL_ID, true);
    expect(mockGetMailById).not.toHaveBeenCalled();
    expect(mockTrainWithEmail).not.toHaveBeenCalled();
  });

  it("returns success when spam marked + fires trainWithEmail (#511)", async () => {
    const { postMarkSpamMailRoute } = await import("./post-spam-mark");
    mockMarkSpam.mockResolvedValueOnce({ found: true, changed: true });
    mockGetMailById.mockResolvedValueOnce({
      subject: "Win cash now",
      text: "Click here",
      html: "<p>Click here</p>",
      from_address: [{ address: "spammer@evil.example" }],
    });
    const req = makeReq({ body: { mail_id: MAIL_ID, is_spam: true } });
    const result = await postMarkSpamMailRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");

    // Wait for the fire-and-forget training pipeline.
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
    expect(mockGetMailById).toHaveBeenCalledWith("u1", MAIL_ID);
    expect(mockTrainWithEmail).toHaveBeenCalledTimes(1);
    expect(mockTrainWithEmail).toHaveBeenCalledWith(
      "u1",
      expect.objectContaining({
        subject: "Win cash now",
        fromAddress: "spammer@evil.example",
      }),
      true,
    );
  });

  it("returns success when spam unmarked (is_spam false) + train with false (#511)", async () => {
    const { postMarkSpamMailRoute } = await import("./post-spam-mark");
    mockMarkSpam.mockResolvedValueOnce({ found: true, changed: true });
    mockGetMailById.mockResolvedValueOnce({
      subject: "Newsletter",
      text: "Updates",
      html: "<p>Updates</p>",
      from_address: [{ address: "news@legit.example" }],
    });
    const req = makeReq({ body: { mail_id: MAIL_ID, is_spam: false } });
    const result = await postMarkSpamMailRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");

    await new Promise<void>((resolve) => setTimeout(resolve, 0));
    expect(mockTrainWithEmail).toHaveBeenCalledWith(
      "u1",
      expect.objectContaining({ fromAddress: "news@legit.example" }),
      false,
    );
  });

  it("returns success but skips training when re-marking with the same value (#491)", async () => {
    const { postMarkSpamMailRoute } = await import("./post-spam-mark");
    mockMarkSpam.mockResolvedValueOnce({ found: true, changed: false });
    const req = makeReq({ body: { mail_id: MAIL_ID, is_spam: true } });
    const result = await postMarkSpamMailRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");

    await new Promise<void>((resolve) => setTimeout(resolve, 0));
    expect(mockGetMailById).not.toHaveBeenCalled();
    expect(mockTrainWithEmail).not.toHaveBeenCalled();
  });

  it("does not fire trainWithEmail when getMailById returns null (race) (#511)", async () => {
    const { postMarkSpamMailRoute } = await import("./post-spam-mark");
    mockMarkSpam.mockResolvedValueOnce({ found: true, changed: true });
    mockGetMailById.mockResolvedValueOnce(null);
    const req = makeReq({ body: { mail_id: MAIL_ID, is_spam: true } });
    const result = await postMarkSpamMailRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");

    await new Promise<void>((resolve) => setTimeout(resolve, 0));
    expect(mockGetMailById).toHaveBeenCalledTimes(1);
    expect(mockTrainWithEmail).not.toHaveBeenCalled();
  });

  it("swallows trainWithEmail rejection — response still success, logger.warn fired (#511)", async () => {
    const { postMarkSpamMailRoute } = await import("./post-spam-mark");
    mockMarkSpam.mockResolvedValueOnce({ found: true, changed: true });
    mockGetMailById.mockResolvedValueOnce({
      subject: "x",
      text: "y",
      html: "<p>y</p>",
      from_address: [{ address: "a@b.example" }],
    });
    mockTrainWithEmail.mockRejectedValueOnce(new Error("classifier offline"));
    const req = makeReq({ body: { mail_id: MAIL_ID, is_spam: true } });
    const result = await postMarkSpamMailRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");

    await new Promise<void>((resolve) => setTimeout(resolve, 0));
    expect(mockLoggerWarn).toHaveBeenCalledTimes(1);
  });

  it("handles non-array from_address shape safely (#511)", async () => {
    const { postMarkSpamMailRoute } = await import("./post-spam-mark");
    mockMarkSpam.mockResolvedValueOnce({ found: true, changed: true });
    // Some mail rows store from_address as null or an object, not always array.
    mockGetMailById.mockResolvedValueOnce({
      subject: "x",
      text: "y",
      html: "<p>y</p>",
      from_address: null,
    });
    const req = makeReq({ body: { mail_id: MAIL_ID, is_spam: true } });
    const result = await postMarkSpamMailRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");

    await new Promise<void>((resolve) => setTimeout(resolve, 0));
    expect(mockTrainWithEmail).toHaveBeenCalledTimes(1);
    expect(mockTrainWithEmail).toHaveBeenCalledWith(
      "u1",
      expect.objectContaining({ fromAddress: undefined }),
      true,
    );
  });

  it("handles empty from_address array safely (#511)", async () => {
    const { postMarkSpamMailRoute } = await import("./post-spam-mark");
    mockMarkSpam.mockResolvedValueOnce({ found: true, changed: true });
    mockGetMailById.mockResolvedValueOnce({
      subject: "x",
      text: "y",
      html: "<p>y</p>",
      from_address: [],
    });
    const req = makeReq({ body: { mail_id: MAIL_ID, is_spam: true } });
    const result = await postMarkSpamMailRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");

    await new Promise<void>((resolve) => setTimeout(resolve, 0));
    expect(mockTrainWithEmail).toHaveBeenCalledWith(
      "u1",
      expect.objectContaining({ fromAddress: undefined }),
      true,
    );
  });
});

// ── get-attachment route ──────────────────────────────────────────────────────

describe("getAttachmentRoute", () => {
  beforeEach(() => {
    mockMailsTableQueryOne.mockClear();
    mockGetAttachmentFile.mockClear();
  });

  it("returns failed with auth error when no session user", async () => {
    const { getAttachmentRoute } = await import("./get-attachment");
    const req = makeReq({ session: { user: undefined } });
    const result = await getAttachmentRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toBe(AUTH_ERROR_MESSAGE);
  });

  it("returns failed when mail not found (IDOR protection)", async () => {
    const { getAttachmentRoute } = await import("./get-attachment");
    mockMailsTableQueryOne.mockResolvedValueOnce(null);
    mockCanViewAttachment.mockResolvedValueOnce(false);
    const req = makeReq({ params: { id: "att1" } });
    const result = await getAttachmentRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toBe("Not found");
    expect(mockGetAttachmentFile).not.toHaveBeenCalled();
  });

  it("serves an attachment of a mail in a domain the user views, by its id alone", async () => {
    // The client asks by attachment id only, as for the user's own mail.
    const { getAttachmentRoute } = await import("./get-attachment");
    const file = { name: "shared.pdf" } as unknown as Bun.BunFile;
    mockMailsTableQueryOne.mockResolvedValueOnce(null);
    mockCanViewAttachment.mockResolvedValueOnce(true);
    mockGetAttachmentFile.mockResolvedValueOnce(file);
    const result = await getAttachmentRoute.callback(makeReq({ params: { id: "att1" } }));
    expect(result).toBe(file);
    expect(mockCanViewAttachment).toHaveBeenCalledWith("u1", "att1");
  });

  // Regression: an expunged mail's attachments were still served.
  it("looks the attachment up among the user's mails that are not expunged", async () => {
    const { getAttachmentRoute } = await import("./get-attachment");
    mockMailsTableQueryOne.mockResolvedValueOnce(null);
    const req = makeReq({ params: { id: "att1" } });
    await getAttachmentRoute.callback(req);
    expect(mockMailsTableQueryOne).toHaveBeenCalledWith({
      user_id: req.session.user!.id,
      attachments: [{ content: { data: "att1" } }],
      expunged: false,
    });
  });

  it("returns failed when attachment file missing from disk", async () => {
    const { getAttachmentRoute } = await import("./get-attachment");
    mockMailsTableQueryOne.mockResolvedValueOnce({ id: "m1" });
    mockGetAttachmentFile.mockResolvedValueOnce(undefined);
    const req = makeReq({ params: { id: "att1" } });
    const result = await getAttachmentRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
  });

  it("streams the stored file without reading it into memory", async () => {
    const { getAttachmentRoute } = await import("./get-attachment");
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "get-attachment-"));
    const filePath = path.join(dir, "att1");
    const content = Buffer.alloc(300_000);
    for (let i = 0; i < content.length; i += 1) content[i] = (i * 13) % 251;
    fs.writeFileSync(filePath, content);
    try {
      const file = Bun.file(filePath);
      mockMailsTableQueryOne.mockResolvedValueOnce({ id: "m1" });
      mockGetAttachmentFile.mockResolvedValueOnce(file);
      const req = makeReq({ params: { id: "att1" } });
      expect(await getAttachmentRoute.callback(req)).toBe(file);

      mockMailsTableQueryOne.mockResolvedValueOnce({ id: "m1" });
      mockGetAttachmentFile.mockResolvedValueOnce(file);
      const response = await getAttachmentRoute.handler(req);
      expect(response.status).toBe(200);
      expect(response.headers.get("content-type")).toBe("application/octet-stream");
      expect(response.headers.get("etag")).toBe(
        `W/"${content.length.toString(16)}-${file.lastModified.toString(16)}"`
      );
      expect(Buffer.from(await response.arrayBuffer()).equals(content)).toBe(true);
    } finally {
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });
});

describe("mail routes: a DB fault reaches the route boundary", () => {
  // A DB fault in the repository must propagate to `Route.handler` — swallowing
  // it and returning the same falsy value that means "no row matched" would
  // reach the user as "not found or you don't have permission", an authorization
  // verdict on a mail the user owns. The route boundary logs, alarms and answers
  // 500. These assert the callback rejects rather than resolving to a not-found
  // body.
  beforeEach(() => {
    mockMarkSpam.mockClear();
    mockMarkRead.mockClear();
    mockMarkSaved.mockClear();
    mockGetMailBody.mockClear();
    mockDeleteMail.mockClear();
  });

  it("post-spam-mark propagates instead of answering not-found", async () => {
    const { postMarkSpamMailRoute } = await import("./post-spam-mark");
    mockMarkSpam.mockRejectedValueOnce(new Error("connection terminated"));
    const req = makeReq({ body: { mail_id: MAIL_ID, is_spam: true } });
    await expect(
      postMarkSpamMailRoute.callback(req)
    ).rejects.toThrow("connection terminated");
  });

  it("post-mark read propagates instead of answering failed-to-mark", async () => {
    const { postMarkMailRoute } = await import("./post-mark");
    mockGetMailBody.mockResolvedValueOnce({ text: "t", html: "h" });
    mockMarkRead.mockRejectedValueOnce(new Error("connection terminated"));
    const req = makeReq({ body: { mail_id: MAIL_ID, read: true } });
    await expect(
      postMarkMailRoute.callback(req)
    ).rejects.toThrow("connection terminated");
  });

  it("post-mark save propagates instead of answering failed-to-update", async () => {
    const { postMarkMailRoute } = await import("./post-mark");
    mockGetMailBody.mockResolvedValueOnce({ text: "t", html: "h" });
    mockMarkSaved.mockRejectedValueOnce(new Error("connection terminated"));
    const req = makeReq({ body: { mail_id: MAIL_ID, save: true } });
    await expect(
      postMarkMailRoute.callback(req)
    ).rejects.toThrow("connection terminated");
  });

  it("get-body propagates instead of answering no-email-is-found", async () => {
    const { getBodyRoute } = await import("./get-body");
    mockGetMailBody.mockRejectedValueOnce(new Error("connection terminated"));
    const req = makeReq({ params: { id: MAIL_ID } });
    await expect(
      getBodyRoute.callback(req)
    ).rejects.toThrow("connection terminated");
  });

  it("delete propagates instead of answering success for a delete that never landed", async () => {
    const { deleteMailRoute } = await import("./delete");
    mockGetMailBody.mockResolvedValueOnce({ text: "t", html: "h" });
    mockDeleteMail.mockRejectedValueOnce(new Error("connection terminated"));
    const req = makeReq({ params: { id: MAIL_ID } });
    await expect(
      deleteMailRoute.callback(req)
    ).rejects.toThrow("connection terminated");
  });
});

describe("mail routes reject a malformed mail_id before it reaches Postgres (#747)", () => {
  // `mails.mail_id` is a uuid column, so Postgres answers a malformed value
  // with a 22P02 error rather than an empty result. Once the repository stopped
  // swallowing errors, an id of the wrong shape — a stale bookmark, an old
  // client's cached id — would have become a 500 plus an alarm page. These pin
  // that the shape check happens first and the repository is never called.
  const BAD_IDS = ["not-a-uuid", "", "123", "'; DROP TABLE mails; --"];

  beforeEach(() => {
    mockGetMailBody.mockClear();
    mockDeleteMail.mockClear();
    mockMarkSpam.mockClear();
    mockMarkRead.mockClear();
  });

  it.each(BAD_IDS)("get-body rejects %p without querying", async (id) => {
    const { getBodyRoute } = await import("./get-body");
    const result = await getBodyRoute.callback(makeReq({ params: { id } }));
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect(mockGetMailBody).not.toHaveBeenCalled();
  });

  it.each(BAD_IDS)("delete rejects %p without querying", async (id) => {
    const { deleteMailRoute } = await import("./delete");
    const result = await deleteMailRoute.callback(
      makeReq({ params: { id } }));
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect(mockGetMailBody).not.toHaveBeenCalled();
    expect(mockDeleteMail).not.toHaveBeenCalled();
  });

  it.each(BAD_IDS)("post-mark rejects %p without querying", async (id) => {
    const { postMarkMailRoute } = await import("./post-mark");
    const result = await postMarkMailRoute.callback(
      makeReq({ body: { mail_id: id, read: true } }));
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect(mockGetMailBody).not.toHaveBeenCalled();
    expect(mockMarkRead).not.toHaveBeenCalled();
  });

  it.each(BAD_IDS)("post-spam-mark rejects %p without querying", async (id) => {
    const { postMarkSpamMailRoute } = await import("./post-spam-mark");
    const result = await postMarkSpamMailRoute.callback(
      makeReq({ body: { mail_id: id, is_spam: true } }));
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect(mockMarkSpam).not.toHaveBeenCalled();
  });

  it("a well-formed uuid still reaches the repository", async () => {
    const { getBodyRoute } = await import("./get-body");
    mockGetMailBody.mockResolvedValueOnce({ id: "m1" });
    const id = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
    const result = await getBodyRoute.callback(makeReq({ params: { id } }));
    expect(mockGetMailBody).toHaveBeenCalledWith("u1", id);
    expect((result as ApiResponse<unknown>).status).toBe("success");
  });
});

// ── upload wiring on the real mails router ────────────────────────────────────

describe("mails router multipart wiring", () => {
  const BOUNDARY = "----mailsuploadtest";

  const ownTempFiles = async () => {
    const { readdirSync } = await import("fs");
    return new Set(readdirSync("/tmp").filter((name) => name.startsWith("upload-")));
  };

  /** Temp files created since `before` that still exist; the unlink is asynchronous. */
  const leftoverSince = async (before: Set<string>) => {
    for (let attempt = 0; attempt < 50; attempt++) {
      const left = [...(await ownTempFiles())].filter((name) => !before.has(name));
      if (left.length === 0) return left;
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    return [...(await ownTempFiles())].filter((name) => !before.has(name));
  };

  beforeEach(() => {
    mockSendMail.mockClear();
  });

  const part = (index: number, payload: string) =>
    `--${BOUNDARY}\r\n` +
    `Content-Disposition: form-data; name="attachments"; filename="p${index}.txt"\r\n` +
    `Content-Type: text/plain\r\n\r\n${payload}\r\n`;

  const sendRequest = (
    body: BodyInit,
    sessionUser?: { id: string; username: string }
  ): import("../../request").HttpRequest => {
    const raw = new Request("http://localhost/api/mails/send", {
      method: "POST",
      headers: { "Content-Type": `multipart/form-data; boundary=${BOUNDARY}` },
      body,
    });
    return {
      method: "POST",
      path: "/api/mails/send",
      session: sessionUser ? { user: sessionUser } : {},
      params: {},
      query: {},
      body: {},
      headers: raw.headers,
      raw,
    } as unknown as import("../../request").HttpRequest;
  };

  const postParts = async (count: number, sessionUser?: { id: string; username: string }) => {
    const handleMails = (await import("./index")).default;
    const body =
      Array.from({ length: count }, (_, i) => part(i, `payload-${i}`)).join("") + `--${BOUNDARY}--\r\n`;
    return (await handleMails(sendRequest(body, sessionUser), "/send"))!;
  };

  // Behind the auth gate, the 401 lands before a byte of the body is read; a
  // body that records every read is what separates that from a parser that
  // ran first and cleaned up afterwards.
  it("does not read a part while the request is still unauthenticated", async () => {
    const handleMails = (await import("./index")).default;
    const before = await ownTempFiles();
    let pulled = false;
    const held = new ReadableStream<Uint8Array>(
      {
        pull(controller) {
          pulled = true;
          controller.enqueue(new TextEncoder().encode(part(0, "x".repeat(4096))));
        },
      },
      { highWaterMark: 0 }
    );

    const res = (await handleMails(sendRequest(held), "/send"))!;

    expect([res.status, pulled]).toEqual([401, false]);
    expect(await leftoverSince(before)).toEqual([]);
    expect(mockSendMail).not.toHaveBeenCalled();
  });

  it("parses attachments for an authenticated send and reclaims them", async () => {
    const before = await ownTempFiles();
    mockSendMail.mockResolvedValueOnce(undefined as never);

    const res = await postParts(2, makeUser());

    expect(await res.json()).toEqual({ status: "success" });
    const attachments = mockSendMail.mock.calls[0]?.[2] as { name: string }[];
    expect(attachments.map((file) => file.name)).toEqual(["p0.txt", "p1.txt"]);
    expect(await leftoverSince(before)).toEqual([]);
  });

  it("rejects a send carrying more attachments than the cap without sending it", async () => {
    const before = await ownTempFiles();

    const res = await postParts(MAX_ATTACHMENTS_PER_MAIL + 5, makeUser());
    const body = (await res.json()) as ApiResponse<unknown>;

    expect(body.status).toBe("failed");
    expect(body.message).toMatch(new RegExp(`${MAX_ATTACHMENTS_PER_MAIL} attachments`));
    expect(mockSendMail).not.toHaveBeenCalled();
    expect(await leftoverSince(before)).toEqual([]);
  });
});

// ── paged list (?limit= / ?cursor=) ──────────────────────────────────────────
describe("paged list routes", () => {
  const CURSOR = { date: "2026-11-15 02:55:04.123456+00", id: "22222222-2222-2222-2222-222222222222" };
  const encoded = Buffer.from(JSON.stringify(CURSOR)).toString("base64url");

  beforeEach(() => {
    mockGetMailHeaders.mockClear();
    mockGetMailHeaderPage.mockClear();
    mockGetUnifiedMailHeaders.mockClear();
    mockGetUnifiedMailHeaderPage.mockClear();
    mockGetAccounts.mockClear();
  });

  const headersReq = (query: Record<string, string>) =>
    makeReq({
      method: "GET",
      session: { user: makeUser("alice") },
      params: { account: "alice@example.com" },
      query,
    });

  it("answers ?limit= with one page and the next cursor, not the full list", async () => {
    const { getHeadersRoute } = await import("./get-headers");
    mockGetMailHeaderPage.mockResolvedValueOnce({ batches: oneBatch<unknown>([{ id: "m1" }]), next: CURSOR });

    const result = (await apiResult(await getHeadersRoute.callback(headersReq({ limit: "100", sent: "1" })))) as ApiResponse<unknown>;

    expect(result).toEqual({ status: "success", body: { next_cursor: encoded, headers: [{ id: "m1" }] } });
    expect(mockGetMailHeaders).not.toHaveBeenCalled();
    const [, account, options, limit, after] = mockGetMailHeaderPage.mock.calls[0];
    expect(account).toBe("alice@example.com");
    expect(options).toEqual({ sent: true, saved: false, spam: false });
    expect(limit).toBe(100);
    expect(after).toBeUndefined();
  });

  it("passes ?cursor= on as the position to start after, and ends with a null cursor", async () => {
    const { getHeadersRoute } = await import("./get-headers");

    const result = (await apiResult(await getHeadersRoute.callback(headersReq({ limit: "7", cursor: encoded })))) as ApiResponse<unknown>;

    expect(result).toEqual({ status: "success", body: { next_cursor: null, headers: [] } });
    expect(mockGetMailHeaderPage.mock.calls[0][4]).toEqual(CURSOR);
  });

  it("refuses a bad limit or cursor instead of answering with the full list", async () => {
    const { getHeadersRoute } = await import("./get-headers");
    for (const query of [{ limit: "0" }, { limit: "100", cursor: "nope" }, { cursor: encoded }]) {
      const result = (await apiResult(await getHeadersRoute.callback(headersReq(query)))) as ApiResponse<unknown>;
      expect(result.status).toBe("failed");
    }
    expect(mockGetMailHeaders).not.toHaveBeenCalled();
    expect(mockGetMailHeaderPage).not.toHaveBeenCalled();
  });

  it("pages the unified list over the received account keys", async () => {
    const { getUnifiedHeadersRoute } = await import("./get-unified-headers");
    mockGetAccounts.mockResolvedValueOnce({ received: [{ key: "a@example.com" }], sent: [], spam: [] } as never);
    mockGetUnifiedMailHeaderPage.mockResolvedValueOnce({ batches: oneBatch<unknown>([{ id: "u1" }]), next: CURSOR });

    const req = makeReq({ method: "GET", session: { user: makeUser("alice") }, params: {}, query: { limit: "50", cursor: encoded } });
    const result = (await apiResult(await getUnifiedHeadersRoute.callback(req))) as ApiResponse<unknown>;

    expect(result).toEqual({ status: "success", body: { next_cursor: encoded, headers: [{ id: "u1" }] } });
    expect(mockGetUnifiedMailHeaders).not.toHaveBeenCalled();
    const [, addresses, limit, after] = mockGetUnifiedMailHeaderPage.mock.calls[0];
    expect(addresses).toEqual(["a@example.com"]);
    expect(limit).toBe(50);
    expect(after).toEqual(CURSOR);
  });
});
