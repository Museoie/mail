import { useState, useContext, useEffect, ChangeEvent, FormEvent } from "react";
import { useMutation } from "react-query";
import { Link } from "react-router-dom";
import { SignedUser, SignedUserType } from "common";
import { LoginPostResponse } from "server";
import {
  Notifier,
  call,
  Context,
  setCacheUser,
  hydrateQueryCache
} from "client";
import LoginIcon from "./components/LoginIcon";
import "./index.scss";

interface LoginBody {
  password: string;
  email?: string;
  username?: string;
}

const Home = () => {
  const { setUserInfo } = useContext(Context);
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
            aria-invalid={isError || undefined}
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
            aria-invalid={isError || undefined}
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
        </div>
      </form>
    </div>
  );
};

export default Home;
