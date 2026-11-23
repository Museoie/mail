/**
 * IMAP credential-revocation fence: `verifyCredentialVersion`
 * de-authenticates a connection whose credential rotated after it
 * authenticated, and lets a current one through untouched.
 *
 * These tests drive the real `ImapSession` with the `server` barrel mocked
 * at the `getUser` seam — the fence's only external read — following the
 * `readonly-user.test.ts` pattern of priming private fields directly rather
 * than driving bcrypt/LOGIN end-to-end.
 */
import { describe, it, expect, mock, spyOn, beforeEach, afterAll } from "bun:test";
import { restoreLeaves } from "test-helpers";
import * as domainRepo from "../postgres/repositories/domains";

const realServer = (globalThis as Record<string, unknown>)
  .__REAL_SERVER as Record<string, unknown>;

const mockGetUser = mock(
  (_input: unknown): Promise<unknown> => Promise.resolve(null)
);
const mockLogger = {
  debug: mock(() => {}),
  info: mock(() => {}),
  warn: mock(() => {}),
  error: mock(() => {}),
};

/**
 * Spread the real barrel rather than listing exports: `mock.module`
 * replaces the export graph-wide, so a hand-picked object deletes every
 * other `server` export for the rest of this file.
 */
mock.module("server", () => ({
  ...realServer,
  getUser: mockGetUser,
  logger: mockLogger,
}));

// Spied after the barrel mock, and the session loaded after both, so the
// session reads the spied export rather than one the mock re-evaluated.
const mockGetDomainAccess = spyOn(domainRepo, "getDomainAccess").mockResolvedValue(null);
const { ImapSession } = await import("./session");
type ImapSession = InstanceType<typeof ImapSession>;

afterAll(() => {
  mockGetDomainAccess.mockRestore();
  mock.module("server", () => realServer);
  restoreLeaves();
});

beforeEach(() => {
  mockGetUser.mockReset();
  mockGetUser.mockImplementation(() => Promise.resolve(null));
  mockLogger.info.mockClear();
});

const mkSocket = () => {
  const writes: string[] = [];
  const socket = {
    write: (data: string) => {
      writes.push(data);
      return true;
    },
    destroyed: false,
    writable: true,
  };
  return { socket, writes };
};

const mkHandler = () => ({ setPendingSaslTag: () => {} });

type PrimedSession = ImapSession & {
  authenticated: boolean;
  authenticatedAs: string | null;
  credentialPasswordVersion: number | null;
};

const primeAuthenticated = (
  stampedVersion: number,
  authenticatedAs = "alice"
): { session: PrimedSession; writes: string[] } => {
  const { socket, writes } = mkSocket();
  const handler = mkHandler();
  const session = new ImapSession(handler as never, socket as never);
  const s = session as unknown as PrimedSession;
  s.authenticated = true;
  s.authenticatedAs = authenticatedAs;
  s.credentialPasswordVersion = stampedVersion;
  return { session: s, writes };
};

describe("ImapSession.verifyCredentialVersion", () => {
  it("de-authenticates and answers NO [AUTHENTICATIONFAILED] when the credential rotated after auth", async () => {
    // The row is on generation 2; this connection was stamped with 1.
    mockGetUser.mockImplementation(() =>
      Promise.resolve({ passwordVersion: 2 })
    );
    const { session, writes } = primeAuthenticated(1);

    const ok = await session.verifyCredentialVersion("A1");

    expect(ok).toBe(false);
    expect(writes).toEqual([
      "A1 NO [AUTHENTICATIONFAILED] Credential changed, please re-authenticate\r\n",
    ]);
    // The grant is gone: a follow-up command must see an unauthenticated
    // session, not merely a refused one.
    expect(session.isAuthenticated()).toBe(false);
  });

  it("lets the command through when the stamped version is current", async () => {
    mockGetUser.mockImplementation(() =>
      Promise.resolve({ passwordVersion: 2 })
    );
    const { session, writes } = primeAuthenticated(2);

    const ok = await session.verifyCredentialVersion("A2");

    expect(ok).toBe(true);
    expect(writes).toEqual([]);
    expect(session.isAuthenticated()).toBe(true);
  });

  it("treats a missing version as generation 1 on both sides", async () => {
    // Rows that predate the fence's migration read back null.
    mockGetUser.mockImplementation(() => Promise.resolve({}));
    const { session } = primeAuthenticated(1);

    expect(await session.verifyCredentialVersion("A3")).toBe(true);
    expect(session.isAuthenticated()).toBe(true);
  });

  it("de-authenticates when the account is gone, as on a rotation", async () => {
    // Deleted, or soft-deleted (which the lookup leaves out), after auth. A
    // missing row used to read as generation 1 and let the session go on.
    mockGetUser.mockImplementation(() => Promise.resolve(undefined));
    const { session, writes } = primeAuthenticated(1);

    const ok = await session.verifyCredentialVersion("A6");

    expect(ok).toBe(false);
    expect(writes).toEqual([
      "A6 NO [AUTHENTICATIONFAILED] Credential changed, please re-authenticate\r\n",
    ]);
    expect(session.isAuthenticated()).toBe(false);
  });

  it("does not touch the database for an unauthenticated session", async () => {
    const { socket, writes } = mkSocket();
    const session = new ImapSession(mkHandler() as never, socket as never);

    expect(await session.verifyCredentialVersion("A4")).toBe(true);
    expect(mockGetUser).not.toHaveBeenCalled();
    expect(writes).toEqual([]);
  });

  it("keys the comparison on the person who signed in, not the store they read", async () => {
    // A viewer's session reads the owner's store but authenticated as its
    // own credential row; the comparison follows `authenticatedAs`, matching
    // the eviction registry's keying.
    mockGetUser.mockImplementation((input: unknown) => {
      const { username } = input as { username: string };
      expect(username).toBe("auditor");
      return Promise.resolve({ passwordVersion: 2 });
    });
    const { session } = primeAuthenticated(1, "auditor");

    expect(await session.verifyCredentialVersion("A5")).toBe(false);
    expect(session.isAuthenticated()).toBe(false);
  });
});

describe("ImapSession — the role a scoped login rests on", () => {
  const SCOPE = { userId: "u-auditor", domain: "example.com", role: "viewer", ownerId: "u-owner" };
  const access = (overrides: Partial<domainRepo.DomainAccess> = {}): domainRepo.DomainAccess => ({
    domainId: "d-1",
    name: "example.com",
    ownerId: "u-owner",
    ownerUsername: "owner",
    role: "viewer",
    verifiedAt: null,
    ...overrides,
  });

  const primeScoped = () => {
    mockGetUser.mockImplementation(() => Promise.resolve({ passwordVersion: 1 }));
    const primed = primeAuthenticated(1, "auditor");
    (primed.session as unknown as { scope: unknown }).scope = SCOPE;
    return primed;
  };

  beforeEach(() => mockGetDomainAccess.mockReset());

  it("lets the command through while the role stands", async () => {
    mockGetDomainAccess.mockResolvedValue(access());
    const { session, writes } = primeScoped();

    expect(await session.verifyCredentialVersion("B1")).toBe(true);
    expect(mockGetDomainAccess).toHaveBeenCalledWith("u-auditor", "example.com");
    expect(writes).toEqual([]);
  });

  it("ends the session at its next command once the role is revoked", async () => {
    mockGetDomainAccess.mockResolvedValue(null);
    const { session, writes } = primeScoped();

    expect(await session.verifyCredentialVersion("B2")).toBe(false);
    expect(writes).toEqual([
      "B2 NO [AUTHENTICATIONFAILED] Your role on example.com changed, please re-authenticate\r\n",
    ]);
    expect(session.isAuthenticated()).toBe(false);
  });

  it("ends it when the domain moved to another owner, or the role changed", async () => {
    for (const changed of [access({ ownerId: "u-new-owner" }), access({ role: "owner", ownerId: "u-auditor" })]) {
      mockGetDomainAccess.mockResolvedValue(changed);
      const { session } = primeScoped();
      expect(await session.verifyCredentialVersion("B3")).toBe(false);
    }
  });

  it("reads no role for a plain login", async () => {
    mockGetUser.mockImplementation(() => Promise.resolve({ passwordVersion: 1 }));
    const { session } = primeAuthenticated(1, "alice");

    expect(await session.verifyCredentialVersion("B4")).toBe(true);
    expect(mockGetDomainAccess).not.toHaveBeenCalled();
  });
});
