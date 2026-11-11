import { describe, it, expect } from "bun:test";
import { copyFor, statusFor } from "./messages";

const base = {
  mode: "signup" as const,
  passwordMismatch: false,
  isLoading: false,
  isError: false
};

describe("copyFor", () => {
  it("titles password reset as a reset, not a welcome", () => {
    expect(copyFor("reset").title).toBe("Reset your password");
    expect(copyFor("reset").button).toBe("Send reset link");
  });

  it("titles sign-up and the details step", () => {
    expect(copyFor("signup").title).toBe("Create an account");
    expect(copyFor("set-info").title).toBe("Set up your account");
  });
});

describe("statusFor", () => {
  it("shows the server's reason when the request is refused", () => {
    expect(
      statusFor({
        ...base,
        data: {
          status: "failed",
          message: "Signup failed because email is invalid."
        }
      })
    ).toEqual({
      message: "Signup failed because email is invalid.",
      isError: true
    });
  });

  it("falls back to a specific prompt when the refusal has no reason", () => {
    expect(statusFor({ ...base, data: { status: "failed" } }).message).toBe(
      "That didn't work. Check the details and try again."
    );
  });

  it("reports a server error separately from a refusal", () => {
    expect(statusFor({ ...base, data: { status: "error" } })).toEqual({
      message: "The server didn't respond. Try again in a moment.",
      isError: true
    });
    expect(statusFor({ ...base, isError: true }).isError).toBe(true);
  });

  it("puts a password mismatch first", () => {
    expect(
      statusFor({ ...base, passwordMismatch: true, data: { status: "success" } })
    ).toEqual({ message: "The two passwords don't match.", isError: true });
  });

  it("tells the user to check their inbox after a sent link", () => {
    expect(
      statusFor({ ...base, mode: "reset", data: { status: "success" } })
    ).toEqual({
      message: "Check your inbox for a link to continue.",
      isError: false
    });
  });

  it("is empty before anything happens", () => {
    expect(statusFor(base)).toEqual({ message: "", isError: false });
  });
});
