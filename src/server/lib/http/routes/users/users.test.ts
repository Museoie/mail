/**
 * Tests for user route handlers: post-login, delete-login, get-login,
 * post-set-info, post-token
 */
import { describe, it, expect, mock, beforeEach, afterAll, spyOn } from "bun:test";
import { restoreLeaves } from "test-helpers";
import * as rateLimit from "../../rate-limit";
import type { HttpRequest } from "../../request";

import * as domains from "../../../domains";
import * as sendModule from "../../../mails/send";

// ── Mocks ─────────────────────────────────────────────────────────────────────

const mockGetUser = mock(async () => null as unknown);
const mockSetUserInfo = mock(async () => null as unknown);
const mockIsValidEmail = mock((_email: string) => true);
const mockCreateToken = mock(async () => ({ id: "u1", username: "alice", token: "tok123" }));
const mockGetSignedUser = mock((_user: unknown) => null as unknown);
const mockCreateAuthenticationMail = mock(() => ({ to: "test@example.com", subject: "auth" }));
const mockSendMail = mock(async () => {});
const mockStartTimer = mock((_id: string) => {});
const mockLogger = {
  debug: mock(() => {}),
  info: mock(() => {}),
  warn: mock(() => {}),
  error: mock(() => {}),
};
const TEST_VERSION = "1.2.3";

mock.module("server", () => ({
  getUser: mockGetUser,
  setUserInfo: mockSetUserInfo,
  isValidEmail: mockIsValidEmail,
  createToken: mockCreateToken,
  getSignedUser: mockGetSignedUser,
  createAuthenticationMail: mockCreateAuthenticationMail,
  sendMail: mockSendMail,
  startTimer: mockStartTimer,
  logger: mockLogger,
  version: TEST_VERSION,
}));

/**
 * Whether anyone but an address's own account can read the mail delivered to
 * it, and the server's own sender, spied (restored in afterAll): what this
 * file tests is the routes' wiring; the rule itself is tested in
 * domains.test.ts against the real lookups.
 */
const mockOthersCanRead = spyOn(domains, "othersCanReadAddress").mockResolvedValue(false);
const mockSendSystemMail = spyOn(sendModule, "sendSystemMail").mockResolvedValue(undefined);

mock.module("../../../logger", () => ({
  logger: { debug: mock(() => {}), info: mock(() => {}), warn: mock(() => {}), error: mock(() => {}) },
}));

// `Route.handler`'s catch calls `sendAlarm(...).catch(...)`. Without a stub of
// its own, this file inherits whichever shape an earlier file last left on the
// module — and a non-thenable makes the handler throw before it writes the 500.
mock.module("../../../alarm", () => ({
  sendAlarm: mock(async () => {}),
}));

// Password verification is stubbed to keep tests fast + deterministic.
const mockVerifyPassword = spyOn(Bun.password, "verify").mockImplementation(async () => false);

afterAll(() => {
  mockVerifyPassword.mockRestore();
  mockOthersCanRead.mockRestore();
  mockSendSystemMail.mockRestore();
  restoreLeaves();
  const realServer = (globalThis as Record<string, unknown>).__REAL_SERVER;
  if (realServer) mock.module("server", () => realServer);
  const realAlarm = (globalThis as Record<string, unknown>).__REAL_ALARM;
  if (realAlarm) mock.module("../../../alarm", () => realAlarm);
});

// ── Helpers ───────────────────────────────────────────────────────────────────

const makeUser = (username = "alice", id = "u1") => ({
  id,
  username,
  password: "$2b$10$hashedpassword",
  getSigned: () => ({ id, username }),
});

const makeReq = (overrides: Record<string, unknown> = {}) => {
  // Regenerating swaps in a fresh, empty session, so an identity attached
  // before the call is dropped rather than carried onto the new id.
  // Modelling that clear is what makes an assign-then-reissue route fail the
  // ordering assertions below instead of passing them.
  const session: Record<string, unknown> = {
    user: null,
    save: mock(async () => {}),
    destroy: mock(async () => {}),
  };
  session.regenerate = mock(async () => {
    session.user = null;
  });
  return {
    method: "POST",
    session,
    body: {},
    params: {},
    query: {},
    headers: {},
    ip: "127.0.0.1",
    ...overrides,
  } as unknown as HttpRequest;
};

/** Reads a handler's response back as its status and JSON body. */
const readResponse = async (response: Response) => ({
  _code: response.status,
  _body: (await response.json()) as unknown,
});

// ── post-login tests ──────────────────────────────────────────────────────────

describe("postLoginRoute", () => {
  beforeEach(() => {
    mockGetUser.mockClear();
    mockVerifyPassword.mockClear();
  });

  it("returns success and session user on valid credentials", async () => {
    const { postLoginRoute } = await import("./post-login");

    const user = makeUser("alice");
    mockGetUser.mockResolvedValueOnce(user);
    mockVerifyPassword.mockResolvedValueOnce(true);

    const req = makeReq({ body: { username: "alice", password: "secret" } });

    const result = await postLoginRoute.callback(req);

    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect((result as ApiResponse<unknown>).body).toMatchObject({ id: "u1", username: "alice" });
    expect((req as unknown as { session: { user: unknown; destroy: ReturnType<typeof mock> } }).session.user).toMatchObject({ id: "u1", username: "alice" });
  });

  it("rejects and never answers success when the session store write fails", async () => {
    const { postLoginRoute } = await import("./post-login");

    const user = makeUser("alice");
    mockGetUser.mockResolvedValueOnce(user);
    mockVerifyPassword.mockResolvedValueOnce(true);

    const req = makeReq({ body: { username: "alice", password: "secret" } });
    (req as unknown as { session: { save: ReturnType<typeof mock> } }).session.save = mock(
      async () => {
        throw new Error("session store error");
      }
    );

    await expect(postLoginRoute.callback(req)).rejects.toThrow(
      "session store error"
    );
  });

  it("answers 500 without the driver's message when the session store write fails", async () => {
    const { postLoginRoute } = await import("./post-login");

    const user = makeUser("alice");
    mockGetUser.mockResolvedValueOnce(user);
    mockVerifyPassword.mockResolvedValueOnce(true);

    const req = makeReq({ body: { username: "alice", password: "secret" } });
    (req as unknown as { session: { save: ReturnType<typeof mock> } }).session.save = mock(
      async () => {
        throw new Error('relation "sessions" does not exist');
      }
    );

    const res = await readResponse(await postLoginRoute.handler(req));

    expect(res._code).toBe(500);
    expect(res._body).toEqual({ status: "error", message: "Internal server error" });
    expect(JSON.stringify(res._body)).not.toContain("sessions");
  });

  it("waits for the session write before answering success", async () => {
    const { postLoginRoute } = await import("./post-login");

    const user = makeUser("alice");
    mockGetUser.mockResolvedValueOnce(user);
    mockVerifyPassword.mockResolvedValueOnce(true);

    const req = makeReq({ body: { username: "alice", password: "secret" } });
    let settleSave: (() => void) | undefined;
    let onSaveCalled: (() => void) | undefined;
    const saveCalled = new Promise<void>((resolve) => {
      onSaveCalled = resolve;
    });
    (req as unknown as { session: { save: ReturnType<typeof mock> } }).session.save = mock(
      () =>
        new Promise<void>((resolve) => {
          settleSave = resolve;
          onSaveCalled?.();
        })
    );

    let answered = false;
    const pending = postLoginRoute.callback(req).then((result) => {
      answered = true;
      return result;
    });

    await saveCalled;
    await Promise.resolve();
    expect(answered).toBe(false);

    settleSave?.();
    expect(((await pending) as ApiResponse<unknown>).status).toBe("success");
  });

  it("returns failed when password doesn't match", async () => {
    const { postLoginRoute } = await import("./post-login");

    const user = makeUser("alice");
    mockGetUser.mockResolvedValueOnce(user);
    mockVerifyPassword.mockResolvedValueOnce(false);

    const req = makeReq({ body: { username: "alice", password: "wrong" } });

    const result = await postLoginRoute.callback(req);

    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toContain("Invalid credentials");
  });

  it("refuses a sign-up that has no password yet as it refuses a wrong password", async () => {
    // The lookup finds the row without a password; the comparison still runs
    // (against the stand-in digest), and the failure counts.
    const { postLoginRoute } = await import("./post-login");

    mockGetUser.mockResolvedValueOnce({
      id: "u-pending",
      username: "user_abc12345",
      email: "pending@elsewhere.example",
      password: undefined,
      getSigned: () => undefined,
    });
    mockVerifyPassword.mockClear();
    const recordFailureSpy = spyOn(rateLimit.loginLimiter, "recordFailure");

    const req = makeReq({ body: { username: "user_abc12345", password: "anything" } });
    const result = await postLoginRoute.callback(req);

    expect(result).toEqual({ status: "failed", message: "Invalid credentials." });
    expect(mockVerifyPassword).toHaveBeenCalledTimes(1);
    expect(recordFailureSpy).toHaveBeenCalledWith("127.0.0.1", "user_abc12345");
    expect((req as unknown as { session: { user: unknown } }).session.user).toBeNull();
    recordFailureSpy.mockRestore();
  });

  it("returns failed when user doesn't exist (runs dummy hash to prevent timing attacks)", async () => {
    const { postLoginRoute } = await import("./post-login");

    mockGetUser.mockResolvedValueOnce(null);
    // dummy compare is called but always returns false
    mockVerifyPassword.mockResolvedValueOnce(false);

    const req = makeReq({ body: { username: "nobody", password: "anypassword" } });

    const result = await postLoginRoute.callback(req);

    expect((result as ApiResponse<unknown>).status).toBe("failed");
    // bcrypt.compare should still have been called (dummy hash)
    expect(mockVerifyPassword).toHaveBeenCalled();
  });

  it("returns failed when body is missing", async () => {
    const { postLoginRoute } = await import("./post-login");

    const req = makeReq({ body: null });

    const result = await postLoginRoute.callback(req);

    expect((result as ApiResponse<unknown>).status).toBe("failed");
  });

  it("returns failed when body is an array", async () => {
    const { postLoginRoute } = await import("./post-login");

    const req = makeReq({ body: [] });

    const result = await postLoginRoute.callback(req);

    expect((result as ApiResponse<unknown>).status).toBe("failed");
  });

  it("returns failed when password is missing", async () => {
    const { postLoginRoute } = await import("./post-login");

    const req = makeReq({ body: { username: "alice" } });

    const result = await postLoginRoute.callback(req);

    expect((result as ApiResponse<unknown>).status).toBe("failed");
  });

  it("returns failed when email field is not a string", async () => {
    const { postLoginRoute } = await import("./post-login");

    const req = makeReq({ body: { email: 123, password: "secret" } });

    const result = await postLoginRoute.callback(req);

    expect((result as ApiResponse<unknown>).status).toBe("failed");
  });

  it("returns failed when username field is not a string", async () => {
    const { postLoginRoute } = await import("./post-login");

    const req = makeReq({ body: { username: {}, password: "secret" } });

    const result = await postLoginRoute.callback(req);

    expect((result as ApiResponse<unknown>).status).toBe("failed");
  });

  it("accepts login with email field instead of username", async () => {
    const { postLoginRoute } = await import("./post-login");

    const user = makeUser("alice");
    mockGetUser.mockResolvedValueOnce(user);
    mockVerifyPassword.mockResolvedValueOnce(true);

    const req = makeReq({ body: { email: "alice@example.com", password: "correct" } });

    const result = await postLoginRoute.callback(req);

    expect((result as ApiResponse<unknown>).status).toBe("success");
  });
});

// ── delete-login tests ────────────────────────────────────────────────────────

describe("deleteLoginRoute", () => {
  it("destroys session and returns success", async () => {
    const { deleteLoginRoute } = await import("./delete-login");

    const req = makeReq({ method: "DELETE" });

    const result = await deleteLoginRoute.callback(req);

    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect((req as unknown as { session: { user: unknown; destroy: ReturnType<typeof mock> } }).session.destroy).toHaveBeenCalled();
  });

  it("rejects and never answers success when session.destroy calls back with an error", async () => {
    const { deleteLoginRoute } = await import("./delete-login");

    const req = makeReq({ method: "DELETE" });
    (req as unknown as { session: { user: unknown; destroy: ReturnType<typeof mock> } }).session.destroy = mock(async () => {
      throw new Error("session store error");
    });

    await expect(deleteLoginRoute.callback(req)).rejects.toThrow(
      "session store error"
    );
  });

  it("waits for the destroy callback before answering", async () => {
    const { deleteLoginRoute } = await import("./delete-login");

    const req = makeReq({ method: "DELETE" });
    let settleDestroy: (() => void) | undefined;
    (req as unknown as { session: { destroy: ReturnType<typeof mock> } }).session.destroy = mock(
      () =>
        new Promise<void>((resolve) => {
          settleDestroy = resolve;
        })
    );

    let answered = false;
    const pending = deleteLoginRoute.callback(req).then((result) => {
      answered = true;
      return result;
    });
    await Promise.resolve();
    expect(answered).toBe(false);

    settleDestroy?.();
    const result = await pending;
    expect((result as ApiResponse<unknown>).status).toBe("success");
  });
});

// ── get-login tests ───────────────────────────────────────────────────────────

describe("getLoginRoute", () => {
  it("returns user and app version when session has user", async () => {
    const { getLoginRoute } = await import("./get-login");
    const req = makeReq({ session: { user: { id: "u1", username: "alice" } } });
    const result = await getLoginRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect((result as ApiResponse<unknown>).body.user).toMatchObject({ id: "u1", username: "alice" });
    expect((result as ApiResponse<unknown>).body.app.version).toBe(TEST_VERSION);
  });

  it("returns null user and app version when not logged in", async () => {
    const { getLoginRoute } = await import("./get-login");
    const req = makeReq({ session: {} });
    const result = await getLoginRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect((result as ApiResponse<unknown>).body.user).toBeUndefined();
    expect((result as ApiResponse<unknown>).body.app.version).toBe(TEST_VERSION);
    expect((result as ApiResponse<unknown>).message).toMatch(/Not logged in/i);
  });
});

// ── post-set-info tests ───────────────────────────────────────────────────────

describe("postSetInfoRoute", () => {
  beforeEach(() => mockSetUserInfo.mockClear());

  it("returns failed when body is missing", async () => {
    const { postSetInfoRoute } = await import("./post-set-info");
    const req = makeReq({ body: null });
    const result = await postSetInfoRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
  });

  it("returns failed when email is missing", async () => {
    const { postSetInfoRoute } = await import("./post-set-info");
    const req = makeReq({ body: { username: "alice", password: "pass" } });
    const result = await postSetInfoRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toMatch(/email is required/i);
  });

  it("returns failed when username is missing", async () => {
    const { postSetInfoRoute } = await import("./post-set-info");
    const req = makeReq({ body: { email: "a@b.com", password: "pass" } });
    const result = await postSetInfoRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toMatch(/username is required/i);
  });

  it("returns failed when password is missing", async () => {
    const { postSetInfoRoute } = await import("./post-set-info");
    const req = makeReq({ body: { email: "a@b.com", username: "alice" } });
    const result = await postSetInfoRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toMatch(/password is required/i);
  });

  it("returns failed when token is not a string", async () => {
    const { postSetInfoRoute } = await import("./post-set-info");
    const req = makeReq({ body: { email: "a@b.com", username: "alice", password: "pass", token: 123 } });
    const result = await postSetInfoRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toMatch(/token must be a string/i);
  });

  it("sets session user and returns success with valid data", async () => {
    const { postSetInfoRoute } = await import("./post-set-info");
    const maskedUser = { id: "u1", username: "alice", email: "a@b.com" };
    // First getUser call is the read-only pre-gate (no existing row).
    mockGetUser.mockResolvedValueOnce(null);
    mockSetUserInfo.mockResolvedValueOnce(maskedUser as Awaited<ReturnType<typeof mockSetUserInfo>>);
    const req = makeReq({ body: { email: "a@b.com", username: "alice", password: "pass" } });
    const result = await postSetInfoRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect((result as ApiResponse<unknown>).body).toEqual(maskedUser);
    expect((req as unknown as { session: { user: unknown; destroy: ReturnType<typeof mock> } }).session.user).toEqual(maskedUser);
  });

  it("refuses an address someone else can read BEFORE setUserInfo (no DB mutation)", async () => {
    // A reset token mailed there is readable by the owner or a viewer of the
    // domain it is delivered into, who could set the account's password with
    // it. setUserInfo unconditionally re-hashes the password on the existing
    // row, so the gate has to come before it.
    const { postSetInfoRoute } = await import("./post-set-info");
    mockSetUserInfo.mockClear();
    mockGetUser.mockResolvedValueOnce({
      id: "admin-id",
      username: "admin",
      email: "admin@example.com",
    });
    mockOthersCanRead.mockResolvedValueOnce(true);
    const req = makeReq({
      body: {
        email: "admin@example.com",
        username: "admin",
        password: "attacker-chosen",
        token: "stolen-token",
      },
    });
    const result = await postSetInfoRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toContain("Invalid credentials");
    expect(mockOthersCanRead).toHaveBeenCalledWith("admin@example.com", "admin-id");
    // Load-bearing: proves the gate fires BEFORE the DB-mutating call.
    expect(mockSetUserInfo).not.toHaveBeenCalled();
  });

  it("lets an ordinary address through to setUserInfo", async () => {
    // Mutation-test the gate: a check that refused every username would pass
    // both refusal cases above while breaking every real signup.
    const { postSetInfoRoute } = await import("./post-set-info");
    mockSetUserInfo.mockClear();
    const maskedUser = { id: "u1", username: "alice", email: "a@b.com" };
    mockGetUser.mockResolvedValueOnce({
      id: "u1",
      username: "alice",
      email: "a@b.com",
    });
    mockSetUserInfo.mockResolvedValueOnce(
      maskedUser as Awaited<ReturnType<typeof mockSetUserInfo>>
    );
    const req = makeReq({
      body: { email: "a@b.com", username: "alice", password: "pass" },
    });
    const result = await postSetInfoRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect(mockSetUserInfo).toHaveBeenCalledTimes(1);
  });

  it("regenerates the session id before it attaches the rotated user", async () => {
    // setUserInfo deletes the user's session rows; without a regenerate the
    // assignment re-upserts the caller's own row under the id it already had,
    // so the session most likely to be the compromised one survives.
    const { postSetInfoRoute } = await import("./post-set-info");
    mockSetUserInfo.mockClear();
    mockGetUser.mockResolvedValueOnce({ id: "u1", username: "alice", email: "a@b.com" });
    mockSetUserInfo.mockResolvedValueOnce(
      { id: "u1", username: "alice", email: "a@b.com" } as Awaited<
        ReturnType<typeof mockSetUserInfo>
      >
    );
    const req = makeReq({
      body: { email: "a@b.com", username: "alice", password: "pass" },
    });
    const result = await postSetInfoRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect(req.session.regenerate).toHaveBeenCalledTimes(1);
    expect(req.session.user).toEqual({ id: "u1", username: "alice", email: "a@b.com" });
  });

  it("fails without attaching the user when the regenerate errors", async () => {
    // A session that could not be reissued must not be handed the rotated
    // identity on the id it was already carrying.
    const { postSetInfoRoute } = await import("./post-set-info");
    mockSetUserInfo.mockClear();
    mockGetUser.mockResolvedValueOnce({ id: "u1", username: "alice", email: "a@b.com" });
    mockSetUserInfo.mockResolvedValueOnce(
      { id: "u1", username: "alice", email: "a@b.com" } as Awaited<
        ReturnType<typeof mockSetUserInfo>
      >
    );
    const req = makeReq({
      body: { email: "a@b.com", username: "alice", password: "pass" },
      session: {
        user: null,
        regenerate: mock(async () => {
          throw new Error("store unavailable");
        }),
        destroy: mock(async () => {}),
      },
    });
    await expect(
      postSetInfoRoute.callback(req)
    ).rejects.toThrow("store unavailable");
    expect(req.session.user).toBeNull();
  });

  it("neither reissues nor attaches when the emailed token does not match", async () => {
    const { postSetInfoRoute } = await import("./post-set-info");
    mockSetUserInfo.mockClear();
    mockGetUser.mockResolvedValueOnce({ id: "u1", username: "alice", email: "a@b.com" });
    mockSetUserInfo.mockRejectedValueOnce(
      new Error("`setUserInfo` failed because token doesn't match.")
    );
    const req = makeReq({
      body: { email: "a@b.com", username: "alice", password: "pass", token: "wrong" },
    });
    await expect(
      postSetInfoRoute.callback(req)
    ).rejects.toThrow("token doesn't match");
    expect(req.session.regenerate).not.toHaveBeenCalled();
    expect(req.session.user).toBeNull();
  });
});

// ── post-token tests ──────────────────────────────────────────────────────────

describe("postTokenRoute", () => {
  beforeEach(() => {
    mockIsValidEmail.mockClear();
    mockCreateToken.mockClear();
    mockGetUser.mockClear();
    mockSendSystemMail.mockClear();
    mockStartTimer.mockClear();
    mockOthersCanRead.mockClear();
  });

  it("returns failed when email is invalid", async () => {
    const { postTokenRoute } = await import("./post-token");
    mockIsValidEmail.mockReturnValueOnce(false);
    const req = makeReq({ body: { email: "notanemail" } });
    const result = await postTokenRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect((result as ApiResponse<unknown>).message).toMatch(/invalid/i);
  });

  it("sends auth email and returns success for valid email", async () => {
    const { postTokenRoute } = await import("./post-token");
    // A brand-new sign-up: no existing row.
    mockGetUser.mockResolvedValueOnce(null);
    const req = makeReq({ body: { email: "user@example.com" } });
    const result = await postTokenRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect(mockSendSystemMail).toHaveBeenCalledTimes(1);
    expect(mockStartTimer).toHaveBeenCalledWith("u1");
  });

  it("sends the link from the server itself, from no account and with no Sent copy", async () => {
    // The body carries a live token. No account sends it, so no mailbox — and
    // nobody with a role on one — ever holds a copy.
    const { postTokenRoute } = await import("./post-token");
    mockGetUser.mockResolvedValueOnce(null);
    mockSendMail.mockClear();
    const req = makeReq({ body: { email: "user@example.com" } });
    await postTokenRoute.callback(req);
    expect(mockSendSystemMail).toHaveBeenCalledTimes(1);
    expect(mockSendMail).not.toHaveBeenCalled();
  });

  it("refuses an address someone other than its account can read BEFORE createToken", async () => {
    // /token is unauthenticated, and the link it mails comes back into the
    // domain the address is delivered into, where its owner and viewers read
    // it. createToken's existing-user branch writes `{token, expiry}` onto
    // the row, so the gate fires first. Same-shape as a normal send so no
    // probe signal.
    const { postTokenRoute } = await import("./post-token");
    mockCreateToken.mockClear();
    mockGetUser.mockResolvedValueOnce({
      id: "admin-id",
      username: "admin",
      email: "admin@example.com",
    });
    mockOthersCanRead.mockResolvedValueOnce(true);
    const recordFailureSpy = spyOn(rateLimit.tokenLimiter, "recordFailure");
    const req = makeReq({ body: { email: "admin@example.com" } });
    const result = await postTokenRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect(mockOthersCanRead).toHaveBeenCalledWith("admin@example.com", "admin-id");
    // Load-bearing — proves the gate fires before every mutation path.
    expect(mockCreateToken).not.toHaveBeenCalled();
    expect(mockStartTimer).not.toHaveBeenCalled();
    expect(mockSendSystemMail).not.toHaveBeenCalled();
    expect(recordFailureSpy).toHaveBeenCalledWith("127.0.0.1");
    recordFailureSpy.mockRestore();
  });

  it("asks about a new sign-up's address with no account, so any reader refuses it", async () => {
    const { postTokenRoute } = await import("./post-token");
    mockCreateToken.mockClear();
    mockGetUser.mockResolvedValueOnce(null);
    mockOthersCanRead.mockResolvedValueOnce(true);
    const req = makeReq({ body: { email: "victim@served-domain.test" } });
    const result = await postTokenRoute.callback(req);
    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect(mockOthersCanRead).toHaveBeenCalledWith("victim@served-domain.test", undefined);
    expect(mockCreateToken).not.toHaveBeenCalled();
    expect(mockSendSystemMail).not.toHaveBeenCalled();
  });
});

describe("postTokenRoute body shape", () => {
  beforeEach(() => {
    mockIsValidEmail.mockReset();
    // The suite-wide stub answers `true` for every input, which cannot
    // distinguish a guarded route from an unguarded one. The real
    // `isValidEmail` opens with `email.split("@")`, so mirror that first
    // statement: a non-string argument must blow up here, exactly as it does
    // in production, or these cases pass against a route that never guards.
    mockIsValidEmail.mockImplementation(
      ((email: unknown) => (email as string).split("@").length === 2) as never
    );
    mockCreateToken.mockClear();
    mockGetUser.mockClear();
    mockSendSystemMail.mockClear();
    mockStartTimer.mockClear();
  });

  it("rejects a non-string email instead of throwing on email.split", async () => {
    const { postTokenRoute } = await import("./post-token");

    for (const email of [undefined, null, 123, {}, ["a@b.com"], true]) {
      const req = makeReq({ body: { email } });
      const result = await postTokenRoute.callback(req);

      expect((result as ApiResponse<unknown>).status).toBe("failed");
      expect((result as ApiResponse<unknown>).message).toMatch(/invalid/i);
    }

    expect(mockCreateToken).not.toHaveBeenCalled();
    expect(mockSendSystemMail).not.toHaveBeenCalled();
    expect(mockStartTimer).not.toHaveBeenCalled();
  });

  it("rejects a body that is not a plain object", async () => {
    const { postTokenRoute } = await import("./post-token");

    for (const body of [undefined, null, "user@example.com", 7, [{ email: "a@b.com" }]]) {
      const req = makeReq({ body });
      const result = await postTokenRoute.callback(req);

      expect((result as ApiResponse<unknown>).status).toBe("failed");
      expect((result as ApiResponse<unknown>).message).toMatch(/invalid/i);
    }

    expect(mockCreateToken).not.toHaveBeenCalled();
    expect(mockSendSystemMail).not.toHaveBeenCalled();
  });

  it("rejects a bodyless request, which parses as {}", async () => {
    const { postTokenRoute } = await import("./post-token");

    const result = await postTokenRoute.callback(makeReq({ body: {} }));

    expect((result as ApiResponse<unknown>).status).toBe("failed");
    expect(mockSendSystemMail).not.toHaveBeenCalled();
  });

  it("still sends the magic link for a well-formed email", async () => {
    const { postTokenRoute } = await import("./post-token");
    // A brand-new signup has no existing row.
    mockGetUser.mockResolvedValueOnce(null);

    const req = makeReq({ body: { email: "user@example.com" } });
    const result = await postTokenRoute.callback(req);

    expect((result as ApiResponse<unknown>).status).toBe("success");
    expect(mockSendSystemMail).toHaveBeenCalledTimes(1);
  });

  // Both rejection branches must consume quota. A bad address already did, and
  // a caller who can be refused without paying for it is the cheaper attack.
  it.each([
    ["a non-object body", "198.51.100.30", "notanobject"],
    ["a non-string email", "198.51.100.31", { email: 123 }]
  ])("counts %s against the IP quota", async (_label, ip, body) => {
    const { postTokenRoute } = await import("./post-token");
    const rateLimit = await import("../../rate-limit");
    rateLimit.tokenLimiter.reset(ip as string);

    const req = makeReq({ body, ip });

    for (let i = 0; i < 3; i++) {
      const result = await postTokenRoute.callback(req);
      expect((result as ApiResponse<unknown>).status).toBe("failed");
    }

    expect(rateLimit.tokenLimiter.check(req)?.status).toBe(429);
  });
});

describe("postLoginRoute + loginLimiter integration (#504)", () => {
  beforeEach(() => {
    mockGetUser.mockClear();
    mockVerifyPassword.mockClear();
  });

  it("records a failure on bad credentials but not on success", async () => {
    const { postLoginRoute } = await import("./post-login");
    const rateLimit = await import("../../rate-limit");
    // Fresh limiter for this IP — reset first.
    rateLimit.loginLimiter.reset("198.51.100.10");

    const req = (body: Record<string, unknown>) =>
      makeReq({ body, ip: "198.51.100.10" });

    // Five consecutive bad-password attempts should all reach the handler
    // (the check lets them through; recordFailure bumps the counter).
    mockGetUser.mockResolvedValue(makeUser("alice"));
    mockVerifyPassword.mockResolvedValue(false);
    for (let i = 0; i < 5; i++) {
      const r = await postLoginRoute.callback(req({ username: "alice", password: "wrong" }));
      expect((r as ApiResponse<unknown>).status).toBe("failed");
    }

    // Now the check should block.
    expect(rateLimit.loginLimiter.check(req({ username: "alice", password: "wrong" }))?.status).toBe(
      429
    );

    // A successful login (different IP, fresh counter) resets the counter and
    // does NOT consume a slot.
    rateLimit.loginLimiter.reset("198.51.100.11");
    const successReq = makeReq({
      body: { username: "alice", password: "right" },
      ip: "198.51.100.11"
    });
    mockGetUser.mockResolvedValueOnce(makeUser("alice"));
    mockVerifyPassword.mockResolvedValueOnce(true);
    const ok = await postLoginRoute.callback(successReq);
    expect((ok as ApiResponse<unknown>).status).toBe("success");

    // Now run 10 more successful logins from the same IP — none should
    // burn a slot (the bug was that successes counted).
    for (let i = 0; i < 10; i++) {
      mockGetUser.mockResolvedValueOnce(makeUser("alice"));
      mockVerifyPassword.mockResolvedValueOnce(true);
      const r = await postLoginRoute.callback(successReq);
      expect((r as ApiResponse<unknown>).status).toBe("success");
    }

    // The 11th success-path check still passes.
    expect(rateLimit.loginLimiter.check(successReq)).toBeUndefined();
  });
});

describe("postTokenRoute + tokenLimiter integration (#504)", () => {
  beforeEach(() => {
    mockIsValidEmail.mockReset();
    mockIsValidEmail.mockReturnValue(true);
    mockCreateToken.mockClear();
    mockGetUser.mockClear();
    mockSendSystemMail.mockClear();
    mockStartTimer.mockClear();
  });

  it("does not count thrown server errors against the IP quota", async () => {
    const { postTokenRoute } = await import("./post-token");
    const rateLimit = await import("../../rate-limit");
    rateLimit.tokenLimiter.reset("198.51.100.20");

    const req = makeReq({
      body: { email: "user@example.com" },
      ip: "198.51.100.20"
    });

    // sendSystemMail rejects → callback throws → recordFailure should NOT run.
    mockGetUser.mockImplementation((async () => null) as never);
    mockSendSystemMail.mockRejectedValue(new Error("smtp transient"));

    for (let i = 0; i < 5; i++) {
      await expect(
        postTokenRoute.callback(req)
      ).rejects.toThrow();
    }

    // The check should still let requests through — server errors did not
    // burn quota.
    expect(rateLimit.tokenLimiter.check(req)).toBeUndefined();
  });
});
