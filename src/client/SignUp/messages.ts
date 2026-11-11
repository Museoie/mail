/**
 * The copy for the sign-up page. `/set-info` serves three steps: asking for an
 * email to create an account, asking for an email to reset a password (the
 * same token mail resets an existing account), and setting the account's
 * details from the link in that mail.
 */
export type SignUpMode = "signup" | "reset" | "set-info";

export interface SignUpCopy {
  title: string;
  intro: string;
  button: string;
}

export const copyFor = (mode: SignUpMode): SignUpCopy => {
  switch (mode) {
    case "reset":
      return {
        title: "Reset your password",
        intro:
          "Enter your account's email address. We'll send you a link to set a new password.",
        button: "Send reset link"
      };
    case "set-info":
      return {
        title: "Set up your account",
        intro: "Choose a username and password.",
        button: "Save"
      };
    default:
      return {
        title: "Create an account",
        intro: "Enter your email address. We'll send you a link to finish signing up.",
        button: "Send sign-up link"
      };
  }
};

export interface StatusInput {
  mode: SignUpMode;
  passwordMismatch: boolean;
  isLoading: boolean;
  isError: boolean;
  /** The route's answer, when one arrived. */
  data?: { status: string; message?: string };
}

export interface Status {
  message: string;
  isError: boolean;
}

/**
 * The status line under the heading. A refused request shows the server's
 * own reason ("Signup failed because email is invalid.") rather than a
 * generic "Something is wrong", so the user knows what to fix.
 */
export const statusFor = ({
  mode,
  passwordMismatch,
  isLoading,
  isError,
  data
}: StatusInput): Status => {
  if (passwordMismatch) {
    return { message: "The two passwords don't match.", isError: true };
  }
  if (isLoading) {
    return {
      message: mode === "set-info" ? "Saving…" : "Sending…",
      isError: false
    };
  }
  if (isError || data?.status === "error") {
    return {
      message: "The server didn't respond. Try again in a moment.",
      isError: true
    };
  }
  if (data?.status === "success") {
    return {
      message:
        mode === "set-info"
          ? "All set! Signing you in…"
          : "Check your inbox for a link to continue.",
      isError: false
    };
  }
  if (data) {
    return {
      message: data.message || "That didn't work. Check the details and try again.",
      isError: true
    };
  }
  return { message: "", isError: false };
};
