import type { HttpRequest } from "../../request";
import { getSso } from "../../../sso";
import { dispatch } from "../route";
import { getAuthLoginRoute } from "./get-login";
import { getAuthCallbackRoute } from "./get-callback";
import { postAuthEventsRoute } from "./post-events";

const routes = [getAuthLoginRoute, getAuthCallbackRoute, postAuthEventsRoute];

/**
 * The provider events route reads its own body: a request there is not
 * parsed as JSON, whatever content type it claims.
 */
export const isRawBodyPath = (pathname: string): boolean =>
  /^\/api\/auth\/events\/?$/i.test(pathname);

/**
 * `/api/auth/*`, with `path` relative to that mount: public, and answered only
 * while single sign-on is on, so with it off they are unknown routes.
 */
const handleAuth = async (req: HttpRequest, path: string): Promise<Response | undefined> =>
  getSso() ? dispatch(routes, req, path) : undefined;

export default handleAuth;

export * from "./get-login";
export * from "./get-callback";
export * from "./post-events";
