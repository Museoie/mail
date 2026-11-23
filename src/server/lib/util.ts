/**
 * Utilities shared across server modules.
 * Placed here to avoid circular imports that occur when importing through the barrel.
 */

export const getDomain = () => process.env.EMAIL_DOMAIN || "mydomain";

/**
 * Whether `address` is under the served zone `domain`: `domain` itself, or
 * any subdomain under it, where every account's own name sits. A domain
 * someone added is local too, by its domain row (`resolveDeliveryDomain`).
 */
export const isLocalAddress = (address: string, domain: string) => {
  const parsedAddress = address.split("@");
  const domainInAddress = parsedAddress[parsedAddress.length - 1].toLowerCase();
  const target = domain.toLowerCase();
  return domainInAddress === target || domainInAddress.endsWith(`.${target}`);
};

/**
 * Whether `username` can name a mailbox: the account's own domain is
 * `<username>.<EMAIL_DOMAIN>`, so only a lower-case DNS label routes mail to
 * the account.
 */
export const isRoutableUsername = (username: string) =>
  /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(username);

/**
 * Resolves `undefined` if `promise` has not settled within `ms`.
 *
 * For awaits on the shutdown path whose own transport carries no deadline: a
 * hung one holds the stop open until the container's grace period expires and
 * SIGKILL replaces the clean exit.
 */
export const withTimeout = <T>(promise: Promise<T>, ms: number): Promise<T | undefined> => {
  let timer: ReturnType<typeof setTimeout>;
  return Promise.race([
    promise,
    new Promise<undefined>((resolve) => {
      timer = setTimeout(() => resolve(undefined), ms);
    }),
  ]).finally(() => clearTimeout(timer));
};
