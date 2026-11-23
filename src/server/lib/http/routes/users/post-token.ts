import { MailDataToSend } from "common";
import {
  createAuthenticationMail,
  createToken,
  getUser,
  isValidEmail,
  startTimer
} from "server";
import { sendSystemMail } from "../../../mails/send";
import { othersCanReadAddress } from "../../../domains";
import { Route } from "../route";
import { getClientIp, tokenLimiter } from "../../rate-limit";

export type TokenPostResponse = undefined;

export const postTokenRoute = new Route<TokenPostResponse>(
  "POST",
  "/token",
  async (req) => {
    const ip = getClientIp(req);
    const body = req.body;

    if (!body || typeof body !== "object" || Array.isArray(body)) {
      tokenLimiter.recordFailure(ip);
      return { status: "failed", message: "Invalid request body." };
    }

    const { email } = body as Record<string, unknown>;

    // `isValidEmail` opens with `email.split("@")`, so every non-string value
    // throws there instead of failing the address check. An absent JSON body
    // reaches it as `undefined`, which is why the guard covers the shape as
    // well as the type.
    if (typeof email !== "string" || !isValidEmail(email)) {
      tokenLimiter.recordFailure(ip);
      return {
        status: "failed",
        message: "Signup failed because email is invalid."
      };
    }

    // Refuse an address someone other than its account can read, before
    // createToken runs: the link comes back into a domain whose owner or
    // viewers read it, so it would hand them the account (or, for a sign-up,
    // an account under an address they don't control). This route is
    // unauthenticated, and createToken's existing-user branch writes
    // `{token, expiry}` onto the matched row, so the gate has to come first.
    // Same-shape success response as a normal send so no probe signal.
    const existing = await getUser({ email });
    if (await othersCanReadAddress(email, existing?.id)) {
      tokenLimiter.recordFailure(ip);
      return { status: "success" };
    }

    const { id, username, token } = await createToken(email);

    const authenticationEmail = createAuthenticationMail(email, token, username);

    // Sent from the server itself and stored nowhere: the body carries a live
    // sign-up token.
    await sendSystemMail(new MailDataToSend(authenticationEmail));

    startTimer(id);

    // Each successful magic-link send consumes one slot in the per-IP quota
    // (the limit exists to prevent mail-sending abuse). Server errors thrown
    // above don't reach this line, so transient 500s no longer burn the quota.
    tokenLimiter.recordFailure(ip);

    return { status: "success" };
  }
);
