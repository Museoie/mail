/**
 * Domains as resources: which domain an address belongs to, who may read and
 * send for it, and how a person proves they control a domain they add.
 *
 * The coarse "may use mail at all" decision stays with the identity provider
 * when single sign-on is on; everything here is mail's own, keyed by mail's
 * user ids, and works the same with no provider at all.
 */

import { randomBytes } from "crypto";
import { promises as dnsPromises } from "dns";
import { getDomain, isRoutableUsername } from "./util";
import {
  assignDomain,
  findDomain,
  listDomainAccess,
  listFormerDomains,
  readersOfDomain,
  type DomainAccess,
} from "./postgres/repositories/domains";
import { findUserByUsername } from "./postgres/repositories/users";
import { DOMAIN_NAME_MAX_LENGTH } from "./postgres/models";

/** The zone `EMAIL_DOMAIN` names, lower-cased: every account's own name sits under it. */
export const servedZone = (): string => getDomain().toLowerCase();

/** Whether `name` is the served zone or a name under it. */
export const isInServedZone = (name: string): boolean => {
  const zone = servedZone();
  const target = name.toLowerCase();
  return target === zone || target.endsWith(`.${zone}`);
};

/** The account's own name under the served zone, e.g. `bob.example.com`. */
export const accountDomain = (username: string): string =>
  `${username.toLowerCase()}.${servedZone()}`;

/** The lower-cased domain part of an address, or "" when it has none. */
export const domainOfAddress = (address: string): string => {
  const at = address.lastIndexOf("@");
  return at === -1 ? "" : address.slice(at + 1).trim().toLowerCase();
};

const LABEL = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;

/**
 * The canonical spelling of a domain name someone typed, or null when it is
 * not one: lower-case ASCII labels (an internationalized name goes in its
 * `xn--` form), at least two of them, no trailing dot, at most 253 characters.
 */
export const normalizeDomainName = (input: unknown): string | null => {
  if (typeof input !== "string") return null;
  const name = input.trim().toLowerCase().replace(/\.$/, "");
  if (!name || name.length > DOMAIN_NAME_MAX_LENGTH) return null;
  const labels = name.split(".");
  if (labels.length < 2 || !labels.every((label) => LABEL.test(label))) return null;
  // A top-level label is never all digits, which also keeps out IPv4 literals.
  if (/^\d+$/.test(labels[labels.length - 1])) return null;
  return name;
};

/** The DNS label a verification record goes under, ahead of the domain. */
export const VERIFICATION_LABEL = "_mail-verification";

/** The TXT record that proves control of `name` for the claim holding `token`. */
export const verificationRecord = (name: string, token: string) => ({
  type: "TXT" as const,
  host: `${VERIFICATION_LABEL}.${name}`,
  value: `mail-verification=${token}`,
});

export const newVerificationToken = (): string => randomBytes(24).toString("hex");

export type TxtResolver = (host: string) => Promise<string[][]>;

let txtResolver: TxtResolver = (host) => dnsPromises.resolveTxt(host);

/** Replaces the TXT lookup; for tests. Returns the previous resolver. */
export const setTxtResolver = (resolver: TxtResolver): TxtResolver => {
  const previous = txtResolver;
  txtResolver = resolver;
  return previous;
};

// Answers that mean "the name has no such record", as opposed to "DNS could
// not be asked": the first is a failed verification, the second a retry.
const NO_RECORD_CODES = new Set(["ENOTFOUND", "ENODATA", "NXDOMAIN", "ENONAME"]);

export type VerificationCheck = "found" | "missing" | "unavailable";

/**
 * Looks up the verification record for a claim. A TXT record may come back
 * split into several strings, which DNS joins, so each record's chunks are
 * joined before comparing.
 */
export const checkVerificationRecord = async (
  name: string,
  token: string
): Promise<VerificationCheck> => {
  const { host, value } = verificationRecord(name, token);
  let records: string[][];
  try {
    records = await txtResolver(host);
  } catch (error) {
    const code = (error as { code?: unknown } | null)?.code;
    return typeof code === "string" && NO_RECORD_CODES.has(code) ? "missing" : "unavailable";
  }
  return records.some((chunks) => chunks.join("").trim() === value) ? "found" : "missing";
};

/** The domain an address is delivered into, and the owner whose rows hold it. */
export interface DeliveryDomain {
  domain: string;
  ownerId: string;
}

/**
 * The domain this server delivers `address` into, or null when it delivers
 * nowhere for it.
 *
 * A domain row decides. An account's own name under the served zone that has
 * no row yet (an account a previous version created during a rolling deploy)
 * gets its row here, the same one the boot backfill gives it.
 */
export const resolveDeliveryDomain = async (address: string): Promise<DeliveryDomain | null> => {
  const domain = domainOfAddress(address);
  if (!domain) return null;
  const row = await findDomain(domain);
  // A soft-deleted account has no mailbox, and its name stays taken.
  if (row) return row.ownerDeleted ? null : { domain: row.name, ownerId: row.ownerId };

  const zone = servedZone();
  if (!domain.endsWith(`.${zone}`)) return null;
  const label = domain.slice(0, -(zone.length + 1));
  if (!isRoutableUsername(label)) return null;
  const account = await findUserByUsername(label);
  // A sign-up still setting its password counts, as in the boot backfill.
  if (!account || account.is_deleted) return null;
  if (!(await assignDomain(account.user_id, domain))) {
    const raced = await findDomain(domain);
    return raced ? { domain: raced.name, ownerId: raced.ownerId } : null;
  }
  return { domain, ownerId: account.user_id };
};

/**
 * Whether anyone but `accountId` can read mail delivered to `address`: its
 * domain's owner or a viewer of it. With no `accountId` (a sign-up that has no
 * account yet), whether anyone can read it at all.
 *
 * Anyone who can read a domain can read every token mailed to it, so a reset
 * or sign-up link sent to such an address, or an identity provider's say-so
 * that someone controls it, would hand that account to them.
 */
export const othersCanReadAddress = async (
  address: string,
  accountId?: string
): Promise<boolean> => {
  const delivery = await resolveDeliveryDomain(address);
  if (!delivery) return false;
  const readers = await readersOfDomain(delivery.domain);
  return readers.some((reader) => reader !== accountId);
};

/** The names of the domains `userId` owns. */
export const ownedDomainNames = async (userId: string): Promise<string[]> =>
  (await listDomainAccess(userId))
    .filter((access) => access.role === "owner")
    .map((access) => access.name);

/**
 * The domains whose addresses are `userId`'s own: the ones they own, and the
 * ones they used to own, whose mail stayed with them.
 */
export const ownAddressDomains = async (userId: string): Promise<string[]> => {
  const [owned, former] = await Promise.all([ownedDomainNames(userId), listFormerDomains(userId)]);
  return [...new Set([...owned, ...former])];
};

/** Whether an address is on one of `domains`. */
export type AddressPredicate = (address: string) => boolean;

/** The predicate for addresses on `domains`, ignoring case. */
export const addressOnDomains = (domains: readonly string[]): AddressPredicate => {
  const names = new Set(domains.map((domain) => domain.toLowerCase()));
  return (address) => names.has(domainOfAddress(address));
};

/** The domains `userId` holds the viewer role on. */
export const viewedDomains = async (userId: string): Promise<DomainAccess[]> =>
  (await listDomainAccess(userId)).filter((access) => access.role === "viewer");

/**
 * The domain a send goes out from when the sender names none: the served
 * zone itself when the account owns it (the account that receives the bare
 * domain), then the account's own name, then the first domain it owns.
 */
export const defaultDomain = (owned: string[], username: string): string | undefined => {
  const zone = servedZone();
  if (owned.includes(zone)) return zone;
  const own = accountDomain(username);
  if (owned.includes(own)) return own;
  return owned[0];
};

/**
 * The Mailgun sending domain for mail from `domain`: the served zone for any
 * name under it, which the zone's own Mailgun domain covers, and the domain
 * itself otherwise, which the operator has to have added to Mailgun.
 */
export const mailgunDomainFor = (domain: string): string =>
  isInServedZone(domain) ? getDomain() : domain;

/**
 * Splits an IMAP or SMTP login into the person and the domain it is scoped
 * to: `bob` signs bob in to everything bob owns, `bob@example.com` signs bob
 * in to the one domain `example.com`.
 */
export const parseScopedLogin = (login: string): { username: string; domain?: string } => {
  const at = login.lastIndexOf("@");
  if (at === -1) return { username: login };
  return { username: login.slice(0, at), domain: login.slice(at + 1).toLowerCase() };
};
