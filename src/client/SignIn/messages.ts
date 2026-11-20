/**
 * What the sign-in page says when a single sign-on came back without
 * completing. The server names the reason with `?sso_error=<code>`.
 */
export const ssoErrorMessage = (code: string): string => {
  switch (code) {
    case "account_exists":
      return "An account with your username already exists here. Ask the administrator to link it to your single sign-on account.";
    case "invalid_username":
      return "Your username can't be used for a mailbox here. Ask the administrator for help.";
    case "access_denied":
      return "Single sign-on was cancelled, or your account has no access to Mail.";
    case "login_failed":
    case "invalid_token":
      return "That sign-in didn't complete. Try again.";
    case "unavailable":
      return "The sign-in service isn't reachable right now. Try again in a moment.";
    default:
      return "Something went wrong while signing in. Try again.";
  }
};
