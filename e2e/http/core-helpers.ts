/**
 * Shared by the `e2e/http` files that cover the non-mail API: sessions,
 * users, push, health, client errors, Mailgun events, static files, the
 * request pipeline and boot.
 */
import crypto from "crypto";
import { SQL } from "bun";
import { expect } from "bun:test";
import {
  POSTGRES,
  startServer as startHarnessServer,
  waitFor,
  type ApiClient,
  type ApiResult,
  type ServerOptions,
  type TestServer,
} from "../harness";

const PORT_TAKEN = /EADDRINUSE|address already in use/i;

/**
 * `startServer` of the harness, started again when the server could not bind
 * one of its ports. The harness picks ports that are free at that moment and
 * hands them to a process that binds them a little later; now and then
 * something else (a stub, an outgoing connection of the test run itself) is
 * given the same number in between. That is no fault of the server under
 * test, so the boot is repeated — on fresh ports for a first start, on the
 * same ones for a restart.
 */
export const startServer = async (options: ServerOptions = {}): Promise<TestServer> => {
  let server: TestServer | undefined;
  for (let attempt = 1; server === undefined; attempt++) {
    try {
      server = await startHarnessServer(options);
    } catch (error) {
      if (attempt >= 4 || !PORT_TAKEN.test(String(error))) throw error;
    }
  }
  const restart = server.restart;
  server.restart = async (env) => {
    for (let attempt = 1; ; attempt++) {
      try {
        return await restart(env);
      } catch (error) {
        if (attempt >= 4 || !PORT_TAKEN.test(String(error))) throw error;
      }
    }
  };
  return server;
};

export const CONTENT_SECURITY_POLICY =
  "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; " +
  "img-src 'self' data: blob: https:; connect-src 'self'; frame-src 'self'; " +
  "font-src 'self' data:; object-src 'none'; base-uri 'self'; form-action 'self'";

export const HSTS = "max-age=31536000; includeSubDomains";

type HeaderSource = Headers | Record<string, string>;

const headerOf = (headers: HeaderSource, name: string): string | null =>
  headers instanceof Headers ? headers.get(name) : (headers[name] ?? null);

/** The four security headers every response carries, and HSTS in production only. */
export const securityHeadersOf = (headers: HeaderSource) => ({
  "content-security-policy": headerOf(headers, "content-security-policy"),
  "x-content-type-options": headerOf(headers, "x-content-type-options"),
  "x-frame-options": headerOf(headers, "x-frame-options"),
  "referrer-policy": headerOf(headers, "referrer-policy"),
  "strict-transport-security": headerOf(headers, "strict-transport-security"),
});

export const expectedSecurityHeaders = (production: boolean) => ({
  "content-security-policy": CONTENT_SECURITY_POLICY,
  "x-content-type-options": "nosniff",
  "x-frame-options": "DENY",
  "referrer-policy": "strict-origin-when-cross-origin",
  "strict-transport-security": production ? HSTS : null,
});

export const JSON_TYPE = "application/json; charset=utf-8";
export const TEXT_TYPE = "text/plain; charset=utf-8";

export const NOT_FOUND = { status: "failed", message: "Not found" };
export const NOT_LOGGED_IN = { status: "failed", message: "Request user is not logged in." };
export const INTERNAL_ERROR = { status: "error", message: "Internal server error" };
export const BAD_REQUEST = { status: "error", message: "Bad Request" };
export const INVALID_CREDENTIALS = { status: "failed", message: "Invalid credentials." };
export const INVALID_BODY = { status: "failed", message: "Invalid request body." };

/** The exact body `GET /api/users/login` answers without a session. */
export const ANONYMOUS_LOGIN_TEXT =
  '{"status":"success","body":{"app":{"version":"1"},"sso":{"enabled":false,"localLogin":true}},"message":"Not logged in."}';

/** `[label, value]` for every JSON type that is not a string. */
export const NON_STRINGS: [string, unknown][] = [
  ["a number", 42],
  ["zero", 0],
  ["true", true],
  ["false", false],
  ["null", null],
  ["an array", ["admin"]],
  ["an empty array", []],
  ["an object", { value: "admin" }],
];

/** The key a server signs its session cookie with. */
export const sessionSecret = (server: TestServer): string => server.env.SECRET ?? "secret";

/** `s:<id>.<signature>`, the value of a signed session cookie before URL encoding. */
export const signSessionId = (sessionId: string, secret: string): string =>
  `s:${sessionId}.${crypto.createHmac("sha256", secret).update(sessionId).digest("base64").replace(/=+$/, "")}`;

/** A `Cookie` header naming `sessionId`, signed as the server would sign it. */
export const sessionCookie = (sessionId: string, secret: string): string =>
  `connect.sid=${encodeURIComponent(signSessionId(sessionId, secret))}`;

export interface ParsedCookie {
  name: string;
  /** Still URL-encoded, as it travels. */
  value: string;
  /** Attribute names in arrival order, lower-cased. */
  attributeNames: string[];
  attributes: Record<string, string>;
}

export const parseSetCookie = (line: string): ParsedCookie => {
  const [pair, ...rest] = line.split("; ");
  const eq = pair!.indexOf("=");
  const attributes: Record<string, string> = {};
  const attributeNames: string[] = [];
  for (const attribute of rest) {
    const at = attribute.indexOf("=");
    const name = (at === -1 ? attribute : attribute.slice(0, at)).toLowerCase();
    attributeNames.push(name);
    attributes[name] = at === -1 ? "" : attribute.slice(at + 1);
  }
  return { name: pair!.slice(0, eq), value: pair!.slice(eq + 1), attributeNames, attributes };
};

/** The session id inside a `connect.sid` cookie value. */
export const sessionIdOfValue = (value: string): string => {
  const decoded = decodeURIComponent(value);
  expect(decoded.startsWith("s:")).toBe(true);
  return decoded.slice(2, decoded.lastIndexOf("."));
};

/** The session id a client's jar holds. */
export const sessionIdOf = (client: ApiClient): string => {
  const value = client.cookies.get("connect.sid");
  if (!value) throw new Error("The client holds no session cookie");
  return sessionIdOfValue(value);
};

/** The one `Set-Cookie` a response carried, parsed; fails when there is not exactly one. */
export const onlySetCookie = (result: ApiResult): ParsedCookie => {
  expect(result.setCookies).toHaveLength(1);
  return parseSetCookie(result.setCookies[0]!);
};

export interface SessionRow {
  session_id: string;
  session_user_id: string;
  session_username: string;
  session_email: string;
  session_is_read_only: boolean | null;
  session_authenticated_as: string | null;
  session_password_version: number | null;
  auth_sid: string | null;
  auth_refresh_token: string | null;
  auth_recheck_at: Date | null;
  cookie_original_max_age: string | null;
  cookie_max_age: string | null;
  cookie_signed: boolean | null;
  cookie_expires: Date | null;
  cookie_http_only: boolean | null;
  cookie_path: string | null;
  cookie_domain: string | null;
  cookie_secure: string | null;
  cookie_same_site: string | null;
  updated: Date;
}

export const sessionRow = async (server: TestServer, sessionId: string): Promise<SessionRow | undefined> => {
  const [row] = await server.db.query<SessionRow>("SELECT * FROM sessions WHERE session_id = $1", [sessionId]);
  return row;
};

export const sessionRows = (server: TestServer): Promise<SessionRow[]> =>
  server.db.query<SessionRow>("SELECT * FROM sessions ORDER BY updated, session_id");

export const sessionCount = async (server: TestServer): Promise<number> => {
  const [row] = await server.db.query<{ count: number }>("SELECT COUNT(*)::int AS count FROM sessions");
  return row!.count;
};

export interface UserRow {
  user_id: string;
  username: string;
  password: string | null;
  password_version: number | null;
  email: string | null;
  expiry: Date | null;
  token: string | null;
  updated: Date;
  is_deleted: boolean | null;
  imap_uid_validity: string | null;
  auth_sub: string | null;
}

export const userRow = async (server: TestServer, username: string): Promise<UserRow | undefined> => {
  const [row] = await server.db.query<UserRow>("SELECT * FROM users WHERE username = $1", [username]);
  return row;
};

export const userRowByEmail = async (server: TestServer, email: string): Promise<UserRow | undefined> => {
  const [row] = await server.db.query<UserRow>("SELECT * FROM users WHERE email = $1", [email]);
  return row;
};

const SEVEN_DAYS_MS = 7 * 24 * 60 * 60 * 1000;

/** Whether `at` is `ms` from now, give or take `slackMs`. */
export const isAbout = (at: Date | number | string, ms: number, slackMs = 15_000): boolean => {
  const delta = new Date(at).getTime() - (Date.now() + ms);
  return Math.abs(delta) <= slackMs;
};

export const isAboutSevenDaysAhead = (at: Date | number | string) => isAbout(at, SEVEN_DAYS_MS);

/** `Sat, 28 Nov 2026 06:46:34 GMT` */
export const HTTP_DATE = /^(Mon|Tue|Wed|Thu|Fri|Sat|Sun), \d{2} (Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) \d{4} \d{2}:\d{2}:\d{2} GMT$/;

export const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/** A session id: 24 random bytes as base64url. */
export const SESSION_ID = /^[A-Za-z0-9_-]{32}$/;

/**
 * Checks the `Set-Cookie` of a freshly issued or renewed session: its name,
 * its attributes in order, and an expiry about seven days ahead.
 */
export const expectSessionCookie = (cookie: ParsedCookie, { secure }: { secure: boolean }) => {
  expect(cookie.name).toBe("connect.sid");
  expect(cookie.attributeNames).toEqual(
    secure ? ["path", "expires", "httponly", "secure", "samesite"] : ["path", "expires", "httponly", "samesite"]
  );
  expect(cookie.attributes.path).toBe("/");
  expect(cookie.attributes.samesite).toBe("Strict");
  expect(cookie.attributes.expires).toMatch(HTTP_DATE);
  expect(isAboutSevenDaysAhead(cookie.attributes.expires!)).toBe(true);
  expect(sessionIdOfValue(cookie.value)).toMatch(SESSION_ID);
};

/** A raw HTTP/1.1 request with `Connection: close`, for `rawHttp`. */
export const rawRequest = (
  method: string,
  target: string,
  headers: Record<string, string> = {},
  body?: string | Uint8Array
): Uint8Array => {
  const bytes = body === undefined ? new Uint8Array() : typeof body === "string" ? new TextEncoder().encode(body) : body;
  const all: Record<string, string> = { Host: "localhost", Connection: "close", ...headers };
  if (body !== undefined && !Object.keys(all).some((name) => /^(content-length|transfer-encoding)$/i.test(name))) {
    all["Content-Length"] = String(bytes.byteLength);
  }
  const head = `${method} ${target} HTTP/1.1\r\n${Object.entries(all)
    .map(([name, value]) => `${name}: ${value}\r\n`)
    .join("")}\r\n`;
  const headBytes = new TextEncoder().encode(head);
  const out = new Uint8Array(headBytes.byteLength + bytes.byteLength);
  out.set(headBytes, 0);
  out.set(bytes, headBytes.byteLength);
  return out;
};

/** A Mailgun webhook signature block for `timestamp` (Unix seconds, as text). */
export const mailgunSignature = (
  key: string,
  timestamp: string = String(Math.floor(Date.now() / 1000)),
  token: string = crypto.randomBytes(25).toString("hex")
) => ({
  timestamp,
  token,
  signature: crypto.createHmac("sha256", key).update(timestamp + token).digest("hex"),
});

/**
 * Runs `during` while the server's database refuses every connection, then
 * lets connections in again and waits until the server reports itself healthy.
 * The database and its rows are untouched; only connecting to it fails.
 */
export const withDatabaseOffline = async <T>(server: TestServer, during: () => Promise<T>): Promise<T> => {
  const admin = new SQL({
    hostname: POSTGRES.host,
    port: POSTGRES.port,
    username: POSTGRES.user,
    password: POSTGRES.password,
    database: "postgres",
    max: 1,
    prepare: false,
  });
  try {
    await admin.unsafe(`ALTER DATABASE ${server.db.name} WITH ALLOW_CONNECTIONS false`);
    await admin.unsafe(
      `SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '${server.db.name}' AND pid <> pg_backend_pid()`
    );
    return await during();
  } finally {
    await admin.unsafe(`ALTER DATABASE ${server.db.name} WITH ALLOW_CONNECTIONS true`);
    await admin.close();
    await waitFor(async () => (await fetch(`${server.url}/api/health`)).status === 200, {
      what: "the server to reach its database again",
      timeoutMs: 15_000,
    });
    // The harness's own connection was terminated too; this makes it reconnect.
    await waitFor(
      () =>
        server.db.query("SELECT 1").then(
          () => true,
          () => false
        ),
      { what: "the test's own database connection" }
    );
  }
};

let eventSequence = 0;

/** A Mailgun event id no other test used. */
export const eventId = (label = "event") => `${label}-${++eventSequence}-${crypto.randomBytes(6).toString("hex")}`;
