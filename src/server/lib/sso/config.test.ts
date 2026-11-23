import { describe, expect, it } from "bun:test";
import { parseLinkUsers, parseSsoConfig, postLogoutRedirect, SsoConfigError } from "./config";

const BASE = {
  AUTH_ISSUER: "https://auth.example.com",
  AUTH_CLIENT_ID: "mail",
  AUTH_CLIENT_SECRET: "test_key_not_real_do_not_use",
  APP_HOSTNAME: "mail.example.com",
};

const problemsOf = (env: Parameters<typeof parseSsoConfig>[0]): string[] => {
  try {
    parseSsoConfig(env);
  } catch (error) {
    if (error instanceof SsoConfigError) return error.problems;
    throw error;
  }
  throw new Error("expected the settings to be refused");
};

describe("parseSsoConfig", () => {
  it("is off with AUTH_ISSUER unset or empty, whatever else is set", () => {
    expect(parseSsoConfig({})).toBeNull();
    expect(parseSsoConfig({ ...BASE, AUTH_ISSUER: "" })).toBeNull();
    expect(parseSsoConfig({ ...BASE, AUTH_ISSUER: "  " })).toBeNull();
    expect(parseSsoConfig({ AUTH_ISSUER: "", AUTH_LINK_USERS: "not a pair" })).toBeNull();
  });

  it("reads every setting, defaulting the redirect URI from APP_HOSTNAME", () => {
    const config = parseSsoConfig({
      ...BASE,
      AUTH_BACKCHANNEL_URL: "http://auth.internal:3000",
      AUTH_AUDIENCE: "mailbox",
      AUTH_LOCAL_LOGIN: "true",
      AUTH_LINK_USERS: "admin=alice, bob=bob",
    });
    expect(config).toEqual({
      issuer: "https://auth.example.com",
      clientId: "mail",
      clientSecret: "test_key_not_real_do_not_use",
      redirectUri: "https://mail.example.com/api/auth/callback",
      backChannelUrl: "http://auth.internal:3000",
      audience: "mailbox",
      localLogin: true,
      linkUsers: new Map([
        ["alice", "admin"],
        ["bob", "bob"],
      ]),
    });
  });

  it("keeps an explicit AUTH_REDIRECT_URI over the APP_HOSTNAME default", () => {
    const config = parseSsoConfig({
      ...BASE,
      AUTH_REDIRECT_URI: "https://webmail.example.com/api/auth/callback",
    });
    expect(config?.redirectUri).toBe("https://webmail.example.com/api/auth/callback");
  });

  it("leaves password sign-in off unless AUTH_LOCAL_LOGIN turns it on", () => {
    expect(parseSsoConfig(BASE)?.localLogin).toBe(false);
    for (const value of ["true", "TRUE", "1"]) {
      expect(parseSsoConfig({ ...BASE, AUTH_LOCAL_LOGIN: value })?.localLogin).toBe(true);
    }
    for (const value of ["", "false", "0"]) {
      expect(parseSsoConfig({ ...BASE, AUTH_LOCAL_LOGIN: value })?.localLogin).toBe(false);
    }
  });

  it("refuses to start listing every problem at once", () => {
    expect(
      problemsOf({
        AUTH_ISSUER: "auth.example.com",
        AUTH_BACKCHANNEL_URL: "ftp://auth.internal",
        AUTH_LOCAL_LOGIN: "yes",
        AUTH_LINK_USERS: "admin",
      })
    ).toEqual([
      "AUTH_ISSUER must be an absolute https URL",
      "AUTH_CLIENT_ID is required when AUTH_ISSUER is set",
      "AUTH_CLIENT_SECRET is required when AUTH_ISSUER is set",
      "AUTH_REDIRECT_URI is required when APP_HOSTNAME is not set",
      "AUTH_BACKCHANNEL_URL must be an absolute http(s) URL",
      'AUTH_LOCAL_LOGIN must be true or false, not "yes"',
      'AUTH_LINK_USERS entry "admin" is not <local account>=<auth username>',
    ]);
  });

  it("refuses a malformed redirect URI, and a hostname that makes one", () => {
    expect(problemsOf({ ...BASE, AUTH_REDIRECT_URI: "/api/auth/callback" })).toEqual([
      "AUTH_REDIRECT_URI must be an absolute http(s) URL",
    ]);
    expect(problemsOf({ ...BASE, APP_HOSTNAME: "mail example com" })).toEqual([
      "APP_HOSTNAME must be a hostname to build the default AUTH_REDIRECT_URI on",
    ]);
  });

  it("names its problems in the error message", () => {
    expect(() => parseSsoConfig({ AUTH_ISSUER: "https://auth.example.com" })).toThrow(
      /Single sign-on is misconfigured:\n {2}- AUTH_CLIENT_ID is required/
    );
  });
});

describe("parseLinkUsers", () => {
  const parse = (value: string | undefined) => {
    const problems: string[] = [];
    return { links: parseLinkUsers(value, problems), problems };
  };

  it("maps each auth username to its local account, skipping empty entries", () => {
    expect(parse("admin=alice,,bob = bob ,").links).toEqual(
      new Map([
        ["alice", "admin"],
        ["bob", "bob"],
      ])
    );
    expect(parse(undefined)).toEqual({ links: new Map(), problems: [] });
  });

  it("refuses entries that are not one pair", () => {
    expect(parse("admin=alice=x,=bob,carol=").problems).toEqual([
      'AUTH_LINK_USERS entry "admin=alice=x" is not <local account>=<auth username>',
      'AUTH_LINK_USERS entry "=bob" is not <local account>=<auth username>',
      'AUTH_LINK_USERS entry "carol=" is not <local account>=<auth username>',
    ]);
  });

  it("refuses a name used twice on either side", () => {
    const { links, problems } = parse("viewer=alice,admin=bob,admin=carol,dave=bob");
    expect(problems).toEqual([
      'AUTH_LINK_USERS names the local account "admin" twice',
      'AUTH_LINK_USERS names the auth username "bob" twice',
    ]);
    expect(links).toEqual(new Map([["alice", "viewer"], ["bob", "admin"]]));
  });
});

describe("postLogoutRedirect", () => {
  it("is the root of the redirect URI's origin", () => {
    expect(postLogoutRedirect({ redirectUri: "https://mail.example.com:8443/api/auth/callback" })).toBe(
      "https://mail.example.com:8443/"
    );
  });
});
