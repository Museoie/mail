import { describe, it, expect, beforeAll, afterAll, afterEach } from "bun:test";
import { GlobalRegistrator } from "@happy-dom/global-registrator";

// The sign-in page follows the mode the server reports at `GET
// /api/users/login`: these tests feed that answer through the real
// `callUser`, then mount the real page.
beforeAll(() => {
  GlobalRegistrator.register();
  (
    globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }
  ).IS_REACT_ACT_ENVIRONMENT = true;
});
const reportSignInMode = async (sso: { enabled: boolean; localLogin: boolean }) => {
  const { call, callUser } = await import("client");
  const originalGet = call.get;
  call.get = (async () => ({
    status: "success",
    body: { app: { version: "" }, sso }
  })) as typeof call.get;
  try {
    await callUser();
  } finally {
    call.get = originalGet;
  }
};

afterAll(async () => {
  // The mode is module state: leave the default for the suites after this one.
  await reportSignInMode({ enabled: false, localLogin: true });
  GlobalRegistrator.unregister();
});

const teardowns: (() => Promise<void>)[] = [];
afterEach(async () => {
  for (const teardown of teardowns.splice(0)) await teardown();
});

const mountSignIn = async (
  sso: { enabled: boolean; localLogin: boolean },
  entry: { pathname: string; search?: string; state?: unknown } = { pathname: "/sign-in" }
) => {
  const { createElement, act } = await import("react");
  const { createRoot } = await import("react-dom/client");
  const { MemoryRouter } = await import("react-router-dom");
  const { QueryClient, QueryClientProvider } = await import("react-query");
  const { Context } = await import("client");
  const SignIn = (await import("./index")).default;
  await reportSignInMode(sso);

  const queryClient = new QueryClient();
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container);
  await act(async () => {
    root.render(
      createElement(
        QueryClientProvider,
        { client: queryClient },
        createElement(
          Context.Provider,
          { value: { setUserInfo: () => {} } as never },
          createElement(
            MemoryRouter,
            { initialEntries: [entry] },
            createElement(SignIn)
          )
        )
      )
    );
  });
  teardowns.push(async () => {
    await act(async () => root.unmount());
    container.remove();
    queryClient.clear();
  });
  return container;
};

const ssoLink = (container: HTMLElement) =>
  container.querySelector<HTMLAnchorElement>("a.login_sso");

describe("sign-in page", () => {
  it("shows only the password form while single sign-on is off", async () => {
    const page = await mountSignIn({ enabled: false, localLogin: true });
    expect(ssoLink(page)).toBeNull();
    expect(page.querySelector("#login-password")).not.toBeNull();
    expect(page.textContent).toContain("Create an account");
  });

  it("shows only single sign-on while password sign-in is off", async () => {
    const page = await mountSignIn({ enabled: true, localLogin: false });
    expect(ssoLink(page)?.getAttribute("href")).toBe("/api/auth/login?returnTo=%2F");
    expect(page.querySelector("#login-username")).toBeNull();
    expect(page.querySelector("#login-password")).toBeNull();
    expect(page.textContent).not.toContain("Create an account");
    expect(page.textContent).not.toContain("Reset it");
  });

  it("offers both while AUTH_LOCAL_LOGIN keeps passwords", async () => {
    const page = await mountSignIn({ enabled: true, localLogin: true });
    expect(ssoLink(page)).not.toBeNull();
    expect(page.querySelector("#login-password")).not.toBeNull();
    expect(page.textContent).toContain("Reset it");
  });

  it("comes back to where the visitor was going", async () => {
    const page = await mountSignIn(
      { enabled: true, localLogin: false },
      { pathname: "/sign-in", state: { returnTo: "/mail/42?x=1" } }
    );
    expect(ssoLink(page)?.getAttribute("href")).toBe(
      "/api/auth/login?returnTo=%2Fmail%2F42%3Fx%3D1"
    );
  });

  it("says why a single sign-on came back without completing", async () => {
    const page = await mountSignIn(
      { enabled: true, localLogin: true },
      { pathname: "/sign-in", search: "?sso_error=account_exists" }
    );
    const status = page.querySelector(".info_message");
    expect(status?.getAttribute("role")).toBe("alert");
    expect(status?.textContent).toContain("administrator to link it");
    // The password fields are not what went wrong.
    expect(page.querySelector("#login-password")?.getAttribute("aria-invalid")).toBeNull();
  });
});

describe("sign-up page", () => {
  const mountSignUp = async (sso: { enabled: boolean; localLogin: boolean }) => {
    const { createElement, act } = await import("react");
    const { createRoot } = await import("react-dom/client");
    const { MemoryRouter, Routes, Route } = await import("react-router-dom");
    const { QueryClient, QueryClientProvider } = await import("react-query");
    const { Context } = await import("client");
    const SignUp = (await import("../SignUp")).default;
    await reportSignInMode(sso);

    const queryClient = new QueryClient();
    const container = document.createElement("div");
    document.body.appendChild(container);
    const root = createRoot(container);
    await act(async () => {
      root.render(
        createElement(
          QueryClientProvider,
          { client: queryClient },
          createElement(
            Context.Provider,
            { value: { setUserInfo: () => {} } as never },
            createElement(
              MemoryRouter,
              { initialEntries: ["/set-info?mode=reset"] },
              createElement(
                Routes,
                null,
                createElement(Route, { path: "/set-info", element: createElement(SignUp) }),
                createElement(Route, { path: "/sign-in", element: "the sign-in page" })
              )
            )
          )
        )
      );
    });
    teardowns.push(async () => {
      await act(async () => root.unmount());
      container.remove();
      queryClient.clear();
    });
    return container;
  };

  it("sends the visitor to sign in while the server keeps no passwords", async () => {
    const page = await mountSignUp({ enabled: true, localLogin: false });
    expect(page.textContent).toBe("the sign-in page");
  });

  it("offers the reset while it does", async () => {
    const page = await mountSignUp({ enabled: true, localLogin: true });
    expect(page.textContent).toContain("Reset your password");
  });
});
