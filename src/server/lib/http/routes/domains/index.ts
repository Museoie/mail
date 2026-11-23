import type { HttpRequest } from "../../request";
import { authRequired, dispatch } from "../route";
import {
  getDomainsRoute,
  postDomainRoute,
  postVerifyDomainRoute,
  deleteDomainRoute,
  postViewerRoute,
  deleteViewerRoute,
} from "./routes";

// The static paths ahead of the parameterized ones, so `/verify` and
// `/viewers` are never read as a domain name.
const routes = [
  getDomainsRoute,
  postDomainRoute,
  postVerifyDomainRoute,
  postViewerRoute,
  deleteViewerRoute,
  deleteDomainRoute,
];

/** `/api/domains/*`, with `path` relative to that mount. Every route needs a session. */
const handleDomains = async (req: HttpRequest, path: string): Promise<Response | undefined> => {
  const denied = authRequired(req);
  if (denied) return denied;
  return dispatch(routes, req, path);
};

export default handleDomains;

export * from "./routes";
