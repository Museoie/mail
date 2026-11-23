import net from "net";
import type { TestServer } from "./server";

export interface ClientOptions {
  /**
   * The client address to claim through `X-Forwarded-For`. Only a production
   * server believes it (from loopback); elsewhere every request is attributed
   * to 127.0.0.1 whatever this says. Defaults to a fresh address per client,
   * so one client's rate-limit buckets are its own.
   */
  ip?: string;
  /** Set false to send no forwarding headers at all. */
  forwarded?: boolean;
  /** Headers added to every request. */
  headers?: Record<string, string>;
}

export interface RequestOptions {
  /** Sent as an `application/json` body. */
  json?: unknown;
  /** Sent as is; set the content type in `headers`. */
  body?: BodyInit | null;
  headers?: Record<string, string>;
  /** Set false to leave the cookie jar out of this request. */
  sendCookies?: boolean;
}

/** One answer from the server, read to the end. */
export interface ApiResult<T = any> {
  status: number;
  headers: Headers;
  /** The raw body. */
  text: string;
  /** The body parsed as JSON, or undefined when it is not JSON. */
  body: T;
  /** Each `Set-Cookie` value the response carried. */
  setCookies: string[];
}

let nextAddress = 1;

/** A distinct address in the documentation ranges, for a client to claim. */
export const uniqueClientIp = (): string => {
  const n = nextAddress++;
  return `198.51.${100 + (Math.floor(n / 250) % 100)}.${(n % 250) + 1}`;
};

const parseSetCookie = (line: string) => {
  const [pair, ...attributes] = line.split(";");
  const eq = pair!.indexOf("=");
  const name = pair!.slice(0, eq).trim();
  const value = pair!.slice(eq + 1).trim();
  const attrs = new Map<string, string>();
  for (const attribute of attributes) {
    const [key, ...rest] = attribute.trim().split("=");
    attrs.set(key!.toLowerCase(), rest.join("="));
  }
  return { name, value, attrs };
};

/**
 * An HTTP client for the server under test that keeps cookies between
 * requests the way a browser would, and never follows a redirect.
 *
 * ```ts
 * const client = server.client();
 * await client.login("admin", ADMIN_PASSWORD);
 * const { status, body } = await client.get("/api/mails/accounts");
 * ```
 */
export class ApiClient {
  readonly cookies = new Map<string, string>();
  readonly ip: string;
  private readonly forwarded: boolean;
  private readonly defaults: Record<string, string>;

  constructor(
    readonly server: TestServer,
    options: ClientOptions = {}
  ) {
    this.ip = options.ip ?? uniqueClientIp();
    this.forwarded = options.forwarded ?? true;
    this.defaults = options.headers ?? {};
  }

  /** The `Cookie` header this client would send now, or undefined with an empty jar. */
  cookieHeader(): string | undefined {
    if (!this.cookies.size) return undefined;
    return [...this.cookies].map(([name, value]) => `${name}=${value}`).join("; ");
  }

  private store(setCookies: string[]) {
    for (const line of setCookies) {
      const { name, value, attrs } = parseSetCookie(line);
      const expires = attrs.get("expires");
      const expired =
        attrs.get("max-age") === "0" || (expires !== undefined && Date.parse(expires) <= Date.now());
      if (expired || value === "") this.cookies.delete(name);
      else this.cookies.set(name, value);
    }
  }

  async request<T = any>(method: string, path: string, options: RequestOptions = {}): Promise<ApiResult<T>> {
    const headers = new Headers(this.defaults);
    if (this.forwarded && this.server.production) {
      headers.set("X-Forwarded-For", this.ip);
      headers.set("X-Forwarded-Proto", "https");
    }
    const cookie = options.sendCookies === false ? undefined : this.cookieHeader();
    if (cookie) headers.set("Cookie", cookie);

    let body: BodyInit | null | undefined = options.body;
    if (options.json !== undefined) {
      body = JSON.stringify(options.json);
      headers.set("Content-Type", "application/json");
    }
    for (const [name, value] of Object.entries(options.headers ?? {})) headers.set(name, value);

    const response = await fetch(`${this.server.url}${path}`, {
      method,
      headers,
      body,
      redirect: "manual",
    });
    const text = await response.text();
    const setCookies = response.headers.getSetCookie();
    this.store(setCookies);

    let parsed: unknown;
    try {
      parsed = text === "" ? undefined : JSON.parse(text);
    } catch {
      parsed = undefined;
    }
    return { status: response.status, headers: response.headers, text, body: parsed as T, setCookies };
  }

  get<T = any>(path: string, options?: RequestOptions) {
    return this.request<T>("GET", path, options);
  }

  head<T = any>(path: string, options?: RequestOptions) {
    return this.request<T>("HEAD", path, options);
  }

  post<T = any>(path: string, json?: unknown, options?: RequestOptions) {
    return this.request<T>("POST", path, { ...options, json: json === undefined ? options?.json : json });
  }

  delete<T = any>(path: string, options?: RequestOptions) {
    return this.request<T>("DELETE", path, options);
  }

  /** Signs in with a password and fails loudly if the server refuses. */
  async login(username: string, password: string): Promise<ApiResult> {
    const result = await this.post("/api/users/login", { username, password });
    if (result.body?.status !== "success") {
      throw new Error(`Could not sign in as ${username}: ${result.status} ${result.text}`);
    }
    return result;
  }
}

export interface RawResponse {
  status: number;
  statusText: string;
  headers: Record<string, string>;
  body: string;
  /** Everything the server sent, undecoded. */
  raw: string;
}

/**
 * Sends `request` byte for byte over a fresh connection and reads until the
 * server closes it. For request lines `fetch` would normalize away: an
 * undecodable escape, a path with `..`, an unusual method.
 *
 * ```ts
 * await rawHttp(server.ports.http, "GET /api/mails/body/%E0%A4%A HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
 * ```
 */
export const rawHttp = (port: number, request: string | Uint8Array, timeoutMs = 10_000): Promise<RawResponse> =>
  new Promise((resolve, reject) => {
    const socket = net.createConnection({ port, host: "127.0.0.1" });
    const chunks: Buffer[] = [];
    const timer = setTimeout(() => {
      socket.destroy();
      finish();
    }, timeoutMs);
    const finish = () => {
      clearTimeout(timer);
      const raw = Buffer.concat(chunks).toString("latin1");
      const split = raw.indexOf("\r\n\r\n");
      const head = split === -1 ? raw : raw.slice(0, split);
      const [statusLine = "", ...headerLines] = head.split("\r\n");
      const match = /^HTTP\/\d\.\d (\d{3}) ?(.*)$/.exec(statusLine);
      const headers: Record<string, string> = {};
      for (const line of headerLines) {
        const colon = line.indexOf(":");
        if (colon > 0) headers[line.slice(0, colon).trim().toLowerCase()] = line.slice(colon + 1).trim();
      }
      let body = split === -1 ? "" : raw.slice(split + 4);
      if (headers["transfer-encoding"]?.toLowerCase() === "chunked") {
        let rest = body;
        body = "";
        for (;;) {
          const lineEnd = rest.indexOf("\r\n");
          if (lineEnd === -1) break;
          const size = parseInt(rest.slice(0, lineEnd), 16);
          if (!size) break;
          body += rest.slice(lineEnd + 2, lineEnd + 2 + size);
          rest = rest.slice(lineEnd + 2 + size + 2);
        }
      }
      resolve({
        status: match ? Number(match[1]) : 0,
        statusText: match?.[2] ?? "",
        headers,
        body: Buffer.from(body, "latin1").toString("utf8"),
        raw,
      });
    };
    socket.on("connect", () => socket.write(request));
    socket.on("data", (chunk) => chunks.push(Buffer.from(chunk)));
    socket.on("end", finish);
    socket.on("close", finish);
    socket.on("error", (error) => {
      clearTimeout(timer);
      if (chunks.length) finish();
      else reject(error);
    });
  });
