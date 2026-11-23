import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import fs from "fs";
import os from "os";
import path from "path";
import { ADMIN_PASSWORD, generateCertificate, rawHttp, waitFor, type TestServer } from "../harness";
import { startServer } from "./core-helpers";
import {
  INTERNAL_ERROR,
  JSON_TYPE,
  NOT_FOUND,
  expectedSecurityHeaders,
  rawRequest,
  securityHeadersOf,
  withDatabaseOffline,
} from "./core-helpers";

type Checks = Record<string, "ok" | "unhealthy" | "not_configured">;

/** The check names in the order the body lists them. */
const checkNames = (server: TestServer) => [
  "database",
  "http",
  `smtp:${server.ports.smtp}`,
  `imap:${server.ports.imap}`,
  `smtp:${server.ports.smtps}`,
  `smtp:${server.ports.submission}`,
  `imap:${server.ports.imaps}`,
];

const expectHealthBody = (
  server: TestServer,
  text: string,
  expected: { healthy: boolean; values: Checks[string][] }
) => {
  const body = JSON.parse(text);
  expect(Object.keys(body)).toEqual(["status", "body"]);
  expect(body.status).toBe(expected.healthy ? "success" : "error");
  expect(Object.keys(body.body)).toEqual(["healthy", "checks", "timestamp"]);
  expect(body.body.healthy).toBe(expected.healthy);
  expect(Object.keys(body.body.checks)).toEqual(checkNames(server));
  expect(Object.values(body.body.checks)).toEqual(expected.values);
  expect(Number.isInteger(body.body.timestamp)).toBe(true);
  expect(Math.abs(body.body.timestamp - Date.now())).toBeLessThan(15_000);
};

const PLAIN: Checks[string][] = ["ok", "ok", "ok", "ok", "not_configured", "not_configured", "not_configured"];

describe("GET /api/health on a server without a certificate", () => {
  let server: TestServer;
  beforeAll(async () => {
    server = await startServer();
  }, 90_000);
  afterAll(async () => {
    await server?.stop();
  });

  test("reports the database, HTTP and the two plain mail ports, and the TLS ports as not configured", async () => {
    const { status, text, headers, setCookies } = await server.client().get("/api/health");
    expect(status).toBe(200);
    expectHealthBody(server, text, { healthy: true, values: PLAIN });
    expect(text).toMatch(
      new RegExp(
        `^\\{"status":"success","body":\\{"healthy":true,"checks":\\{"database":"ok","http":"ok",` +
          `"smtp:${server.ports.smtp}":"ok","imap:${server.ports.imap}":"ok",` +
          `"smtp:${server.ports.smtps}":"not_configured","smtp:${server.ports.submission}":"not_configured",` +
          `"imap:${server.ports.imaps}":"not_configured"\\},"timestamp":\\d{13}\\}\\}$`
      )
    );
    expect(headers.get("content-type")).toBe(JSON_TYPE);
    expect(headers.get("content-length")).toBe(String(text.length));
    expect(headers.get("etag")).toMatch(/^W\/"[0-9a-f]+-[A-Za-z0-9+/]{27}"$/);
    expect(securityHeadersOf(headers)).toEqual(expectedSecurityHeaders(false));
    expect(setCookies).toEqual([]);
  });

  test("answers HEAD with the same status and no body", async () => {
    const { status, text, headers } = await server.client().head("/api/health");
    expect(status).toBe(200);
    expect(text).toBe("");
    expect(headers.get("content-type")).toBe(JSON_TYPE);
    expect(Number(headers.get("content-length"))).toBeGreaterThan(200);
  });

  test("answers with a trailing slash, in any letter case and with a query string", async () => {
    for (const target of ["/api/health/", "/API/HEALTH", "/Api/Health/", "/api/health?verbose=1&x"]) {
      const { status, text } = await server.client().get(target);
      expect({ target, status }).toEqual({ target, status: 200 });
      expectHealthBody(server, text, { healthy: true, values: PLAIN });
    }
  });

  test("is not answered below the path or for other methods", async () => {
    for (const [method, target] of [
      ["GET", "/api/health/live"],
      ["GET", "/api/health//"],
      ["GET", "/api/healthz"],
      ["POST", "/api/health"],
      ["PUT", "/api/health"],
      ["DELETE", "/api/health"],
      ["PATCH", "/api/health"],
      ["OPTIONS", "/api/health"],
    ] as const) {
      const { status, body } = await server.client().request(method, target);
      expect({ method, target, status, body }).toEqual({ method, target, status: 404, body: NOT_FOUND });
    }
  });

  test("answers a signed-in caller the same", async () => {
    const client = server.client();
    await client.login("admin", ADMIN_PASSWORD);
    const { status, text } = await client.get("/api/health");
    expect(status).toBe(200);
    expectHealthBody(server, text, { healthy: true, values: PLAIN });
  });

  test("does not answer 304 to the validator of an earlier answer, whose timestamp has passed", async () => {
    const first = await server.client().get("/api/health");
    await waitFor(() => Date.now() > JSON.parse(first.text).body.timestamp, { what: "the clock to move" });
    const second = await server.client().get("/api/health", { headers: { "If-None-Match": first.headers.get("etag")! } });
    expect(second.status).toBe(200);
    expect(JSON.parse(second.text).body.timestamp).toBeGreaterThan(JSON.parse(first.text).body.timestamp);
  });

  test("answers over a bare connection", async () => {
    const response = await rawHttp(server.ports.http, rawRequest("GET", "/api/health"));
    expect(response.status).toBe(200);
    expectHealthBody(server, response.body, { healthy: true, values: PLAIN });
    expect(response.headers["content-length"]).toBe(String(response.body.length));
  });
});

describe("GET /api/health on a server with a certificate", () => {
  let server: TestServer;
  beforeAll(async () => {
    server = await startServer({ tls: true, startupTimeoutMs: 20_000 });
  }, 90_000);
  afterAll(async () => {
    await server?.stop();
  });

  test("probes the three TLS ports as well", async () => {
    const { status, text } = await server.client().get("/api/health");
    expect(status).toBe(200);
    expectHealthBody(server, text, { healthy: true, values: ["ok", "ok", "ok", "ok", "ok", "ok", "ok"] });
  });

  test("answers HEAD", async () => {
    const { status, text } = await server.client().head("/api/health");
    expect(status).toBe(200);
    expect(text).toBe("");
  });
});

describe("GET /api/health when something is down", () => {
  let server: TestServer;
  let certDir: string;
  beforeAll(async () => {
    // The certificate paths are configured but nothing is there at boot, so
    // the TLS listeners are never opened.
    certDir = await fs.promises.mkdtemp(path.join(os.tmpdir(), "mail-e2e-late-cert-"));
    server = await startServer({
      env: {
        SSL_CERTIFICATE: path.join(certDir, "tls-cert.pem"),
        SSL_CERTIFICATE_KEY: path.join(certDir, "tls-key.pem"),
      },
    });
  }, 90_000);
  afterAll(async () => {
    await server?.stop();
    if (certDir) await fs.promises.rm(certDir, { recursive: true, force: true });
  });

  test("a configured certificate that cannot be read counts as no certificate", async () => {
    const { status, text } = await server.client().get("/api/health");
    expect(status).toBe(200);
    expectHealthBody(server, text, { healthy: true, values: PLAIN });
  });

  test("answers 503 and names the dead ports once a certificate is readable but nothing listens for TLS", async () => {
    await generateCertificate(certDir);
    try {
      const { status, text, headers } = await server.client().get("/api/health");
      expect(status).toBe(503);
      expectHealthBody(server, text, {
        healthy: false,
        values: ["ok", "ok", "ok", "ok", "unhealthy", "unhealthy", "unhealthy"],
      });
      expect(headers.get("content-type")).toBe(JSON_TYPE);
      expect(securityHeadersOf(headers)).toEqual(expectedSecurityHeaders(false));

      const head = await server.client().head("/api/health");
      expect(head.status).toBe(503);
      expect(head.text).toBe("");

      // Only the key missing again: back to "no certificate".
      await fs.promises.rm(path.join(certDir, "tls-key.pem"));
      const keyless = await server.client().get("/api/health");
      expect(keyless.status).toBe(200);
      expectHealthBody(server, keyless.text, { healthy: true, values: PLAIN });
    } finally {
      await fs.promises.rm(path.join(certDir, "tls-cert.pem"), { force: true });
      await fs.promises.rm(path.join(certDir, "tls-key.pem"), { force: true });
    }
    const recovered = await server.client().get("/api/health");
    expect(recovered.status).toBe(200);
  });

  test("answers 503 while the database refuses connections, and 200 again afterwards", async () => {
    const client = server.client();
    await client.login("admin", ADMIN_PASSWORD);

    await withDatabaseOffline(server, async () => {
      const { status, text } = await server.client().get("/api/health");
      expect(status).toBe(503);
      expectHealthBody(server, text, {
        healthy: false,
        values: ["unhealthy", "ok", "ok", "ok", "not_configured", "not_configured", "not_configured"],
      });

      // A request that needs no session still works…
      expect((await server.client().get("/api/ping")).status).toBe(200);
      expect((await server.client().get("/")).status).toBe(200);
      // …one that carries a session cookie cannot load it, whatever it asks for…
      const api = await client.get("/api/ping");
      expect(api.status).toBe(500);
      expect(api.body).toEqual(INTERNAL_ERROR);
      expect(api.setCookies).toEqual([]);
      const page = await client.get("/");
      expect(page.status).toBe(500);
      expect(page.text).toBe("Internal server error");
      expect(page.headers.get("content-type")).toBe("text/plain; charset=utf-8");
      expect(securityHeadersOf(page.headers)).toEqual(expectedSecurityHeaders(false));
      // …and a route that needs the database fails as a route.
      const login = await server.client().post("/api/users/login", { username: "admin", password: ADMIN_PASSWORD });
      expect(login.status).toBe(500);
      expect(login.body).toEqual(INTERNAL_ERROR);
    });

    const after = await server.client().get("/api/health");
    expect(after.status).toBe(200);
    expectHealthBody(server, after.text, { healthy: true, values: PLAIN });
    // The session was never lost.
    expect((await client.get("/api/users/login")).body.body.user.username).toBe("admin");

    const titles = server.alarms.alarms.map((alarm) => alarm.title);
    expect(titles).toContain("🚨 Mail Server Error: Unhandled Middleware Error");
    expect(titles).toContain("🚨 Mail Server Error: Route Error: POST /login");
    const middleware = server.alarms.alarms.find((alarm) => alarm.title.endsWith("Unhandled Middleware Error"))!;
    expect(middleware.description).toBe(
      `**Request:** GET /api/ping\n**Error:** database "${server.db.name}" is not currently accepting connections`
    );
    const route = server.alarms.alarms.find((alarm) => alarm.title.endsWith("Route Error: POST /login"))!;
    expect(route.description).toBe(`**Error:** database "${server.db.name}" is not currently accepting connections`);
  });
});
