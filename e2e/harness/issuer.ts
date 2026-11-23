import crypto from "crypto";
import type { ApiClient, ApiResult } from "./client";

/**
 * What tokens carry in `iss` unless a test says otherwise: a public-looking
 * URL nothing listens on. The server under test reaches the fake provider
 * through `AUTH_BACKCHANNEL_URL` instead.
 */
export const ISSUER = "https://auth.e2e.test";

export const CLIENT_ID = "mail";

/** The cookie the server keeps a sign-in's state in between its two requests. */
export const LOGIN_COOKIE = "__Host-login";

/** A person as the provider knows them. */
export interface Identity {
  sub: string;
  username: string;
  name: string;
  role: string;
  /** The provider's session id, which `session.ended` names. */
  sid: string;
  email?: string | null;
  emailVerified?: boolean;
}

/**
 * A provider identity named after `username`: subject `usr_<username>`,
 * session `ses_<username>`, and a verified address outside the mail domain.
 *
 * ```ts
 * const alice = person("alice", { emailVerified: false });
 * ```
 */
export const person = (username: string, overrides: Partial<Identity> = {}): Identity => ({
  sub: `usr_${username}`,
  username,
  name: username.slice(0, 1).toUpperCase() + username.slice(1),
  role: "user",
  sid: `ses_${username}`,
  email: `${username}@people.example`,
  emailVerified: true,
  ...overrides,
});

/** One request the server under test made to the provider. */
export interface IssuerCall {
  method: string;
  path: string;
  query: URLSearchParams;
  headers: Headers;
  /** The raw body. */
  body: string;
  /** The body read as a form; empty for a GET. */
  form: URLSearchParams;
}

export type IssuerReply = (call: IssuerCall) => Response | Promise<Response>;

/** How a token is signed; everything defaults to a valid token from key `k1`. */
export interface MintOptions {
  /** The key that signs, and the `kid` the header names. */
  kid?: string;
  /** Header fields to add or replace; `undefined` removes one. */
  header?: Record<string, unknown>;
  /** Flips a bit of the signature after signing. */
  tamper?: boolean;
}

/** What the token endpoint answers for one authorization code. */
export interface GrantOptions {
  /** Access token claims to add or replace; `undefined` removes one. */
  access?: Record<string, unknown>;
  /** ID token claims to add or replace; `undefined` removes one. */
  id?: Record<string, unknown>;
  accessMint?: MintOptions;
  idMint?: MintOptions;
  /** The refresh token to issue; `null` issues none. A random one by default. */
  refreshToken?: string | null;
  /** A last change to the token response before it is sent. */
  answer?: (answer: Record<string, unknown>) => Record<string, unknown>;
}

/** An app password or API key the introspection endpoint knows. */
export interface AppPassword {
  secret: string;
  owner: Identity;
  /** Space-separated, as introspection answers it. */
  scope: string;
  /** The credential's id, answered as `jti`. */
  id: string;
  active: boolean;
}

interface SigningKey {
  kid: string;
  privateKey: crypto.KeyObject;
  jwk: Record<string, unknown>;
  published: boolean;
}

interface PendingGrant {
  who: Identity;
  nonce: string | null;
  challenge: string | null;
  redirectUri: string | null;
  options: GrantOptions;
}

export interface IssuerOptions {
  /**
   * The issuer URL tokens and the discovery document name. `"self"` uses the
   * fake's own `http://127.0.0.1:<port>` address, for a server configured
   * without a back channel.
   */
  issuer?: string | "self";
  clientId?: string;
  clientSecret?: string;
  /** The `aud` of access tokens and events when it is not the client id. */
  audience?: string;
}

const b64url = (value: string | Uint8Array) => Buffer.from(value).toString("base64url");

/** RFC 6749 §2.3.1: how a client id or secret is encoded before Basic auth. */
export const formEncode = (value: string) => encodeURIComponent(value).replace(/%20/g, "+");

export const jsonReply = (status: number, body: unknown): Response =>
  new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });

/** The fields the server writes into its login cookie. */
export interface LoginCookieState {
  state?: string;
  nonce?: string;
  verifier?: string;
  returnTo?: string;
  iat?: number;
  maxAge?: number;
  fresh?: boolean;
  [field: string]: unknown;
}

/** The state inside a login cookie's value. */
export const decodeLoginCookie = (value: string): LoginCookieState =>
  JSON.parse(Buffer.from(value, "base64url").toString("utf8")) as LoginCookieState;

/** A login cookie value carrying `state`, as the server itself would write it. */
export const encodeLoginCookie = (state: unknown): string => b64url(JSON.stringify(state));

/**
 * A fake OpenID Connect provider on `127.0.0.1`: discovery, a key set, the
 * token, introspection, revocation and end-session endpoints. It behaves like
 * a small real provider by default (codes bound to their PKCE challenge,
 * refresh tokens that can be revoked, app passwords it knows), and a test can
 * script any answer, take it down, or make an endpoint hang.
 *
 * ```ts
 * const issuer = await startIssuer();
 * const server = await startServer({ env: issuer.env() });
 * const client = server.client();
 * const { done } = await signIn(client, issuer, { who: person("alice") });
 * ```
 */
export const startIssuer = async (options: IssuerOptions = {}) => {
  const clientId = options.clientId ?? CLIENT_ID;
  // Generated per run, with characters that only survive Basic authentication
  // when each part is form-encoded first.
  const clientSecret = options.clientSecret ?? `${crypto.randomBytes(12).toString("base64url")} +/:=~!*'()&%`;

  const keys = new Map<string, SigningKey>();
  const codes = new Map<string, PendingGrant>();
  const once = new Map<string, IssuerReply[]>();
  const always = new Map<string, IssuerReply>();
  const holds = new Map<string, { promise: Promise<void>; release: () => void }>();
  const aliases = new Map<string, string>();
  let server: ReturnType<typeof Bun.serve> | undefined;
  let port = 0;

  const addKey = (kid: string, publish = true): SigningKey => {
    const { publicKey, privateKey } = crypto.generateKeyPairSync("ed25519");
    const key: SigningKey = {
      kid,
      privateKey,
      jwk: { ...publicKey.export({ format: "jwk" }), kid, alg: "EdDSA", use: "sig" },
      published: publish,
    };
    keys.set(kid, key);
    return key;
  };
  addKey("k1");

  const nowS = () => Math.floor(Date.now() / 1000);
  const prune = (object: Record<string, unknown>) =>
    Object.fromEntries(Object.entries(object).filter(([, value]) => value !== undefined));

  const sign = (header: Record<string, unknown>, payload: Record<string, unknown>, mint: MintOptions = {}) => {
    const kid = mint.kid ?? "k1";
    const key = keys.get(kid) ?? addKey(kid, false);
    const h = b64url(JSON.stringify(prune({ ...header, kid, ...mint.header })));
    const p = b64url(JSON.stringify(prune(payload)));
    const signature = crypto.sign(null, Buffer.from(`${h}.${p}`), key.privateKey);
    if (mint.tamper) signature[0] = signature[0]! ^ 0x01;
    return `${h}.${p}.${signature.toString("base64url")}`;
  };

  const accessClaims = (who: Identity) => ({
    iss: fake.issuer,
    sub: who.sub,
    aud: fake.audience,
    iat: nowS(),
    exp: nowS() + 600,
    sid: who.sid,
    username: who.username,
    name: who.name,
    role: who.role,
  });

  const idClaims = (who: Identity, nonce: string | null) => ({
    iss: fake.issuer,
    sub: who.sub,
    aud: clientId,
    iat: nowS(),
    exp: nowS() + 600,
    nonce: nonce ?? undefined,
    auth_time: nowS(),
    amr: ["pwd"],
    email: who.email,
    email_verified: who.emailVerified,
  });

  const authenticated = (call: IssuerCall) => {
    const expected = `Basic ${Buffer.from(`${formEncode(clientId)}:${formEncode(clientSecret)}`).toString("base64")}`;
    return call.headers.get("authorization") === expected;
  };

  const challengeOf = (verifier: string) => crypto.createHash("sha256").update(verifier).digest("base64url");

  const tokenEndpoint: IssuerReply = (call) => {
    if (!authenticated(call)) return jsonReply(401, { error: "invalid_client" });
    const { form } = call;
    const grantType = form.get("grant_type");
    if (grantType === "authorization_code") {
      const code = form.get("code") ?? "";
      const grant = codes.get(code);
      // A code is spent by its first use, whatever comes of it.
      codes.delete(code);
      if (
        !grant ||
        form.get("redirect_uri") !== grant.redirectUri ||
        challengeOf(form.get("code_verifier") ?? "") !== grant.challenge
      ) {
        return jsonReply(400, { error: "invalid_grant" });
      }
      const { who, options: grantOptions } = grant;
      const answer: Record<string, unknown> = {
        access_token: fake.accessToken(who, grantOptions.access, grantOptions.accessMint),
        id_token: fake.idToken(who, { nonce: grant.nonce ?? undefined, ...grantOptions.id }, grantOptions.idMint),
        token_type: "Bearer",
        expires_in: 600,
      };
      if (grantOptions.refreshToken !== null) {
        const refreshToken = grantOptions.refreshToken ?? `rt_${crypto.randomBytes(9).toString("base64url")}`;
        fake.refreshTokens.set(refreshToken, { who, revoked: false });
        answer.refresh_token = refreshToken;
      }
      return jsonReply(200, grantOptions.answer ? grantOptions.answer(answer) : answer);
    }
    if (grantType === "refresh_token") {
      const presented = form.get("refresh_token") ?? "";
      const held = fake.refreshTokens.get(presented);
      if (!held || held.revoked) return jsonReply(400, { error: "invalid_grant" });
      const answer: Record<string, unknown> = {
        access_token: fake.accessToken(held.who),
        token_type: "Bearer",
        expires_in: 600,
      };
      if (fake.rotateRefreshTokens) {
        held.revoked = true;
        const next = `rt_${crypto.randomBytes(9).toString("base64url")}`;
        fake.refreshTokens.set(next, { who: held.who, revoked: false });
        answer.refresh_token = next;
      }
      return jsonReply(200, answer);
    }
    return jsonReply(400, { error: "unsupported_grant_type" });
  };

  const introspectionEndpoint: IssuerReply = (call) => {
    if (!authenticated(call)) return jsonReply(401, { error: "invalid_client" });
    const known = fake.appPasswords.get(call.form.get("token") ?? "");
    if (!known || !known.active) return jsonReply(200, { active: false });
    return jsonReply(200, {
      active: true,
      sub: known.owner.sub,
      username: known.owner.username,
      role: known.owner.role,
      scope: known.scope,
      jti: known.id,
    });
  };

  const revocationEndpoint: IssuerReply = (call) => {
    if (!authenticated(call)) return jsonReply(401, { error: "invalid_client" });
    const held = fake.refreshTokens.get(call.form.get("token") ?? "");
    if (held) held.revoked = true;
    return new Response(null, { status: 200 });
  };

  const defaults: Record<string, IssuerReply> = {
    "/.well-known/openid-configuration": () => jsonReply(200, fake.discovery()),
    "/.well-known/jwks.json": () =>
      jsonReply(200, { keys: [...keys.values()].filter((key) => key.published).map((key) => key.jwk) }),
    "/oauth/token": tokenEndpoint,
    "/oauth/introspect": introspectionEndpoint,
    "/oauth/revoke": revocationEndpoint,
    "/logout": () => new Response("Signed out", { status: 200 }),
  };

  const handle = async (req: Request): Promise<Response> => {
    const url = new URL(req.url);
    const body = req.method === "GET" || req.method === "HEAD" ? "" : await req.text();
    const path = aliases.get(url.pathname) ?? url.pathname;
    const call: IssuerCall = {
      method: req.method,
      path,
      query: url.searchParams,
      headers: req.headers,
      body,
      form: new URLSearchParams(body),
    };
    fake.calls.push(call);
    await holds.get(path)?.promise;
    const reply = once.get(path)?.shift() ?? always.get(path) ?? always.get("*") ?? defaults[path];
    return reply ? reply(call) : jsonReply(404, { error: "not_found" });
  };

  const listen = () => {
    server = Bun.serve({ port, hostname: "127.0.0.1", fetch: handle });
    port = server.port!;
  };

  const fake = {
    /** What tokens carry in `iss`, and the discovery document names. */
    issuer: ISSUER,
    /** Where the fake listens: the back channel. */
    url: "",
    clientId,
    clientSecret,
    /** The `aud` of access tokens and events. */
    audience: options.audience ?? clientId,
    /** Every request received, oldest first. */
    calls: [] as IssuerCall[],
    /** Refresh tokens issued so far. A revoked or unknown one is refused with `invalid_grant`. */
    refreshTokens: new Map<string, { who: Identity; revoked: boolean }>(),
    /** When true, each refresh revokes the presented token and answers a new one. */
    rotateRefreshTokens: false,
    /** App passwords the introspection endpoint vouches for, by secret. */
    appPasswords: new Map<string, AppPassword>(),
    /** Fields to add to or replace in the discovery document; `undefined` removes one. */
    discoveryOverrides: {} as Record<string, unknown>,

    /** The settings that point a server at this provider. */
    env(overrides: Record<string, string | undefined> = {}): Record<string, string | undefined> {
      return {
        AUTH_ISSUER: fake.issuer,
        ...(fake.issuer === fake.url ? {} : { AUTH_BACKCHANNEL_URL: fake.url }),
        AUTH_CLIENT_ID: clientId,
        AUTH_CLIENT_SECRET: clientSecret,
        ...(fake.audience === clientId ? {} : { AUTH_AUDIENCE: fake.audience }),
        ...overrides,
      };
    },

    discovery(): Record<string, unknown> {
      const at = (path: string) => `${fake.issuer.replace(/\/+$/, "")}${path}`;
      return prune({
        issuer: fake.issuer,
        authorization_endpoint: at("/oauth/authorize"),
        token_endpoint: at("/oauth/token"),
        revocation_endpoint: at("/oauth/revoke"),
        introspection_endpoint: at("/oauth/introspect"),
        jwks_uri: at("/.well-known/jwks.json"),
        end_session_endpoint: at("/logout"),
        ...fake.discoveryOverrides,
      });
    },

    /** Generates a signing key; `publish: false` keeps it out of the key set. */
    addKey: (kid: string, publish = true) => void addKey(kid, publish),
    /** Publishes or withdraws a key in the key set. */
    publishKey(kid: string, published: boolean) {
      const key = keys.get(kid);
      if (!key) throw new Error(`The fake issuer has no key "${kid}"`);
      key.published = published;
    },

    /** Signs any header and payload as a compact JWT. */
    sign,

    /** An access token for `who`. */
    accessToken: (who: Identity, claims: Record<string, unknown> = {}, mint: MintOptions = {}) =>
      sign({ alg: "EdDSA", typ: "at+jwt" }, { ...accessClaims(who), ...claims }, mint),

    /** An ID token for `who`; pass the `nonce` of the sign-in in `claims`. */
    idToken: (who: Identity, claims: Record<string, unknown> = {}, mint: MintOptions = {}) =>
      sign({ alg: "EdDSA", typ: "JWT" }, { ...idClaims(who, null), ...claims }, mint),

    /**
     * A security event token carrying `events`, such as
     * `{ "user.disabled": { sub: "usr_alice" } }`.
     */
    eventToken: (events: unknown, claims: Record<string, unknown> = {}, mint: MintOptions = {}) =>
      sign(
        { alg: "EdDSA", typ: "secevent+jwt" },
        {
          iss: fake.issuer,
          aud: fake.audience,
          iat: nowS(),
          jti: `evt_${crypto.randomBytes(6).toString("hex")}`,
          events,
          ...claims,
        },
        mint
      ),

    /**
     * Plays the provider's sign-in page: approves the authorization request
     * the server redirected to and returns the code the browser would carry
     * back. The token endpoint later exchanges it, checking the PKCE verifier
     * and the redirect URI, for tokens carrying that request's nonce.
     */
    authorize(authorizeUrl: string | URL, who: Identity, grant: GrantOptions = {}): string {
      const query = new URL(authorizeUrl).searchParams;
      const code = `code_${crypto.randomBytes(9).toString("base64url")}`;
      codes.set(code, {
        who,
        nonce: query.get("nonce"),
        challenge: query.get("code_challenge"),
        redirectUri: query.get("redirect_uri"),
        options: grant,
      });
      return code;
    },

    /** Registers an app password for `owner` and returns its secret. */
    appPassword(owner: Identity, overrides: Partial<Omit<AppPassword, "owner">> = {}): AppPassword {
      const known: AppPassword = {
        secret: `apw-${crypto.randomBytes(12).toString("base64url")}`,
        owner,
        scope: "imap smtp",
        id: `apw_${crypto.randomBytes(4).toString("hex")}`,
        active: true,
        ...overrides,
      };
      fake.appPasswords.set(known.secret, known);
      return known;
    },

    /** Answers the next request to `path` only. Several queue up in order. */
    replyOnce(path: string, reply: IssuerReply) {
      const queue = once.get(path) ?? [];
      queue.push(reply);
      once.set(path, queue);
    },
    /** Answers every request to `path` (`"*"` for any path) until `resetReplies`. */
    replyAlways(path: string, reply: IssuerReply) {
      always.set(path, reply);
    },
    /** Drops every scripted answer, so the default provider behaviour is back. */
    resetReplies() {
      once.clear();
      always.clear();
    },
    /** Serves `path` the way the default endpoint at `target` is served. */
    alias(path: string, target: string) {
      aliases.set(path, target);
    },

    /** The requests received at `path`. */
    seen: (path: string) => fake.calls.filter((call) => call.path === path),
    clearCalls() {
      fake.calls.length = 0;
    },

    /**
     * Stops listening, so every connection is refused: the provider is
     * unreachable. `up` listens again on the same port.
     */
    async down() {
      await server?.stop(true);
      server = undefined;
    },
    up() {
      if (!server) listen();
    },
    get isDown() {
      return server === undefined;
    },

    /**
     * Makes requests to `path` wait, unanswered, until the returned function
     * is called: the provider is slow.
     */
    hold(path: string): () => void {
      let release!: () => void;
      const promise = new Promise<void>((resolve) => {
        release = resolve;
      });
      holds.set(path, { promise, release });
      return () => {
        holds.delete(path);
        release();
      };
    },

    async stop() {
      for (const { release } of holds.values()) release();
      holds.clear();
      await server?.stop(true);
      server = undefined;
    },
  };

  listen();
  fake.url = `http://127.0.0.1:${port}`;
  if (options.issuer === "self") fake.issuer = fake.url;
  else if (options.issuer !== undefined) fake.issuer = options.issuer;
  return fake;
};

export type FakeIssuer = Awaited<ReturnType<typeof startIssuer>>;

export interface StartedSignIn {
  /** The answer to `GET /api/auth/login`. */
  started: ApiResult;
  /** Where the server sent the browser: the provider's authorization endpoint. */
  authorizeUrl: URL;
  /** The `state` parameter of that redirect. */
  state: string;
  /** The value of the login cookie the server set. */
  loginCookie: string;
}

/**
 * Starts a sign-in the way the sign-in page does: `GET /api/auth/login`, then
 * reads the redirect and the login cookie. Throws when the server did not
 * redirect to the provider.
 */
export const startSignIn = async (client: ApiClient, returnTo?: string): Promise<StartedSignIn> => {
  const query = returnTo === undefined ? "" : `?returnTo=${encodeURIComponent(returnTo)}`;
  const started = await client.get(`/api/auth/login${query}`);
  const location = started.headers.get("location");
  const loginCookie = client.cookies.get(LOGIN_COOKIE);
  let authorizeUrl: URL | undefined;
  try {
    authorizeUrl = new URL(location ?? "");
  } catch {
    authorizeUrl = undefined;
  }
  const state = authorizeUrl?.searchParams.get("state");
  if (started.status !== 302 || !authorizeUrl || !state || !loginCookie) {
    throw new Error(`The server did not start a sign-in: ${started.status} → ${location} ${started.text}`);
  }
  return { started, authorizeUrl, state, loginCookie };
};

export interface SignInOptions {
  who: Identity;
  /** The `returnTo` the sign-in page passes. */
  returnTo?: string;
  /** What the token endpoint answers for this sign-in. */
  grant?: GrantOptions;
}

export interface SignInResult extends StartedSignIn {
  code: string;
  /** The answer to `GET /api/auth/callback`. */
  done: ApiResult;
}

/**
 * Drives a whole sign-in for `who`: starts it, approves it at the fake
 * provider, and follows the provider's redirect back to the callback. Asserts
 * nothing about how the callback answered; read `done`.
 *
 * ```ts
 * const { done } = await signIn(client, issuer, { who: person("alice"), returnTo: "/mail/42" });
 * expect(done.headers.get("location")).toBe("/mail/42");
 * ```
 */
export const signIn = async (client: ApiClient, issuer: FakeIssuer, options: SignInOptions): Promise<SignInResult> => {
  const started = await startSignIn(client, options.returnTo);
  const code = issuer.authorize(started.authorizeUrl, options.who, options.grant);
  const done = await client.get(
    `/api/auth/callback?code=${encodeURIComponent(code)}&state=${encodeURIComponent(started.state)}`
  );
  return { ...started, code, done };
};

/** The `?sso_error=` code a failed sign-in landed on, or undefined for another location. */
export const ssoErrorOf = (result: ApiResult): string | undefined => {
  const location = result.headers.get("location") ?? "";
  return /^\/sign-in\?sso_error=([a-z_]+)$/.exec(location)?.[1];
};
