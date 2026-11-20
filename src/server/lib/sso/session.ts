import type { SsoSessionType } from "common";
import { InvalidGrant, Unavailable, type Login } from "../auth-client";
import type { HttpSession } from "../http/session";
import { logger } from "../logger";
import { getSso } from ".";

/** How long a session goes before its access is re-checked with the provider. */
export const RECHECK_INTERVAL_MS = 10 * 60 * 1000;

/**
 * How long past a due re-check a session keeps serving while the provider
 * cannot be reached: the staleness a provider outage is allowed to cost.
 */
export const MAX_RECHECK_DELAY_MS = 60 * 60 * 1000;

/** The provider fields a session started by `login` keeps. */
export const ssoSessionFields = (
  login: Pick<Login, "claims" | "refreshToken">,
  now = Date.now()
): SsoSessionType => ({
  sid: login.claims.sid,
  refreshToken: login.refreshToken ?? null,
  recheckAt: new Date(now + RECHECK_INTERVAL_MS),
});

const endSession = async (session: HttpSession, reason: string): Promise<void> => {
  logger.info("Ended a session", { component: "sso", reason, userId: session.user?.id });
  await session.destroy();
  session.user = undefined;
  session.auth = undefined;
};

/**
 * Holds the session a request arrived with to the sign-in mode, before any
 * route reads it:
 *
 * - a session from single sign-on ends once the feature is off, since nothing
 *   can re-check it any more, and a password session ends while password
 *   sign-in is off;
 * - a session from single sign-on whose re-check is due refreshes its token
 *   first. A refused token ends the session. An unreachable provider leaves it
 *   serving, to try again on the next request, until the re-check is
 *   {@link MAX_RECHECK_DELAY_MS} late.
 *
 * Costs nothing on a request whose re-check is not due.
 */
export const enforceSsoSession = async (session: HttpSession, now = Date.now()): Promise<void> => {
  if (!session.user) return;
  const sso = getSso();
  const { auth } = session;
  if (!auth) {
    if (sso && !sso.config.localLogin) await endSession(session, "password sign-in is off");
    return;
  }
  if (!sso) return endSession(session, "single sign-on is off");
  if (auth.recheckAt.getTime() > now) return;
  if (!auth.refreshToken) return endSession(session, "no refresh token to re-check access with");

  try {
    const refreshed = await sso.client.refresh(auth.refreshToken);
    session.auth = {
      sid: auth.sid,
      refreshToken: refreshed.refreshToken ?? auth.refreshToken,
      recheckAt: new Date(now + RECHECK_INTERVAL_MS),
    };
  } catch (error) {
    if (error instanceof InvalidGrant) return endSession(session, "the provider refused the session");
    if (now > auth.recheckAt.getTime() + MAX_RECHECK_DELAY_MS) {
      return endSession(session, "access could not be re-checked for too long");
    }
    if (error instanceof Unavailable) {
      logger.warn("Could not re-check a session's access; serving it meanwhile", {
        component: "sso",
        userId: session.user.id,
        reason: error.message,
      });
    } else {
      logger.error("Re-checking a session's access failed; serving it meanwhile", {
        component: "sso",
        userId: session.user.id,
      }, error);
    }
  }
};
