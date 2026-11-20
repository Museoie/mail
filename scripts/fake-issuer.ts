/**
 * A stand-in OpenID Connect provider for the single sign-on tests, after the
 * one the provider client's own tests use: discovery, an Ed25519 key set, and
 * token, introspection and revocation endpoints whose answers each test sets.
 *
 * The client is pointed at it with `issuer` as the issuer URL (what tokens
 * carry in `iss`) and `backChannelUrl` as the back channel, so every call it
 * makes reaches this server. `fetch` throws while `down` is set, which the
 * client reads as an unreachable provider.
 */
import { createHash, generateKeyPairSync, sign } from "node:crypto";

export const FAKE_ISSUER = "https://auth.example.test";
export const FAKE_CLIENT_ID = "mail";
export const FAKE_CLIENT_SECRET = "test_key_not_real_do_not_use";

type Handler = (form: URLSearchParams, req: Request) => Response;

export interface FakeIssuerCall {
  path: string;
  headers: Headers;
  form: URLSearchParams;
}

export const jsonResponse = (status: number, body: unknown): Response =>
  new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });

export const startFakeIssuer = () => {
  const { publicKey, privateKey } = generateKeyPairSync("ed25519");
  const jwk = { ...publicKey.export({ format: "jwk" }), kid: "k1", alg: "EdDSA", use: "sig" };
  const nowS = () => Math.floor(fake.now() / 1000);

  const signJwt = (header: object, payload: object): string => {
    const h = Buffer.from(JSON.stringify(header)).toString("base64url");
    const p = Buffer.from(JSON.stringify(payload)).toString("base64url");
    return `${h}.${p}.${sign(null, Buffer.from(`${h}.${p}`), privateKey).toString("base64url")}`;
  };

  const fake = {
    issuer: FAKE_ISSUER,
    backChannelUrl: "",
    calls: [] as FakeIssuerCall[],
    down: false,
    now: () => Date.now(),
    token: (() => jsonResponse(500, {})) as Handler,
    introspect: (() => jsonResponse(200, { active: false })) as Handler,
    revoke: (() => new Response(null, { status: 200 })) as Handler,

    accessToken: (claims: object = {}): string =>
      signJwt(
        { alg: "EdDSA", typ: "at+jwt", kid: "k1" },
        {
          iss: FAKE_ISSUER,
          sub: "usr_alice",
          aud: FAKE_CLIENT_ID,
          iat: nowS(),
          exp: nowS() + 600,
          sid: "ses_alice",
          username: "alice",
          name: "Alice",
          role: "user",
          ...claims,
        }
      ),

    idToken: (claims: object = {}): string =>
      signJwt(
        { alg: "EdDSA", typ: "JWT", kid: "k1" },
        {
          iss: FAKE_ISSUER,
          sub: "usr_alice",
          aud: FAKE_CLIENT_ID,
          iat: nowS(),
          exp: nowS() + 600,
          nonce: "n",
          auth_time: nowS(),
          amr: ["pwd"],
          email: "alice@example.com",
          email_verified: true,
          ...claims,
        }
      ),

    eventToken: (events: object, claims: object = {}): string =>
      signJwt(
        { alg: "EdDSA", typ: "secevent+jwt", kid: "k1" },
        { iss: FAKE_ISSUER, aud: FAKE_CLIENT_ID, iat: nowS(), jti: "evt_1", events, ...claims }
      ),

    /**
     * A token endpoint that answers the code exchange as the provider would:
     * it checks the PKCE verifier against the authorize request, then issues
     * tokens carrying that request's nonce.
     */
    issueFor: (authorizeUrl: string, ids: object = {}, at: object = {}): Handler => {
      const query = new URL(authorizeUrl).searchParams;
      return (form) => {
        const challenge = createHash("sha256")
          .update(form.get("code_verifier") ?? "")
          .digest("base64url");
        if (form.get("code") !== "c0de" || challenge !== query.get("code_challenge")) {
          return jsonResponse(400, { error: "invalid_grant" });
        }
        return jsonResponse(200, {
          access_token: fake.accessToken(at),
          id_token: fake.idToken({ nonce: query.get("nonce"), ...ids }),
          refresh_token: "rt_alice",
          token_type: "Bearer",
          expires_in: 600,
        });
      };
    },

    seen: (path: string) => fake.calls.filter((call) => call.path === path),

    reset: () => {
      fake.calls = [];
      fake.down = false;
      fake.now = () => Date.now();
      fake.token = () => jsonResponse(200, { access_token: fake.accessToken(), token_type: "Bearer" });
      fake.introspect = () => jsonResponse(200, { active: false });
      fake.revoke = () => new Response(null, { status: 200 });
    },

    fetch: (async (input: string | URL | Request, init?: RequestInit) => {
      if (fake.down) throw new TypeError("fetch failed");
      return fetch(input, init);
    }) as typeof fetch,

    stop: () => server.stop(true),
  };

  const server = Bun.serve({
    port: 0,
    async fetch(req) {
      const path = new URL(req.url).pathname;
      const form = new URLSearchParams(req.method === "POST" ? await req.text() : "");
      fake.calls.push({ path, headers: req.headers, form });
      const at = (p: string) => `${FAKE_ISSUER}${p}`;
      switch (path) {
        case "/.well-known/openid-configuration":
          return jsonResponse(200, {
            issuer: FAKE_ISSUER,
            authorization_endpoint: at("/oauth/authorize"),
            token_endpoint: at("/oauth/token"),
            revocation_endpoint: at("/oauth/revoke"),
            introspection_endpoint: at("/oauth/introspect"),
            jwks_uri: at("/.well-known/jwks.json"),
            end_session_endpoint: at("/logout"),
          });
        case "/.well-known/jwks.json":
          return jsonResponse(200, { keys: [jwk] });
        case "/oauth/token":
          return fake.token(form, req);
        case "/oauth/introspect":
          return fake.introspect(form, req);
        case "/oauth/revoke":
          return fake.revoke(form, req);
      }
      return jsonResponse(404, { error: "not_found" });
    },
  });
  fake.backChannelUrl = `http://localhost:${server.port}`;
  fake.reset();
  return fake;
};

export type FakeIssuer = ReturnType<typeof startFakeIssuer>;

/** The environment that points single sign-on at `issuer`. */
export const fakeIssuerEnv = (issuer: FakeIssuer, overrides: Record<string, string> = {}) => ({
  AUTH_ISSUER: issuer.issuer,
  AUTH_BACKCHANNEL_URL: issuer.backChannelUrl,
  AUTH_CLIENT_ID: FAKE_CLIENT_ID,
  AUTH_CLIENT_SECRET: FAKE_CLIENT_SECRET,
  APP_HOSTNAME: "mail.example.test",
  ...overrides,
});
