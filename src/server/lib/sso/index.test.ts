import { afterEach, describe, expect, it } from "bun:test";
import {
  getSso,
  initializeSso,
  isLocalLoginAllowed,
  requireSso,
  resetSso,
  SsoConfigError,
  ssoMode,
} from ".";

const ENV = {
  AUTH_ISSUER: "https://auth.example.test",
  AUTH_CLIENT_ID: "mail",
  AUTH_CLIENT_SECRET: "test_key_not_real_do_not_use",
  APP_HOSTNAME: "mail.example.test",
};

afterEach(resetSso);

describe("initializeSso", () => {
  it("stays off without AUTH_ISSUER, leaving every sign-in as it was", () => {
    expect(initializeSso({})).toBeNull();
    expect(getSso()).toBeNull();
    expect(ssoMode()).toEqual({ enabled: false, localLogin: true });
    expect(isLocalLoginAllowed()).toBe(true);
    expect(() => requireSso()).toThrow("Single sign-on is off");
  });

  it("builds the provider client without contacting the provider", () => {
    let fetched = 0;
    const sso = initializeSso(ENV, {
      fetch: (async () => {
        fetched++;
        return new Response(null, { status: 500 });
      }) as unknown as typeof fetch,
    });
    expect(sso?.config.clientId).toBe("mail");
    expect(getSso()).toBe(sso);
    expect(requireSso()).toBe(sso!);
    expect(fetched).toBe(0);
  });

  it("turns password sign-in off unless AUTH_LOCAL_LOGIN keeps it", () => {
    initializeSso(ENV);
    expect(ssoMode()).toEqual({ enabled: true, localLogin: false });
    expect(isLocalLoginAllowed()).toBe(false);
    initializeSso({ ...ENV, AUTH_LOCAL_LOGIN: "true" });
    expect(ssoMode()).toEqual({ enabled: true, localLogin: true });
    expect(isLocalLoginAllowed()).toBe(true);
  });

  it("refuses settings the provider client refuses, as a configuration problem", () => {
    const attempt = () => initializeSso({ ...ENV, AUTH_ISSUER: "http://auth.example.test" });
    expect(attempt).toThrow(SsoConfigError);
    expect(attempt).toThrow("issuer must be an https URL (http only for localhost)");
    expect(getSso()).toBeNull();
  });
});
