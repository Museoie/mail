import { SignedUser } from "common";
import { requireSso } from "../../../sso";
import { postgresAccountDirectory, resolveSsoAccount } from "../../../sso/accounts";
import { ssoSessionFields } from "../../../sso/session";
import { CLEAR_LOGIN_COOKIE, signInPathFor, ssoErrorCodeOf } from "../../../sso/sign-in";
import { logger } from "../../../logger";
import { sendAlarm } from "../../../alarm";
import { issueAuthenticatedSession } from "../../issue-session";
import { redirect } from "../../respond";
import { Route } from "../route";

/**
 * Where the provider sends the browser back. Finds, links or creates the
 * local account, starts the app's own session for it, and lands on the path
 * the sign-in started from. Any failure lands on the sign-in page with a code
 * saying why.
 */
export const getAuthCallbackRoute = new Route<undefined>("GET", "/callback", async (req) => {
  const sso = requireSso();
  try {
    const login = await sso.client.finishLogin(req.raw);
    const result = await resolveSsoAccount(
      {
        sub: login.claims.sub,
        username: login.claims.username,
        email: login.idClaims.email,
        emailVerified: login.idClaims.email_verified,
      },
      sso.config.linkUsers,
      postgresAccountDirectory
    );
    if (!result.ok) {
      logger.info("Refused a single sign-on", {
        component: "sso",
        reason: result.reason,
        username: login.claims.username,
      });
      return redirect(signInPathFor(result.reason), [CLEAR_LOGIN_COOKIE]);
    }

    const { account, outcome } = result;
    const user = new SignedUser({
      id: account.userId,
      username: account.username,
      email: account.email ?? undefined,
      passwordVersion: account.passwordVersion,
    });
    await issueAuthenticatedSession(req, user);
    req.session.auth = ssoSessionFields(login);
    // Written before the redirect, as the password sign-in does: the cookie
    // means nothing until the row behind it exists.
    await req.session.save();

    logger.info("Single sign-on completed", {
      component: "sso",
      outcome,
      userId: account.userId,
      username: account.username,
    });
    return redirect(login.returnTo, [login.clearCookie]);
  } catch (error) {
    const code = ssoErrorCodeOf(error);
    if (code === "failed") {
      logger.error("Single sign-on failed", { component: "sso" }, error);
      const reason = error instanceof Error ? error.message : String(error);
      sendAlarm("Single sign-on failed", `**Error:** ${reason}`).catch(() => undefined);
    } else {
      logger.warn("Single sign-on did not complete", { component: "sso", reason: code });
    }
    return redirect(signInPathFor(code), [CLEAR_LOGIN_COOKIE]);
  }
});
