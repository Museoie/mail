import { describe, it, expect } from "bun:test";
import { ssoErrorMessage } from "./messages";

describe("ssoErrorMessage", () => {
  it("tells someone whose username is taken that the administrator links accounts", () => {
    expect(ssoErrorMessage("account_exists")).toContain("administrator to link it");
  });

  it("says why for each code the server sends", () => {
    expect(ssoErrorMessage("invalid_username")).toContain("can't be used for a mailbox");
    expect(ssoErrorMessage("access_denied")).toContain("no access to Mail");
    expect(ssoErrorMessage("unavailable")).toContain("isn't reachable");
    expect(ssoErrorMessage("login_failed")).toBe("That sign-in didn't complete. Try again.");
    expect(ssoErrorMessage("invalid_token")).toBe("That sign-in didn't complete. Try again.");
  });

  it("falls back to a general message for a code it does not know, never echoing it", () => {
    const message = ssoErrorMessage("<script>");
    expect(message).toBe("Something went wrong while signing in. Try again.");
  });
});
