import { SignedUser } from "common";
import { version } from "server";
import { ssoMode, type SsoMode } from "../../../sso";
import { Route } from "../route";

export interface LoginGetResponse {
  user?: SignedUser;
  app: { version: string };
  /** How the sign-in page may sign in. */
  sso: SsoMode;
}

export const getLoginRoute = new Route<LoginGetResponse>(
  "GET",
  "/login",
  async (req) => {
    const { user } = req.session;
    return {
      status: "success",
      body: { user, app: { version }, sso: ssoMode() },
      message: user ? undefined : "Not logged in."
    };
  }
);
