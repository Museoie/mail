import { describe, expect, it } from "bun:test";
import { InvalidGrant, InvalidToken, LoginError, Unavailable } from "../auth-client";
import { signInPathFor, ssoErrorCodeOf } from "./sign-in";

describe("ssoErrorCodeOf", () => {
  it("names each failure of the provider client with a short code", () => {
    expect(ssoErrorCodeOf(new LoginError("authorization_error", "access_denied"))).toBe(
      "access_denied"
    );
    expect(ssoErrorCodeOf(new LoginError("authorization_error", "login_required"))).toBe(
      "login_failed"
    );
    for (const code of ["no_login_cookie", "login_expired", "state_mismatch", "missing_code"] as const) {
      expect(ssoErrorCodeOf(new LoginError(code))).toBe("login_failed");
    }
    expect(ssoErrorCodeOf(new InvalidGrant("refused"))).toBe("login_failed");
    expect(ssoErrorCodeOf(new InvalidToken("bad signature"))).toBe("invalid_token");
    expect(ssoErrorCodeOf(new Unavailable("down"))).toBe("unavailable");
  });

  it("names anything else `failed`, whatever its message says", () => {
    expect(ssoErrorCodeOf(new Error("rt_secret leaked into a message"))).toBe("failed");
    expect(ssoErrorCodeOf("a string")).toBe("failed");
  });
});

describe("signInPathFor", () => {
  it("is the sign-in page with the code", () => {
    expect(signInPathFor("account_exists")).toBe("/sign-in?sso_error=account_exists");
  });
});
