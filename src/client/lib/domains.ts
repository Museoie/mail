import type { DomainsGetResponse } from "server";
import { call } from "./call";

export const domainsUrl = "/api/domains";

/** The user's domains, viewed and pending ones included; null when the request failed. */
export const fetchDomains = async (): Promise<DomainsGetResponse | null> => {
  const { status, body } = await call.get<DomainsGetResponse>(domainsUrl);
  return status === "success" && body ? body : null;
};

/** The names of the domains in `data` the user owns, which they can send from. */
export const ownedDomainsOf = (data: DomainsGetResponse | null | undefined): string[] =>
  (data?.domains ?? []).filter((domain) => domain.role === "owner").map((domain) => domain.name);

/** The lower-cased domain part of an address, or "" when it has none. */
export const domainOf = (address: string): string =>
  address.toLowerCase().split("@").pop() ?? "";

/** Whether `address` is on one of `domains`. */
export const isOnDomains = (address: string, domains: string[]): boolean => {
  if (!address.includes("@")) return false;
  const host = domainOf(address);
  return domains.some((domain) => domain.toLowerCase() === host);
};

/**
 * The domain a new message goes out from when nothing else picks one, by the
 * server's rule: the served zone when the user owns it, then their own name
 * under it, then their first domain.
 */
export const defaultDomainOf = (
  domains: string[],
  username: string | undefined,
  zone: string
): string => {
  const lower = zone.toLowerCase();
  if (lower && domains.includes(lower)) return lower;
  const own = username && lower ? `${username.toLowerCase()}.${lower}` : "";
  if (own && domains.includes(own)) return own;
  return domains[0] ?? (own || lower);
};
