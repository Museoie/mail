import { getSso, postLogoutRedirect } from "../../../sso";
import { logger } from "../../../logger";
import { Route } from "../route";

export type LoginDeleteResponse =
  | {
      /** Where to send the browser to end the single sign-on session too. */
      logoutUrl: string;
    }
  | undefined;

export const deleteLoginRoute = new Route<LoginDeleteResponse>(
  "DELETE",
  "/login",
  async (req) => {
    const { auth } = req.session;
    await req.session.destroy();
    const sso = getSso();
    if (!auth || !sso) return { status: "success" };

    // Best effort and off the response path: the session here is already gone,
    // and the provider's logout ends the token along with its own session.
    if (auth.refreshToken) {
      sso.client.revoke(auth.refreshToken).catch((error: unknown) =>
        logger.warn("Could not revoke a refresh token at sign-out", {
          component: "sso",
          reason: error instanceof Error ? error.message : String(error),
        })
      );
    }
    const logoutUrl = await sso.client
      .logoutUrl(postLogoutRedirect(sso.config))
      .catch(() => undefined);
    return { status: "success", body: logoutUrl ? { logoutUrl } : undefined };
  }
);
