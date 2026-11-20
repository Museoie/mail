import { requireSso } from "../../../sso";
import { signInPathFor, ssoErrorCodeOf } from "../../../sso/sign-in";
import { logger } from "../../../logger";
import { sendAlarm } from "../../../alarm";
import { redirect } from "../../respond";
import { Route } from "../route";

/**
 * Starts a single sign-on: sends the browser to the provider, carrying the
 * local path to come back to in the login cookie.
 */
export const getAuthLoginRoute = new Route<undefined>("GET", "/login", async (req) => {
  const sso = requireSso();
  const returnTo = [req.query.returnTo].flat()[0];
  try {
    const { redirectUrl, loginCookie } = await sso.client.startLogin(req.raw, { returnTo });
    return redirect(redirectUrl, [loginCookie]);
  } catch (error) {
    const code = ssoErrorCodeOf(error);
    const reason = error instanceof Error ? error.message : String(error);
    if (code === "unavailable") {
      logger.warn("Could not start a single sign-on", { component: "sso", reason });
    } else {
      logger.error("Starting a single sign-on failed", { component: "sso" }, error);
      sendAlarm("Single sign-on failed", `**Error:** ${reason}`).catch(() => undefined);
    }
    return redirect(signInPathFor(code));
  }
});
