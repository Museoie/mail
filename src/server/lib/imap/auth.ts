/**
 * IMAP authentication helpers (LOGIN and AUTHENTICATE PLAIN).
 *
 * These are free functions; they take session state as explicit parameters
 * and return { store, authenticated } updates rather than mutating session directly.
 */

import { Socket } from "net";
import { SignedUser } from "common";
import { getUser, logger } from "server";
import {
  isAuthRateLimited,
  normalizeAuthAccount,
  recordAuthFailure,
  resetAuthFailures,
} from "../auth-rate-limit";
import { verifyPassword } from "../verify-password";
import { getSso } from "../sso";
import { checkAppPassword } from "../sso/app-password";
import { Store } from "./store";
import { defaultDomain, ownedDomainNames, parseScopedLogin } from "../domains";
import { getDomainAccess } from "../postgres/repositories/domains";
import { closeSocket } from "./close-socket";

export interface AuthResult {
  store: Store;
  authenticated: true;
  /** True when the login is scoped to a domain the person only views. */
  isReadOnly: boolean;
  /**
   * The person who signed in: the username part of the login, which names
   * the credential row. The store may be another account's (a viewed
   * domain's owner).
   */
  authenticatedAs: string;
  /** The domain a `user@domain` login is scoped to, and the grant it rests on. */
  scope?: SessionScope;
  /**
   * The credential generation the session was minted from.
   * Read from the credential row at auth time — the per-command gate
   * compares this against the row's current version.
   */
  passwordVersion: number;
}

/** Shared failure reply: rate-limit exhaustion closes the socket, anything
 * else is a flat "Invalid credentials" that reveals nothing about which
 * check failed. */
const failAuth = async (
  tag: string,
  ip: string,
  socket: Socket,
  write: (data: string) => boolean | undefined,
  account?: string
): Promise<null> => {
  const limited = await recordAuthFailure(ip, account);
  if (limited) {
    write(`${tag} NO [AUTHENTICATIONFAILED] Too many failed attempts\r\n`);
    closeSocket(socket);
    return null;
  }
  write(`${tag} NO [AUTHENTICATIONFAILED] Invalid credentials.\r\n`);
  return null;
};

/** The domain a scoped login reads, the person's role on it, and its owner. */
export interface SessionScope {
  /** The person's own user id, which the role is keyed by. */
  userId: string;
  domain: string;
  role: "owner" | "viewer";
  ownerId: string;
}

interface ResolvedSessionUser {
  /** The account whose rows the session reads. */
  user: SignedUser;
  isReadOnly: boolean;
  /** The person's username, without any `@domain` scope. */
  username: string;
  scope?: SessionScope;
  /** Undefined reads the whole store; set, only those domains. */
  domains?: string[];
  /** The domains whose addresses are listed as account folders. */
  accountDomains: string[];
  /** The domain account folders are read under. */
  primaryDomain: string;
}

/**
 * The store a person's login opens. A plain login reads their own store, all
 * their domains. A `user@domain` login reads only that domain: from their own
 * store when they own it, read-only from its owner's when they view it, and
 * not at all (null) when they hold no role on it.
 */
const resolveScope = async (
  person: SignedUser,
  username: string,
  scopeDomain: string | undefined
): Promise<ResolvedSessionUser | null> => {
  if (scopeDomain === undefined) {
    const owned = await ownedDomainNames(person.id);
    const primaryDomain = defaultDomain(owned, username) ?? "";
    // Account folders are named by local part and read back on the primary
    // domain, so only that domain's addresses are listed: a folder for an
    // address on another domain would open a different mailbox. A login
    // scoped to that domain lists its folders.
    return {
      user: person,
      isReadOnly: false,
      username,
      accountDomains: primaryDomain ? [primaryDomain] : [],
      primaryDomain,
    };
  }
  const access = await getDomainAccess(person.id, scopeDomain);
  if (!access) return null;
  const scope: SessionScope = {
    userId: person.id,
    domain: access.name,
    role: access.role,
    ownerId: access.ownerId,
  };
  const base = {
    username,
    scope,
    domains: [access.name],
    accountDomains: [access.name],
    primaryDomain: access.name,
  };
  if (access.role === "owner") return { ...base, user: person, isReadOnly: false };
  const owner = (await getUser({ id: access.ownerId }))?.getSigned();
  if (!owner) return null;
  return { ...base, user: owner, isReadOnly: true };
};

/**
 * The shared authentication core both mechanisms funnel through, so the
 * security-relevant policy — the rate-limit gate, the password check, the
 * read-only session resolution, and the failure wording that must not
 * distinguish "no such user" from "wrong password" — is written once.
 * Each entry point owns only its own credential framing (PLAIN base64
 * decoding, LOGIN quote stripping) and its success response.
 *
 * With single sign-on on, the password is checked as the provider's app
 * password; while `AUTH_LOCAL_LOGIN` keeps local passwords valid, the stored
 * one is tried first. A provider that cannot answer gets `NO [UNAVAILABLE]`,
 * which is not counted as a failed attempt.
 *
 * Returns the resolved session user on success. On failure, writes the
 * tagged NO reply (and closes the socket when the rate limiter says so)
 * and returns null. Throws only on unexpected errors, which
 * handleAuthenticate converts to BAD (handleLogin has no SASL framing to
 * fail, so it lets them propagate as before).
 */
const authenticateCredentials = async (
  login: string,
  password: string,
  ip: string,
  socket: Socket,
  write: (data: string) => boolean | undefined,
  tag: string
): Promise<(ResolvedSessionUser & { passwordVersion: number }) | null> => {
  const { username, domain: scopeDomain } = parseScopedLogin(login);
  // The attempted identity: failures accumulate per (ip, account) and the
  // success below forgives only this account's own bucket. The per-IP
  // aggregate is never reset here, so alternating accounts from one IP
  // cannot wipe the counter.
  const account = normalizeAuthAccount(username);

  // A scope the person holds no role on fails like a wrong password, so the
  // answer says nothing about which half of the login was wrong.
  const granted = async (person: SignedUser, passwordVersion: number) => {
    const resolved = await resolveScope(person, username, scopeDomain);
    if (!resolved) return failAuth(tag, ip, socket, write, account);
    // Scoped reset: forgive only the authenticating account's own bucket.
    // The per-IP aggregate keeps its count, so a success here cannot wipe
    // another account's failures from the same IP.
    resetAuthFailures(ip, account);
    return { ...resolved, passwordVersion };
  };
  if (isAuthRateLimited(ip, account)) {
    write(`${tag} NO [AUTHENTICATIONFAILED] Too many failed attempts\r\n`);
    closeSocket(socket);
    return null;
  }

  const sso = getSso();
  // While single sign-on and local passwords are both on (a migration), the
  // local password goes first: it is never sent to the provider, and the
  // clients still using one don't spend the provider's per-client and
  // per-address limits on failed introspections, which would then refuse
  // the app passwords of everyone behind the same address.
  if (!sso || sso.config.localLogin) {
    const local = await checkLocalPassword(username, password);
    if (local) return granted(local.user, local.passwordVersion);
    if (!sso) return failAuth(tag, ip, socket, write, account);
  }

  const appPassword = await checkAppPassword(sso, username, password, "imap", ip);
  if (appPassword.status === "unavailable") {
    write(`${tag} NO [UNAVAILABLE] Authentication is temporarily unavailable\r\n`);
    return null;
  }
  if (appPassword.status === "rejected") return failAuth(tag, ip, socket, write, account);
  return granted(appPassword.user, appPassword.passwordVersion);
};

/** The account's own stored password; null when it doesn't match. */
const checkLocalPassword = async (
  username: string,
  password: string
): Promise<{ user: SignedUser; passwordVersion: number } | null> => {
  const user = await getUser({ username, password });
  const signedUser = user?.getSigned();

  const pwMatches = await verifyPassword(password, user?.password);

  if (!user || !signedUser || !pwMatches) return null;

  // Stamp the credential row's generation — the per-command gate compares
  // this against the row's current version. Read here, at auth time, so a
  // LOGIN that straddled a rotation stamps the stale generation and fails
  // its next comparison.
  return { user: signedUser, passwordVersion: user.passwordVersion ?? 1 };
};

/**
 * Handle AUTHENTICATE PLAIN mechanism.
 *
 * Returns AuthResult on success. On failure, writes directly to the socket
 * and returns null.
 */
export async function handleAuthenticate(
  tag: string,
  mechanism: string,
  initialResponse: string | undefined,
  socket: Socket,
  write: (data: string) => boolean | undefined,
  setPendingSaslTag: (tag: string) => void,
  getCapabilities: () => string
): Promise<AuthResult | null> {
  if (mechanism !== "PLAIN") {
    write(`${tag} NO Only PLAIN authentication supported\r\n`);
    return null;
  }

  if (!initialResponse) {
    write(`+ \r\n`);
    setPendingSaslTag(tag);
    return null;
  }

  const ip = socket.remoteAddress ?? "unknown";

  try {
    const decoded = Buffer.from(initialResponse, "base64").toString("utf8");
    const parts = decoded.split("\0");

    if (parts.length !== 3) {
      write(`${tag} BAD Invalid PLAIN response format\r\n`);
      return null;
    }

    const [, username, password] = parts;

    const resolved = await authenticateCredentials(
      username,
      password,
      ip,
      socket,
      write,
      tag
    );
    if (!resolved) return null;

    // Auth-audit line — never behind the per-command threshold gate in
    // handler.ts (a fast bcrypt round on strong hardware would drop the
    // per-command "IMAP command completed" line to DEBUG). Auth events
    // need a durable INFO surface at the same level as CREATE / RENAME /
    // DELETE from mailbox-ops.ts. `authenticatedAs` names the person, and
    // `effectiveUsername` the account whose rows a viewer's session reads.
    logger.info("IMAP AUTHENTICATE success", {
      component: "imap",
      tag,
      authenticatedAs: resolved.username,
      scopeDomain: resolved.scope?.domain,
      effectiveUsername: resolved.user.username,
      isReadOnly: resolved.isReadOnly,
      remote: `${ip}:${socket.remotePort ?? 0}`,
      mechanism: "PLAIN",
    });
    write(
      `${tag} OK [CAPABILITY ${getCapabilities()}] AUTHENTICATE completed\r\n`
    );
    return toAuthResult(resolved);
  } catch (error) {
    logger.error("AUTHENTICATE error", { component: "imap" }, error);
    write(`${tag} BAD AUTHENTICATE failed\r\n`);
    return null;
  }
}

/**
 * Handle LOGIN command.
 *
 * Returns AuthResult on success; writes error responses and returns null otherwise.
 */
export async function handleLogin(
  tag: string,
  args: string[],
  socket: Socket,
  write: (data: string) => boolean | undefined,
  getCapabilities: () => string
): Promise<AuthResult | null> {
  if (args.length < 2) {
    write(`${tag} BAD LOGIN requires username and password\r\n`);
    return null;
  }

  const ip = socket.remoteAddress ?? "unknown";

  const [username, password] = args;
  const cleanUsername = username.replace(/^"(.*)"$/, "$1");
  const cleanPassword = password.replace(/^"(.*)"$/, "$1");

  const resolved = await authenticateCredentials(
    cleanUsername,
    cleanPassword,
    ip,
    socket,
    write,
    tag
  );
  if (!resolved) return null;

  // Auth-audit line — same rationale as the AUTHENTICATE success log
  // above. Threshold gate in handler.ts is scoped to per-command
  // memory/latency triage; auth events need their own INFO surface.
  logger.info("IMAP LOGIN success", {
    component: "imap",
    tag,
    authenticatedAs: resolved.username,
    scopeDomain: resolved.scope?.domain,
    effectiveUsername: resolved.user.username,
    isReadOnly: resolved.isReadOnly,
    remote: `${ip}:${socket.remotePort ?? 0}`,
  });
  write(`${tag} OK [CAPABILITY ${getCapabilities()}] LOGIN completed\r\n`);
  return toAuthResult(resolved);
}

const toAuthResult = (
  resolved: ResolvedSessionUser & { passwordVersion: number }
): AuthResult => ({
  store: new Store(resolved.user, {
    domains: resolved.domains,
    accountDomains: resolved.accountDomains,
    primaryDomain: resolved.primaryDomain,
  }),
  authenticated: true,
  isReadOnly: resolved.isReadOnly,
  authenticatedAs: resolved.username,
  scope: resolved.scope,
  passwordVersion: resolved.passwordVersion,
});
