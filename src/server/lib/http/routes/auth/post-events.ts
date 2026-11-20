import { InvalidToken, Unavailable } from "../../../auth-client";
import { requireSso } from "../../../sso";
import { handleSsoEvent } from "../../../sso/events";
import { logger } from "../../../logger";
import { HttpError, readTextBody } from "../../request";
import { json } from "../../respond";
import { Route } from "../route";

/** A security event token is a few hundred bytes; anything this size is not one. */
const EVENT_BODY_LIMIT = 64 * 1024;

/**
 * Receives the provider's events (`application/secevent+jwt`). The body's
 * signature is what authenticates the call, so the route is public and reads
 * the body as text whatever its content type. A `2xx` acknowledges; anything
 * else makes the provider retry.
 */
export const postAuthEventsRoute = new Route<undefined>("POST", "/events", async (req) => {
  const sso = requireSso();
  let body: string;
  try {
    body = await readTextBody(req.raw, EVENT_BODY_LIMIT);
  } catch (error) {
    if (error instanceof HttpError) {
      return json({ status: "failed", message: error.message }, error.status);
    }
    throw error;
  }
  let event;
  try {
    event = await sso.client.readEvent(body);
  } catch (error) {
    if (error instanceof InvalidToken) {
      logger.warn("Refused a provider event", { component: "sso", reason: error.message });
      return json({ status: "failed", message: "Invalid event." }, 400);
    }
    if (error instanceof Unavailable) {
      logger.warn("Could not verify a provider event", { component: "sso", reason: error.message });
      return json({ status: "error", message: "Try again later." }, 503);
    }
    throw error;
  }
  logger.info("Provider event", { component: "sso", type: event.type, jti: event.jti });
  await handleSsoEvent(event);
  return json({ status: "success" }, 202);
});
