
/** The environment single sign-on is configured from. */
export interface SsoEnv {
  AUTH_ISSUER?: string;
  AUTH_CLIENT_ID?: string;
  AUTH_CLIENT_SECRET?: string;
  AUTH_REDIRECT_URI?: string;
  AUTH_BACKCHANNEL_URL?: string;
  AUTH_AUDIENCE?: string;
  AUTH_LOCAL_LOGIN?: string;
  AUTH_LINK_USERS?: string;
  /** The web app's public hostname, which the default redirect URI is built on. */
  APP_HOSTNAME?: string;
}

export interface SsoConfig {
  issuer: string;
  clientId: string;
  clientSecret: string;
  redirectUri: string;
  backChannelUrl?: string;
  audience?: string;
  /** Password sign-in, sign-up and reset, and local passwords on IMAP/SMTP, stay on. */
  localLogin: boolean;
  /** Auth username → the local username its first sign-in links to. */
  linkUsers: Map<string, string>;
}

export const readSsoEnv = (): SsoEnv => {
  const {
    AUTH_ISSUER,
    AUTH_CLIENT_ID,
    AUTH_CLIENT_SECRET,
    AUTH_REDIRECT_URI,
    AUTH_BACKCHANNEL_URL,
    AUTH_AUDIENCE,
    AUTH_LOCAL_LOGIN,
    AUTH_LINK_USERS,
    APP_HOSTNAME,
  } = process.env;
  return {
    AUTH_ISSUER,
    AUTH_CLIENT_ID,
    AUTH_CLIENT_SECRET,
    AUTH_REDIRECT_URI,
    AUTH_BACKCHANNEL_URL,
    AUTH_AUDIENCE,
    AUTH_LOCAL_LOGIN,
    AUTH_LINK_USERS,
    APP_HOSTNAME,
  };
};

export class SsoConfigError extends Error {
  constructor(readonly problems: string[]) {
    super(
      "Single sign-on is misconfigured:\n" + problems.map((problem) => `  - ${problem}`).join("\n")
    );
    this.name = "SsoConfigError";
  }
}

const isHttpUrl = (value: string): boolean => {
  try {
    const url = new URL(value);
    return url.protocol === "https:" || url.protocol === "http:";
  } catch {
    return false;
  }
};

const parseBoolean = (name: string, value: string | undefined, problems: string[]): boolean => {
  const normalized = (value ?? "").trim().toLowerCase();
  if (normalized === "true" || normalized === "1") return true;
  if (normalized === "" || normalized === "false" || normalized === "0") return false;
  problems.push(`${name} must be true or false, not "${value}"`);
  return false;
};

/**
 * `alice=alice,admin=bob` → auth username → local username. Each local account
 * and each auth username may appear once, and the read-only account is never
 * linked: it is a view onto admin's mail that only a local password opens.
 */
export const parseLinkUsers = (
  value: string | undefined,
  problems: string[]
): Map<string, string> => {
  const links = new Map<string, string>();
  const locals = new Set<string>();
  for (const entry of (value ?? "").split(",")) {
    if (entry.trim() === "") continue;
    const parts = entry.split("=");
    const local = parts[0]?.trim() ?? "";
    const remote = parts[1]?.trim() ?? "";
    if (parts.length !== 2 || !local || !remote) {
      problems.push(`AUTH_LINK_USERS entry "${entry.trim()}" is not <local account>=<auth username>`);
      continue;
    }
    if (locals.has(local)) {
      problems.push(`AUTH_LINK_USERS names the local account "${local}" twice`);
      continue;
    }
    if (links.has(remote)) {
      problems.push(`AUTH_LINK_USERS names the auth username "${remote}" twice`);
      continue;
    }
    locals.add(local);
    links.set(remote, local);
  }
  return links;
};

/**
 * Single sign-on settings, or null when `AUTH_ISSUER` is unset or empty, which
 * turns the feature off. Throws {@link SsoConfigError} naming every problem at
 * once otherwise. Reads nothing over the network.
 */
export const parseSsoConfig = (env: SsoEnv): SsoConfig | null => {
  const issuer = env.AUTH_ISSUER?.trim();
  if (!issuer) return null;

  const problems: string[] = [];
  if (!isHttpUrl(issuer)) problems.push("AUTH_ISSUER must be an absolute https URL");

  const clientId = env.AUTH_CLIENT_ID?.trim() ?? "";
  if (!clientId) problems.push("AUTH_CLIENT_ID is required when AUTH_ISSUER is set");
  const clientSecret = env.AUTH_CLIENT_SECRET ?? "";
  if (!clientSecret) problems.push("AUTH_CLIENT_SECRET is required when AUTH_ISSUER is set");

  let redirectUri = env.AUTH_REDIRECT_URI?.trim() ?? "";
  const hostname = env.APP_HOSTNAME?.trim();
  if (!redirectUri && hostname) redirectUri = `https://${hostname}/api/auth/callback`;
  if (!redirectUri) {
    problems.push("AUTH_REDIRECT_URI is required when APP_HOSTNAME is not set");
  } else if (!isHttpUrl(redirectUri)) {
    problems.push(
      env.AUTH_REDIRECT_URI?.trim()
        ? "AUTH_REDIRECT_URI must be an absolute http(s) URL"
        : "APP_HOSTNAME must be a hostname to build the default AUTH_REDIRECT_URI on"
    );
  }

  const backChannelUrl = env.AUTH_BACKCHANNEL_URL?.trim() || undefined;
  if (backChannelUrl !== undefined && !isHttpUrl(backChannelUrl)) {
    problems.push("AUTH_BACKCHANNEL_URL must be an absolute http(s) URL");
  }
  const audience = env.AUTH_AUDIENCE?.trim() || undefined;

  const localLogin = parseBoolean("AUTH_LOCAL_LOGIN", env.AUTH_LOCAL_LOGIN, problems);
  const linkUsers = parseLinkUsers(env.AUTH_LINK_USERS, problems);

  if (problems.length) throw new SsoConfigError(problems);
  return {
    issuer,
    clientId,
    clientSecret,
    redirectUri,
    backChannelUrl,
    audience,
    localLogin,
    linkUsers,
  };
};

/** Where the provider sends the browser after it ends the SSO session. */
export const postLogoutRedirect = (config: Pick<SsoConfig, "redirectUri">): string =>
  `${new URL(config.redirectUri).origin}/`;
