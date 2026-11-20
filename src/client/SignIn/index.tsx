import { useState, useContext, useEffect, ChangeEvent, FormEvent } from "react";
import { useMutation } from "react-query";
import { Link, useLocation } from "react-router-dom";
import { SignedUser, SignedUserType } from "common";
import { LoginPostResponse } from "server";
import {
  Notifier,
  call,
  Context,
  setCacheUser,
  hydrateQueryCache,
  getSignInMode,
  ssoLoginUrl
} from "client";
import LoginIcon from "./components/LoginIcon";
import { ssoErrorMessage } from "./messages";
import "./index.scss";

interface LoginBody {
  password: string;
  email?: string;
  username?: string;
}

/** Set by the redirect that sent a signed-out visitor here. */
interface SignInState {
  returnTo?: string;
}

const Home = () => {
  const { setUserInfo } = useContext(Context);
  const signInMode = getSignInMode();
  const location = useLocation();
  const ssoError = new URLSearchParams(location.search).get("sso_error");
  const returnTo = (location.state as SignInState | null)?.returnTo;
  const [usernameInput, setUsernameInput] = useState("");
  const [passwordInput, setPasswordInput] = useState("");
  const [missingField, setMissingField] = useState(false);

  const onChangeUsername = (e: ChangeEvent<HTMLInputElement>) => {
    setUsernameInput(e.target.value);
  };

  const onChangePassword = (e: ChangeEvent<HTMLInputElement>) => {
    setPasswordInput(e.target.value);
  };

  const mutation = useMutation((body: LoginBody) => {
    return call.post<LoginPostResponse>("/api/users/login", body);
  });

  // Status line under the heading. Errors are announced (role="alert") and
  // styled as errors; progress and success are plain status text.
  let infoMessage: string | undefined;
  let isError = false;

  const ssoMessage = ssoError ? ssoErrorMessage(ssoError) : undefined;
  if (ssoMessage) {
    infoMessage = ssoMessage;
    isError = true;
  }
  if (missingField) {
    infoMessage = "Enter your username or email and your password.";
    isError = true;
  }
  if (mutation.isLoading) infoMessage = "Signing in…";
  if (mutation.isError) {
    infoMessage = "The server didn't respond. Try again in a moment.";
    isError = true;
  }
  // `call` resolves on a 5xx too, so a server-side failure arrives as a
  // parsed body rather than a rejected mutation.
  if (mutation.data?.status === "error") {
    infoMessage = "The server didn't respond. Try again in a moment.";
    isError = true;
  }
  if (mutation.data?.status === "success") infoMessage = "Welcome back!";
  if (mutation.data?.status === "failed") {
    infoMessage = mutation.data?.message || "Wrong username or password.";
    isError = true;
  }

  // The fields are not what a failed single sign-on got wrong.
  const fieldsInvalid = isError && infoMessage !== ssoMessage;

  useEffect(() => {
    const isSuccess = mutation.data?.status === "success";
    if (isSuccess && setUserInfo) {
      const user = new SignedUser(
        mutation.data?.body as unknown as SignedUserType
      );
      setCacheUser(user.id);
      hydrateQueryCache(user.id).finally(() => {
        setTimeout(() => setUserInfo(user), 500);
      });
    }
  }, [mutation.data, setUserInfo]);

  const onSubmit = async (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    if (!usernameInput.trim() || !passwordInput) {
      setMissingField(true);
      return;
    }
    setMissingField(false);
    const body: LoginBody = { password: passwordInput };
    if (usernameInput.includes("@")) body.email = usernameInput;
    else body.username = usernameInput;
    await new Notifier().requestPermission();
    mutation.mutate(body);
  };

  return (
    <div className="container_login">
      <form onSubmit={onSubmit} noValidate>
        <div className="login_card">
          <h2 className="greeting">Sign in</h2>
          <div
            className={"info_message" + (isError ? " error" : "")}
            role={isError ? "alert" : "status"}
          >
            {infoMessage}
          </div>
          {signInMode.enabled ? (
            <a className="login_ui login_sso" href={ssoLoginUrl(returnTo)}>
              <LoginIcon />
              <span>Sign in with SSO</span>
            </a>
          ) : null}
          {signInMode.localLogin ? (
            <>
              {signInMode.enabled ? (
                <div className="login_divider">or with a password</div>
              ) : null}
              <label className="login_label" htmlFor="login-username">
                Username or email
              </label>
              <input
                id="login-username"
                className="login_ui"
                type="text"
                name="username"
                autoComplete="username"
                autoCapitalize="off"
                autoCorrect="off"
                spellCheck={false}
                required
                aria-invalid={fieldsInvalid || undefined}
                value={usernameInput}
                onChange={onChangeUsername}
              />
              <label className="login_label" htmlFor="login-password">
                Password
              </label>
              <input
                id="login-password"
                className="login_ui"
                type="password"
                name="password"
                autoComplete="current-password"
                required
                aria-invalid={fieldsInvalid || undefined}
                value={passwordInput}
                onChange={onChangePassword}
              />
              <button
                className="login_ui login_submit"
                type="submit"
                disabled={mutation.isLoading}
              >
                <LoginIcon />
                <span>Sign in</span>
              </button>
              <div className="card_footer">
                <div className="footer_line">
                  <span>Forgot your password?</span>{" "}
                  <Link to="/set-info?mode=reset">Reset it</Link>
                </div>
                <div className="footer_line">
                  <span>New here?</span> <Link to="/set-info">Create an account</Link>
                </div>
              </div>
            </>
          ) : null}
        </div>
      </form>
    </div>
  );
};

export default Home;
