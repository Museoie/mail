/**
 * Single sign-on settings: what the server does with them at boot, the
 * sign-in mode it reports to the browser, and the password routes the mode
 * turns off.
 */
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import {
  ADMIN_PASSWORD,
  bootExpectingExit,
  startServer,
  type ServerOptions,
  type TestServer,
} from "../harness";
import { ISSUER, person, signIn, startIssuer, startSignIn, type FakeIssuer } from "../harness/issuer";

const NOT_FOUND = { status: "failed", message: "Not found" };
const LOCAL_SIGN_IN_OFF = {
  status: "failed",
  message: "Password sign-in is turned off on this server. Sign in with SSO instead.",
};

/** Settings that would turn single sign-on on, pointing at a provider nothing reaches. */
const SETTINGS = {
  AUTH_ISSUER: ISSUER,
  AUTH_CLIENT_ID: "mail",
  AUTH_CLIENT_SECRET: "test_key_not_real_do_not_use",
};

const ssoModeOf = async (server: TestServer) => (await server.client().get("/api/users/login")).body.body.sso;

describe("with single sign-on off", () => {
  let server: TestServer;
  beforeAll(async () => {
    server = await startServer();
  }, 90_000);
  afterAll(() => server?.stop());

  test("the /api/auth routes are unknown routes", async () => {
    const client = server.client();
    for (const [method, path] of [
      ["GET", "/api/auth/login"],
      ["GET", "/api/auth/login?returnTo=%2Fmail"],
      ["GET", "/api/auth/callback?code=c0de&state=s"],
      ["GET", "/api/auth/callback?error=access_denied"],
      ["POST", "/api/auth/events"],
      ["GET", "/api/auth"],
      ["DELETE", "/api/auth/login"],
    ] as const) {
      const { status, body, setCookies, headers } = await client.request(method, path);
      expect({ method, path, status, body }).toEqual({ method, path, status: 404, body: NOT_FOUND });
      expect(setCookies).toEqual([]);
      expect(headers.get("location")).toBeNull();
    }
  });

  test("an event posted to it is not read: any body is the same unknown route", async () => {
    const client = server.client();
    for (const [contentType, body] of [
      ["application/secevent+jwt", "aaa.bbb.ccc"],
      ["application/json", "{ this is not JSON"],
      ["text/plain", "x".repeat(70_000)],
    ] as const) {
      const result = await client.request("POST", "/api/auth/events", {
        body,
        headers: { "Content-Type": contentType },
      });
      expect({ contentType, status: result.status, body: result.body }).toEqual({
        contentType,
        status: 404,
        body: NOT_FOUND,
      });
    }
    // Every other path under /api still has its JSON body parsed first.
    const elsewhere = await client.request("POST", "/api/auth/login", {
      body: "{ this is not JSON",
      headers: { "Content-Type": "application/json" },
    });
    expect(elsewhere.status).toBe(400);
    expect(elsewhere.body).toEqual({ status: "error", message: "Bad Request" });
  });

  test("the browser is told to sign in with a password", async () => {
    const { status, body } = await server.client().get("/api/users/login");
    expect(status).toBe(200);
    expect(body).toEqual({
      status: "success",
      body: { app: { version: expect.any(String) }, sso: { enabled: false, localLogin: true } },
      message: "Not logged in.",
    });
  });

  test("password sign-in and sign-out answer as they always did", async () => {
    const client = server.client();
    const signedIn = await client.post("/api/users/login", { username: "admin", password: ADMIN_PASSWORD });
    expect(signedIn.status).toBe(200);
    expect(signedIn.body.status).toBe("success");
    expect(signedIn.body.body.username).toBe("admin");

    const me = await client.get("/api/users/login");
    expect(me.body.body.user.username).toBe("admin");
    expect(me.body.body.sso).toEqual({ enabled: false, localLogin: true });

    const [row] = await server.db.query("SELECT auth_sid, auth_refresh_token, auth_recheck_at FROM sessions");
    expect(row).toEqual({ auth_sid: null, auth_refresh_token: null, auth_recheck_at: null });

    const signedOut = await client.delete("/api/users/login");
    expect(signedOut.status).toBe(200);
    expect(signedOut.body).toEqual({ status: "success" });
    expect(await server.db.query("SELECT 1 FROM sessions")).toEqual([]);
  });

  test("the sign-up and reset routes reach their handlers", async () => {
    const client = server.client();
    const token = await client.post("/api/users/token", { email: "not an address" });
    expect(token.status).toBe(200);
    expect(token.body).toEqual({ status: "failed", message: "Signup failed because email is invalid." });
    const setInfo = await client.post("/api/users/set-info", []);
    expect(setInfo.status).toBe(200);
    expect(setInfo.body).toEqual({ status: "failed", message: "Invalid request body." });
  });
});

describe("AUTH_ISSUER unset or empty leaves every other AUTH_* setting unread", () => {
  const malformed = {
    AUTH_CLIENT_ID: "",
    AUTH_REDIRECT_URI: "not a url",
    AUTH_BACKCHANNEL_URL: "ftp://auth.internal",
    AUTH_LOCAL_LOGIN: "maybe",
    AUTH_LINK_USERS: "not a pair",
  };

  for (const [label, issuer] of [
    ["unset", undefined],
    ["empty", ""],
    ["only spaces", "   "],
  ] as const) {
    test(`AUTH_ISSUER ${label}: the server starts with single sign-on off`, async () => {
      const server = await startServer({ env: { ...malformed, AUTH_ISSUER: issuer } });
      try {
        expect(await ssoModeOf(server)).toEqual({ enabled: false, localLogin: true });
        const { status, body } = await server.client().get("/api/auth/login");
        expect({ status, body }).toEqual({ status: 404, body: NOT_FOUND });
      } finally {
        await server.stop();
      }
    }, 90_000);
  }
});

describe("a misconfiguration refuses to start, naming every problem", () => {
  const refusal = async (env: ServerOptions["env"]) => {
    const { exitCode, logs } = await bootExpectingExit({ env });
    return { exitCode, logs };
  };

  /** The problems the boot listed, in order: the `  - …` lines under the heading. */
  const problemsIn = (logs: string): string[] => {
    const lines = logs.split("\n");
    const heading = lines.findIndex((line) => line.includes("Single sign-on is misconfigured:"));
    if (heading === -1) return [];
    const problems: string[] = [];
    for (const line of lines.slice(heading + 1)) {
      if (!line.startsWith("  - ")) break;
      problems.push(line.slice(4));
    }
    return problems;
  };

  const cases: [string, ServerOptions["env"], string[]][] = [
    [
      "everything wrong at once",
      {
        AUTH_ISSUER: "auth.example.com",
        AUTH_BACKCHANNEL_URL: "ftp://auth.internal",
        AUTH_LOCAL_LOGIN: "yes",
        AUTH_LINK_USERS: "admin",
        APP_HOSTNAME: undefined,
      },
      [
        "AUTH_ISSUER must be an absolute https URL",
        "AUTH_CLIENT_ID is required when AUTH_ISSUER is set",
        "AUTH_CLIENT_SECRET is required when AUTH_ISSUER is set",
        "AUTH_REDIRECT_URI is required when APP_HOSTNAME is not set",
        "AUTH_BACKCHANNEL_URL must be an absolute http(s) URL",
        'AUTH_LOCAL_LOGIN must be true or false, not "yes"',
        'AUTH_LINK_USERS entry "admin" is not <local account>=<auth username>',
      ],
    ],
    [
      "an issuer that is not an http(s) URL",
      { ...SETTINGS, AUTH_ISSUER: "ftp://auth.e2e.test" },
      ["AUTH_ISSUER must be an absolute https URL"],
    ],
    [
      "no client id",
      { ...SETTINGS, AUTH_CLIENT_ID: undefined },
      ["AUTH_CLIENT_ID is required when AUTH_ISSUER is set"],
    ],
    [
      "a client id of spaces",
      { ...SETTINGS, AUTH_CLIENT_ID: "   " },
      ["AUTH_CLIENT_ID is required when AUTH_ISSUER is set"],
    ],
    [
      "no client secret",
      { ...SETTINGS, AUTH_CLIENT_SECRET: undefined },
      ["AUTH_CLIENT_SECRET is required when AUTH_ISSUER is set"],
    ],
    [
      "an empty client secret",
      { ...SETTINGS, AUTH_CLIENT_SECRET: "" },
      ["AUTH_CLIENT_SECRET is required when AUTH_ISSUER is set"],
    ],
    [
      "neither AUTH_REDIRECT_URI nor APP_HOSTNAME",
      { ...SETTINGS, APP_HOSTNAME: undefined },
      ["AUTH_REDIRECT_URI is required when APP_HOSTNAME is not set"],
    ],
    [
      "an APP_HOSTNAME of spaces, which builds no default redirect URI",
      { ...SETTINGS, APP_HOSTNAME: "  " },
      ["AUTH_REDIRECT_URI is required when APP_HOSTNAME is not set"],
    ],
    [
      "a relative AUTH_REDIRECT_URI",
      { ...SETTINGS, AUTH_REDIRECT_URI: "/api/auth/callback" },
      ["AUTH_REDIRECT_URI must be an absolute http(s) URL"],
    ],
    [
      "an AUTH_REDIRECT_URI that is not http(s)",
      { ...SETTINGS, AUTH_REDIRECT_URI: "ftp://mail.example.com/api/auth/callback" },
      ["AUTH_REDIRECT_URI must be an absolute http(s) URL"],
    ],
    [
      "an APP_HOSTNAME that is not a hostname",
      { ...SETTINGS, APP_HOSTNAME: "mail example com" },
      ["APP_HOSTNAME must be a hostname to build the default AUTH_REDIRECT_URI on"],
    ],
    [
      "a back channel that is not http(s)",
      { ...SETTINGS, AUTH_BACKCHANNEL_URL: "auth.internal:3000" },
      ["AUTH_BACKCHANNEL_URL must be an absolute http(s) URL"],
    ],
    [
      "AUTH_LOCAL_LOGIN that is not a boolean",
      { ...SETTINGS, AUTH_LOCAL_LOGIN: "on" },
      ['AUTH_LOCAL_LOGIN must be true or false, not "on"'],
    ],
    [
      "AUTH_LINK_USERS entries that are not one pair",
      { ...SETTINGS, AUTH_LINK_USERS: "admin=alice=x, =bob ,carol=,dave" },
      [
        'AUTH_LINK_USERS entry "admin=alice=x" is not <local account>=<auth username>',
        'AUTH_LINK_USERS entry "=bob" is not <local account>=<auth username>',
        'AUTH_LINK_USERS entry "carol=" is not <local account>=<auth username>',
        'AUTH_LINK_USERS entry "dave" is not <local account>=<auth username>',
      ],
    ],
    [
      "AUTH_LINK_USERS linking the read-only account, or a name twice on either side",
      { ...SETTINGS, AUTH_LINK_USERS: "admin-ro=alice,admin=bob,admin=carol,dave=bob" },
      [
        "AUTH_LINK_USERS cannot link admin-ro, which only signs in locally",
        'AUTH_LINK_USERS names the local account "admin" twice',
        'AUTH_LINK_USERS names the auth username "bob" twice',
      ],
    ],
    // These pass the settings check and are refused by the provider client.
    [
      "a plain-http issuer that is not this machine",
      { ...SETTINGS, AUTH_ISSUER: "http://auth.e2e.test" },
      ["issuer must be an https URL (http only for localhost)"],
    ],
    [
      "an issuer with a query",
      { ...SETTINGS, AUTH_ISSUER: "https://auth.e2e.test/?tenant=1" },
      ["issuer must have no query or fragment"],
    ],
    [
      "an issuer with a fragment",
      { ...SETTINGS, AUTH_ISSUER: "https://auth.e2e.test/#x" },
      ["issuer must have no query or fragment"],
    ],
    [
      "a back channel with a query",
      { ...SETTINGS, AUTH_BACKCHANNEL_URL: "http://127.0.0.1:9/?x=1" },
      ["backChannelUrl must have no query or fragment"],
    ],
    [
      "a back channel with a fragment",
      { ...SETTINGS, AUTH_BACKCHANNEL_URL: "http://127.0.0.1:9/#x" },
      ["backChannelUrl must have no query or fragment"],
    ],
  ];

  for (const [label, env, problems] of cases) {
    test(
      label,
      async () => {
        const { exitCode, logs } = await refusal(env);
        expect({ exitCode, problems: problemsIn(logs) }).toEqual({ exitCode: 1, problems });
        expect(logs).toContain("Fatal error during startup:");
      },
      60_000
    );
  }

  test("the settings problems are all reported before the provider client is asked", async () => {
    // A plain-http issuer alone is the client's refusal; next to a settings
    // problem only the settings problems are listed.
    const { exitCode, logs } = await refusal({
      ...SETTINGS,
      AUTH_ISSUER: "http://auth.e2e.test",
      AUTH_LOCAL_LOGIN: "nope",
    });
    expect({ exitCode, problems: problemsIn(logs) }).toEqual({
      exitCode: 1,
      problems: ['AUTH_LOCAL_LOGIN must be true or false, not "nope"'],
    });
  }, 60_000);
});

describe("with single sign-on on", () => {
  let issuer: FakeIssuer;
  let server: TestServer;

  beforeAll(async () => {
    issuer = await startIssuer();
    server = await startServer({ env: issuer.env() });
  }, 90_000);
  afterAll(async () => {
    await server?.stop();
    await issuer?.stop();
  });

  test("the provider is not contacted at boot, nor by reading the sign-in mode", async () => {
    expect(await ssoModeOf(server)).toEqual({ enabled: true, localLogin: false });
    expect(issuer.calls).toEqual([]);
  });

  test("the browser is told to sign in with SSO only", async () => {
    const { status, body } = await server.client().get("/api/users/login");
    expect(status).toBe(200);
    expect(body).toEqual({
      status: "success",
      body: { app: { version: expect.any(String) }, sso: { enabled: true, localLogin: false } },
      message: "Not logged in.",
    });
  });

  test("password sign-in, sign-up and reset answer 403 with the reason", async () => {
    const client = server.client();
    for (const [path, json] of [
      ["/api/users/login", { username: "admin", password: ADMIN_PASSWORD }],
      ["/api/users/token", { email: "someone@elsewhere.example" }],
      ["/api/users/set-info", { email: "someone@elsewhere.example", username: "someone", password: "pw", token: "t" }],
      // The refusal comes before the route looks at the body.
      ["/api/users/login", []],
      ["/api/users/token", {}],
      ["/api/users/set-info", undefined],
      // Routes match in any letter case, with or without a trailing slash.
      ["/api/users/login/", { username: "admin", password: ADMIN_PASSWORD }],
      ["/API/Users/LOGIN", { username: "admin", password: ADMIN_PASSWORD }],
      ["/api/users/TOKEN/", { email: "someone@elsewhere.example" }],
      ["/api/users/Set-Info/", {}],
    ] as const) {
      const { status, body, setCookies, headers } = await client.post(path, json);
      expect({ path, status, body }).toEqual({ path, status: 403, body: LOCAL_SIGN_IN_OFF });
      expect(setCookies).toEqual([]);
      expect(headers.get("content-type")).toBe("application/json; charset=utf-8");
    }
    expect(await server.db.query("SELECT 1 FROM sessions")).toEqual([]);
    expect(server.mailgun.requests).toEqual([]);
  });

  test("a body that is not JSON is refused as malformed before the mode is looked at", async () => {
    const { status, body } = await server.client().request("POST", "/api/users/login", {
      body: "{ this is not JSON",
      headers: { "Content-Type": "application/json" },
    });
    expect(status).toBe(400);
    expect(body).toEqual({ status: "error", message: "Bad Request" });
  });

  test("the refusals are not counted as failed sign-ins: they never turn into a rate limit", async () => {
    const client = server.client();
    for (let attempt = 0; attempt < 25; attempt++) {
      const { status } = await client.post("/api/users/login", { username: "admin", password: "wrong" });
      expect(status).toBe(403);
    }
  });

  test("every other route answers as it does without single sign-on", async () => {
    const client = server.client();
    const ping = await client.get("/api/ping");
    expect({ status: ping.status, body: ping.body }).toEqual({ status: 200, body: { status: "success" } });

    const health = await client.get("/api/health");
    expect(health.status).toBe(200);

    const signOut = await client.delete("/api/users/login");
    expect({ status: signOut.status, body: signOut.body }).toEqual({ status: 200, body: { status: "success" } });

    const mails = await client.get("/api/mails/accounts");
    expect({ status: mails.status, body: mails.body }).toEqual({
      status: 401,
      body: { status: "failed", message: "Request user is not logged in." },
    });

    const unknownUsersRoute = await client.post("/api/users/elsewhere", {});
    expect({ status: unknownUsersRoute.status, body: unknownUsersRoute.body }).toEqual({
      status: 404,
      body: NOT_FOUND,
    });
    // GET and DELETE of the same paths are not password sign-in.
    const getToken = await client.get("/api/users/token");
    expect({ status: getToken.status, body: getToken.body }).toEqual({ status: 404, body: NOT_FOUND });

    const page = await client.get("/sign-in");
    expect(page.status).toBe(200);
    expect(page.headers.get("content-type")).toContain("text/html");
    expect(issuer.calls).toEqual([]);
  });

  test("an /api/auth path that is not a route is still unknown", async () => {
    const client = server.client();
    for (const [method, path] of [
      ["GET", "/api/auth"],
      ["GET", "/api/auth/"],
      ["GET", "/api/auth/logout"],
      ["GET", "/api/auth/login/extra"],
      ["POST", "/api/auth/login"],
      ["POST", "/api/auth/callback"],
      ["DELETE", "/api/auth/events"],
      ["GET", "/api/auth/events"],
      ["GET", "/api/authx/login"],
    ] as const) {
      const { status, body } = await client.request(method, path);
      expect({ method, path, status, body }).toEqual({ method, path, status: 404, body: NOT_FOUND });
    }
    expect(issuer.calls).toEqual([]);
  });
});

describe("AUTH_LOCAL_LOGIN", () => {
  let issuer: FakeIssuer;
  let server: TestServer;

  beforeAll(async () => {
    issuer = await startIssuer();
    server = await startServer({ env: issuer.env({ AUTH_LOCAL_LOGIN: "true" }) });
  }, 90_000);
  afterAll(async () => {
    await server?.stop();
    await issuer?.stop();
  });

  test("true keeps password sign-in next to single sign-on", async () => {
    expect(await ssoModeOf(server)).toEqual({ enabled: true, localLogin: true });

    const client = server.client();
    const signedIn = await client.post("/api/users/login", { username: "admin", password: ADMIN_PASSWORD });
    expect(signedIn.status).toBe(200);
    expect(signedIn.body.status).toBe("success");
    const me = await client.get("/api/users/login");
    expect(me.body.body.user.username).toBe("admin");
    expect(me.body.body.sso).toEqual({ enabled: true, localLogin: true });

    const wrong = await server.client().post("/api/users/login", { username: "admin", password: "wrong" });
    expect({ status: wrong.status, body: wrong.body }).toEqual({
      status: 200,
      body: { status: "failed", message: "Invalid credentials." },
    });

    const token = await server.client().post("/api/users/token", { email: "not an address" });
    expect(token.body).toEqual({ status: "failed", message: "Signup failed because email is invalid." });
    const setInfo = await server.client().post("/api/users/set-info", []);
    expect(setInfo.body).toEqual({ status: "failed", message: "Invalid request body." });

    // And single sign-on is there too.
    const started = await startSignIn(server.client());
    expect(started.authorizeUrl.origin).toBe(ISSUER);
  });

  test("each accepted spelling", async () => {
    for (const [value, localLogin] of [
      ["TRUE", true],
      ["1", true],
      [" true ", true],
      ["false", false],
      ["FALSE", false],
      ["0", false],
      ["", false],
      ["  ", false],
      [undefined, false],
    ] as const) {
      await server.restart({ AUTH_LOCAL_LOGIN: value });
      expect({ value, mode: await ssoModeOf(server) }).toEqual({
        value,
        mode: { enabled: true, localLogin },
      });
    }
  }, 120_000);
});

describe("the redirect URI", () => {
  const redirectUriOf = async (env: Record<string, string | undefined>) => {
    const issuer = await startIssuer();
    const server = await startServer({ env: issuer.env(env) });
    try {
      const { authorizeUrl } = await startSignIn(server.client());
      return authorizeUrl.searchParams.get("redirect_uri");
    } finally {
      await server.stop();
      await issuer.stop();
    }
  };

  test("defaults to the callback on APP_HOSTNAME, over https", async () => {
    expect(await redirectUriOf({})).toBe("https://app.mail.test/api/auth/callback");
    expect(await redirectUriOf({ APP_HOSTNAME: "  webmail.example.org " })).toBe(
      "https://webmail.example.org/api/auth/callback"
    );
    expect(await redirectUriOf({ APP_HOSTNAME: "localhost:3004", AUTH_REDIRECT_URI: "  " })).toBe(
      "https://localhost:3004/api/auth/callback"
    );
  }, 120_000);

  test("is AUTH_REDIRECT_URI when that is set, with or without APP_HOSTNAME", async () => {
    expect(await redirectUriOf({ AUTH_REDIRECT_URI: " http://localhost:3004/api/auth/callback " })).toBe(
      "http://localhost:3004/api/auth/callback"
    );
    expect(
      await redirectUriOf({ AUTH_REDIRECT_URI: "https://webmail.example.org/sso/return?x=1", APP_HOSTNAME: undefined })
    ).toBe("https://webmail.example.org/sso/return?x=1");
  }, 120_000);
});

describe("the issuer and its back channel", () => {
  const withServer = async (
    issuer: FakeIssuer,
    env: Record<string, string | undefined>,
    run: (server: TestServer) => Promise<void>
  ) => {
    const server = await startServer({ env: issuer.env(env) });
    try {
      await run(server);
    } finally {
      await server.stop();
      await issuer.stop();
    }
  };

  test("a plain-http issuer on this machine works without a back channel", async () => {
    const issuer = await startIssuer({ issuer: "self" });
    expect(issuer.env().AUTH_BACKCHANNEL_URL).toBeUndefined();
    await withServer(issuer, {}, async (server) => {
      const client = server.client();
      const { authorizeUrl, done } = await signIn(client, issuer, { who: person("alice") });
      expect(authorizeUrl.origin + authorizeUrl.pathname).toBe(`${issuer.url}/oauth/authorize`);
      expect(done.headers.get("location")).toBe("/");
      expect((await client.get("/api/users/login")).body.body.user.username).toBe("alice");
      expect(issuer.calls.map((call) => `${call.method} ${call.path}`)).toEqual([
        "GET /.well-known/openid-configuration",
        "POST /oauth/token",
        "GET /.well-known/jwks.json",
      ]);
    });
  }, 90_000);

  test("the back channel replaces the issuer's whole base, path included, for every server-to-server call", async () => {
    const issuer = await startIssuer({ issuer: "https://login.e2e.test/realms/mail/" });
    await withServer(issuer, { AUTH_BACKCHANNEL_URL: `${issuer.url}//` }, async (server) => {
      const client = server.client();
      const { authorizeUrl, done } = await signIn(client, issuer, { who: person("alice") });
      // The browser is sent to the public address.
      expect(authorizeUrl.origin + authorizeUrl.pathname).toBe("https://login.e2e.test/realms/mail/oauth/authorize");
      expect(done.headers.get("location")).toBe("/");
      expect(issuer.calls.map((call) => `${call.method} ${call.path}`)).toEqual([
        "GET /.well-known/openid-configuration",
        "POST /oauth/token",
        "GET /.well-known/jwks.json",
      ]);
    });
  }, 90_000);

  test("an endpoint outside the issuer is called where the discovery document says", async () => {
    const issuer = await startIssuer();
    issuer.alias("/elsewhere/token", "/oauth/token");
    issuer.alias("/elsewhere/keys", "/.well-known/jwks.json");
    issuer.discoveryOverrides.token_endpoint = `${issuer.url}/elsewhere/token`;
    issuer.discoveryOverrides.jwks_uri = `${issuer.url}/elsewhere/keys`;
    await withServer(issuer, {}, async (server) => {
      const { done } = await signIn(server.client(), issuer, { who: person("alice") });
      expect(done.headers.get("location")).toBe("/");
      expect(issuer.calls.map((call) => call.headers.get("host"))).toEqual(Array(3).fill(new URL(issuer.url).host));
      expect(issuer.calls.map((call) => call.path)).toEqual([
        "/.well-known/openid-configuration",
        "/oauth/token",
        "/.well-known/jwks.json",
      ]);
    });
  }, 90_000);

  test("AUTH_AUDIENCE names the audience of access tokens, apart from the client id", async () => {
    const issuer = await startIssuer({ audience: "mailbox" });
    expect(issuer.env().AUTH_AUDIENCE).toBe("mailbox");
    await withServer(issuer, {}, async (server) => {
      const accepted = await signIn(server.client(), issuer, { who: person("alice") });
      expect(accepted.done.headers.get("location")).toBe("/");

      // An access token for the client id is not one for this service...
      const forClient = await signIn(server.client(), issuer, {
        who: person("alice"),
        grant: { access: { aud: issuer.clientId } },
      });
      expect(forClient.done.headers.get("location")).toBe("/sign-in?sso_error=invalid_token");
      // ...while the ID token is always for the client.
      const idForService = await signIn(server.client(), issuer, {
        who: person("alice"),
        grant: { id: { aud: "mailbox" } },
      });
      expect(idForService.done.headers.get("location")).toBe("/sign-in?sso_error=invalid_token");
    });
  }, 90_000);

  test("the server starts while the provider is down, and signs in once it is back", async () => {
    const issuer = await startIssuer();
    await issuer.down();
    await withServer(issuer, {}, async (server) => {
      expect(await ssoModeOf(server)).toEqual({ enabled: true, localLogin: false });
      const refused = await server.client().get("/api/auth/login");
      expect(refused.headers.get("location")).toBe("/sign-in?sso_error=unavailable");
      issuer.up();
      const { done } = await signIn(server.client(), issuer, { who: person("alice") });
      expect(done.headers.get("location")).toBe("/");
    });
  }, 90_000);
});
