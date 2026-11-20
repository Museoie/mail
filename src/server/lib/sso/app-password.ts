import { isIP } from "net";
import { SignedUser } from "common";
import { Unavailable, type Introspection } from "../auth-client";
import { logger } from "../logger";
import { findUserByAuthSub } from "../postgres/repositories/users";
import type { Sso } from ".";

export type AppPasswordScope = "imap" | "smtp";

export type AppPasswordCheck =
  | { status: "accepted"; user: SignedUser; passwordVersion: number }
  | { status: "rejected" }
  | { status: "unavailable" };

/**
 * How long the provider client may serve a cached answer: a minute while the
 * provider answers, and up to ten minutes while it can't be reached. An
 * account changed more recently than this may have had the credential
 * revoked by an event another copy of the server received, so its answer is
 * asked for afresh rather than served from a cache that predates the change.
 */
const CACHED_ANSWER_MS = 600_000;

const ipOption = (address: string): { ip?: string } => {
  const ip = address.replace(/^::ffff:/i, "");
  return isIP(ip) ? { ip } : {};
};

/**
 * Checks `password` as an app password from the identity provider, for a
 * protocol that can carry nothing but a username and a password. It is
 * accepted when the provider vouches for it, it belongs to the account
 * linked to its owner, `username` names that account, and its scopes cover
 * the protocol.
 *
 * `unavailable` means the provider could not answer, so nothing was learnt
 * about the password. A failed lookup of the local account rejects.
 */
export const checkAppPassword = async (
  sso: Sso,
  username: string,
  password: string,
  scope: AppPasswordScope,
  remoteAddress: string,
  now = Date.now()
): Promise<AppPasswordCheck> => {
  const introspect = async (): Promise<Introspection | null | "unavailable"> => {
    try {
      return await sso.client.introspect(password, ipOption(remoteAddress));
    } catch (error) {
      if (error instanceof Unavailable) {
        logger.warn("Could not check an app password", { component: scope, reason: error.message });
      } else {
        logger.error("Checking an app password failed", { component: scope }, error);
      }
      return "unavailable";
    }
  };

  let answer = await introspect();
  if (answer === "unavailable") return { status: "unavailable" };
  if (!answer) return { status: "rejected" };
  const account = await findUserByAuthSub(answer.sub);
  if (!account) return { status: "rejected" };
  if (now - Date.parse(account.updated) < CACHED_ANSWER_MS) {
    sso.client.forget({ sub: answer.sub });
    answer = await introspect();
    if (answer === "unavailable") return { status: "unavailable" };
    if (!answer) return { status: "rejected" };
  }
  if (account.username !== username || !answer.scopes.includes(scope)) {
    return { status: "rejected" };
  }

  logger.info("App password accepted", {
    component: scope,
    authenticatedAs: username,
    credential: answer.id,
  });
  const passwordVersion = account.password_version ?? 1;
  return {
    status: "accepted",
    passwordVersion,
    user: new SignedUser({
      id: account.user_id,
      username: account.username,
      email: account.email ?? undefined,
      passwordVersion,
    }),
  };
};
