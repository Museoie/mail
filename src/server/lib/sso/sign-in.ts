import { InvalidGrant, InvalidToken, LoginError, Unavailable } from "../auth-client";
import type { SsoAccountRefusal } from "./accounts";

/**
 * Why a single sign-on did not complete, as the sign-in page reads it from
 * `?sso_error=`. A short code, never a provider message or a token.
 */
export type SsoErrorCode =
  | SsoAccountRefusal
  | "access_denied"
  | "login_failed"
  | "invalid_token"
  | "unavailable"
  | "failed";

/** The code for a failure the provider client threw; `failed` for anything else. */
export const ssoErrorCodeOf = (error: unknown): SsoErrorCode => {
  if (error instanceof LoginError) {
    return error.code === "authorization_error" && error.error === "access_denied"
      ? "access_denied"
      : "login_failed";
  }
  if (error instanceof InvalidGrant) return "login_failed";
  if (error instanceof InvalidToken) return "invalid_token";
  if (error instanceof Unavailable) return "unavailable";
  return "failed";
};

export const signInPathFor = (code: SsoErrorCode): string => `/sign-in?sso_error=${code}`;

/**
 * Deletes the login cookie the provider client set when the sign-in started.
 * The client hands this value back only for a sign-in that completes; a
 * failed one would otherwise leave the cookie behind until it expires.
 */
export const CLEAR_LOGIN_COOKIE = "__Host-login=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0";
