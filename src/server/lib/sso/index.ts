import { createAuthClient, type AuthClient, type AuthClientOptions } from "../auth-client";
import { logger } from "../logger";
import { parseSsoConfig, readSsoEnv, SsoConfigError, type SsoConfig, type SsoEnv } from "./config";

export * from "./config";

/** Single sign-on as this process runs it: its settings and the provider client. */
export interface Sso {
  config: SsoConfig;
  client: AuthClient;
}

let current: Sso | null = null;

/**
 * Reads the single sign-on settings and builds the provider client, or turns
 * the feature off when `AUTH_ISSUER` is unset. Contacts nothing: the client
 * fetches the provider's discovery document on its first use. Throws
 * {@link SsoConfigError} listing every problem with the settings.
 *
 * `clientOptions` is for tests, which point the client at a fake provider.
 */
export const initializeSso = (
  env: SsoEnv = readSsoEnv(),
  clientOptions: Pick<AuthClientOptions, "fetch" | "now"> = {}
): Sso | null => {
  const config = parseSsoConfig(env);
  if (!config) {
    current = null;
    return null;
  }
  let client: AuthClient;
  try {
    client = createAuthClient({
      issuer: config.issuer,
      backChannelUrl: config.backChannelUrl,
      clientId: config.clientId,
      clientSecret: config.clientSecret,
      redirectUri: config.redirectUri,
      audience: config.audience,
      ...clientOptions,
    });
  } catch (error) {
    throw new SsoConfigError([error instanceof Error ? error.message : String(error)]);
  }
  current = { config, client };
  logger.info("Single sign-on is on", {
    issuer: config.issuer,
    localLogin: config.localLogin,
    linkedAccounts: config.linkUsers.size,
  });
  return current;
};

/** The running single sign-on, or null when it is off. */
export const getSso = (): Sso | null => current;

/** The running single sign-on, for code that only runs while it is on. */
export const requireSso = (): Sso => {
  if (!current) throw new Error("Single sign-on is off");
  return current;
};

/** Turns single sign-on off again. For tests. */
export const resetSso = (): void => {
  current = null;
};

/** How the browser may sign in: what the sign-in page shows. */
export interface SsoMode {
  enabled: boolean;
  localLogin: boolean;
}

export const ssoMode = (): SsoMode => ({
  enabled: current !== null,
  localLogin: current === null || current.config.localLogin,
});

/** Whether passwords stored in this app's own database may sign in. */
export const isLocalLoginAllowed = (): boolean => ssoMode().localLogin;
