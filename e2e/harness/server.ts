import crypto from "crypto";
import fs from "fs";
import os from "os";
import path from "path";
import type { Subprocess } from "bun";

import {
  BUN_ENTRY,
  CLIENT_DIR,
  DOMAIN,
  IMPL,
  POSTGRES,
  REPO_ROOT,
  RUST_BINARY,
  STREAM_LOGS,
  type ServerImpl,
} from "./config";
import { ApiClient, type ClientOptions } from "./client";
import { freePorts, hasIpv6, sleep } from "./net";
import { createTestDatabase, type TestDatabase } from "./postgres";
import { startAlarmStub, startMailgunStub, type AlarmStub, type MailgunStub } from "./stubs";
import { generateCertificate, type TestCertificate } from "./tls";
import { seedAdmin } from "./fixtures";

export const ADMIN_PASSWORD = "admin-password-e2e";
export const MAILGUN_KEY = "test_key_not_real_do_not_use";

export interface ServerOptions {
  /** Extra environment for the server; `undefined` removes a harness default. */
  env?: Record<string, string | undefined>;
  /** Give the server a certificate, so its TLS and STARTTLS listeners open. */
  tls?: boolean;
  /**
   * Run with `NODE_ENV=production` and a private `SECRET`: secure cookies,
   * HSTS, and forwarding headers believed from loopback, which lets a test
   * name its own client address with `X-Forwarded-For`.
   */
  production?: boolean;
  /** How long to wait for every listener to come up. */
  startupTimeoutMs?: number;
  /**
   * Seed `admin` (password {@link ADMIN_PASSWORD}) owning the served zone once
   * the server is up, as an operator does after installing. Default true.
   */
  seedAdmin?: boolean;
}

export interface ServerPorts {
  http: number;
  smtp: number;
  smtps: number;
  submission: number;
  imap: number;
  imaps: number;
}

/** One running server under test, with everything a test needs to drive it. */
export interface TestServer {
  impl: ServerImpl;
  /** `http://127.0.0.1:<port>` */
  url: string;
  ports: ServerPorts;
  domain: string;
  production: boolean;
  /** The environment the server process was started with. */
  env: Record<string, string>;
  /** The server's working directory; `attachments/` is created under it. */
  cwd: string;
  db: TestDatabase;
  mailgun: MailgunStub;
  alarms: AlarmStub;
  certificate?: TestCertificate;
  /** A client with its own cookie jar. */
  client(options?: ClientOptions): ApiClient;
  /** Everything the server has written to stdout and stderr so far. */
  logs(): string;
  /** Stops the process and starts it again on the same database, ports and directory. */
  restart(env?: Record<string, string | undefined>): Promise<void>;
  /** Stops the process without dropping anything; `restart` brings it back. */
  kill(signal?: NodeJS.Signals): Promise<number>;
  /** Stops the server and removes its database, stubs and working directory. */
  stop(): Promise<void>;
}

const live = new Set<Subprocess>();
process.on("exit", () => {
  for (const child of live) child.kill("SIGKILL");
});

const baseEnv = (): Record<string, string> => {
  const env: Record<string, string> = {};
  for (const name of ["PATH", "HOME", "TMPDIR", "LANG", "TZ"]) {
    const value = process.env[name];
    if (value !== undefined) env[name] = value;
  }
  return env;
};

const command = async (): Promise<string[]> => {
  if (IMPL === "rust") {
    if (!fs.existsSync(RUST_BINARY)) {
      throw new Error(
        `The Rust server binary is missing at ${RUST_BINARY}. Build it with ` +
          "`bun run test:e2e:rust`, or point MAIL_E2E_RUST_BIN at one."
      );
    }
    return [RUST_BINARY];
  }
  const preload = (await hasIpv6())
    ? []
    : ["--preload", path.join(import.meta.dir, "ipv4-listen.preload.ts")];
  return [process.execPath, ...preload, BUN_ENTRY];
};

const pump = async (stream: ReadableStream<Uint8Array>, sink: string[], label: string) => {
  const decoder = new TextDecoder();
  for await (const chunk of stream as unknown as AsyncIterable<Uint8Array>) {
    const text = decoder.decode(chunk, { stream: true });
    sink.push(text);
    if (STREAM_LOGS) process.stderr.write(text.replace(/^/gm, `[${label}] `));
  }
};

interface Launch {
  env: Record<string, string>;
  cwd: string;
  url: string;
  startupTimeoutMs: number;
  output: string[];
  /** Give up as soon as the server reports a port it could not bind. */
  failFastOnPortClash?: boolean;
}

const isPortClash = (logs: string) => /EADDRINUSE|Address already in use|address in use|Failed to listen/i.test(logs);

const launch = async ({
  env,
  cwd,
  url,
  startupTimeoutMs,
  output,
  failFastOnPortClash,
}: Launch): Promise<Subprocess> => {
  const from = output.length;
  const child = Bun.spawn(await command(), { cwd, env, stdout: "pipe", stderr: "pipe", stdin: "ignore" });
  live.add(child);
  void pump(child.stdout as ReadableStream<Uint8Array>, output, IMPL);
  void pump(child.stderr as ReadableStream<Uint8Array>, output, IMPL);
  void child.exited.then(() => live.delete(child));

  const deadline = Date.now() + startupTimeoutMs;
  for (;;) {
    if (child.exitCode !== null) {
      throw new Error(`The ${IMPL} server exited with ${child.exitCode} during startup:\n${output.join("")}`);
    }
    try {
      // /api/health is 200 only once the database answers and every mail
      // listener the configuration calls for accepts a connection.
      const response = await fetch(`${url}/api/health`, { signal: AbortSignal.timeout(5000) });
      await response.arrayBuffer();
      if (response.status === 200) return child;
    } catch {
      // Not listening yet.
    }
    if (failFastOnPortClash && isPortClash(output.slice(from).join(""))) {
      child.kill("SIGKILL");
      await child.exited;
      throw new Error(`The ${IMPL} server could not bind one of its ports:\n${output.slice(from).join("")}`);
    }
    if (Date.now() > deadline) {
      child.kill("SIGKILL");
      throw new Error(`The ${IMPL} server was not healthy after ${startupTimeoutMs}ms:\n${output.join("")}`);
    }
    await sleep(50);
  }
};

const terminate = async (child: Subprocess, signal: NodeJS.Signals = "SIGTERM"): Promise<number> => {
  if (child.exitCode !== null) return child.exitCode;
  child.kill(signal);
  const exited = await Promise.race([child.exited, sleep(15_000).then(() => undefined)]);
  if (exited === undefined) {
    child.kill("SIGKILL");
    return child.exited;
  }
  return exited;
};

const buildEnv = (
  options: ServerOptions,
  parts: {
    ports: ServerPorts;
    db: TestDatabase;
    mailgun: MailgunStub;
    alarms: AlarmStub;
    certificate?: TestCertificate;
  }
): Record<string, string> => {
  const { ports, db, mailgun, alarms, certificate } = parts;
  const env: Record<string, string | undefined> = {
    ...baseEnv(),
    // INTERNALDATE, SEARCH dates and log stamps follow the server's zone.
    TZ: "UTC",
    EMAIL_DOMAIN: DOMAIN,
    APP_HOSTNAME: `app.${DOMAIN}`,
    // Read only by a server that still seeds `admin` itself; the harness
    // seeds it either way (see `seedAdmin`), with the same password.
    ADMIN_PASSWORD,
    POSTGRES_HOST: POSTGRES.host,
    POSTGRES_PORT: String(POSTGRES.port),
    POSTGRES_USER: POSTGRES.user,
    POSTGRES_PASSWORD: POSTGRES.password,
    POSTGRES_DATABASE: db.name,
    PORT: String(ports.http),
    SMTP_PORT: String(ports.smtp),
    SMTPS_PORT: String(ports.smtps),
    SMTP_SUBMISSION_PORT: String(ports.submission),
    IMAP_PORT: String(ports.imap),
    IMAP_TLS_PORT: String(ports.imaps),
    MAILGUN_KEY,
    MAILGUN_URL: mailgun.url,
    DISCORD_ALARM_WEBHOOK: alarms.url,
    // The Rust server reads where the built client lives; the Bun server
    // resolves the same directory from its own source location.
    CLIENT_DIR,
  };
  if (certificate) {
    env.SSL_CERTIFICATE = certificate.certPath;
    env.SSL_CERTIFICATE_KEY = certificate.keyPath;
  }
  if (options.production) {
    env.NODE_ENV = "production";
    env.SECRET = crypto.randomBytes(36).toString("base64");
  }
  Object.assign(env, options.env ?? {});
  return Object.fromEntries(
    Object.entries(env).filter((entry): entry is [string, string] => entry[1] !== undefined)
  );
};

/**
 * Boots the server under test (Bun or Rust, per `MAIL_E2E_SERVER`) on its own
 * empty database, free ports and working directory, with Mailgun and the
 * alarm webhook stubbed. Resolves once `/api/health` reports every listener up.
 *
 * ```ts
 * let server: TestServer;
 * beforeAll(async () => { server = await startServer(); }, 60_000);
 * afterAll(() => server.stop());
 * ```
 */
export const startServer = async (options: ServerOptions = {}): Promise<TestServer> => {
  const cwd = await fs.promises.mkdtemp(path.join(os.tmpdir(), "mail-e2e-"));
  const db = await createTestDatabase();
  const mailgun = startMailgunStub();
  const alarms = startAlarmStub();
  const certificate = options.tls ? await generateCertificate(cwd) : undefined;
  const startupTimeoutMs = options.startupTimeoutMs ?? 60_000;
  const output: string[] = [];

  const cleanup = async () => {
    mailgun.stop();
    alarms.stop();
    await db.drop().catch(() => undefined);
    await fs.promises.rm(cwd, { recursive: true, force: true }).catch(() => undefined);
  };

  // A port is only known to be free at the moment it was probed: another
  // process can take it before the server binds. Such a start is retried on
  // fresh ports rather than failed.
  let ports!: ServerPorts;
  let env!: Record<string, string>;
  let url!: string;
  let child!: Subprocess;
  for (let attempt = 1; ; attempt++) {
    const [http, smtp, smtps, submission, imap, imaps] = await freePorts(6);
    ports = { http, smtp, smtps, submission, imap, imaps };
    env = buildEnv(options, { ports, db, mailgun, alarms, certificate });
    url = `http://127.0.0.1:${ports.http}`;
    const from = output.length;
    try {
      child = await launch({ env, cwd, url, startupTimeoutMs, output, failFastOnPortClash: true });
      break;
    } catch (error) {
      const clash = isPortClash(output.slice(from).join(""));
      if (clash && attempt < 4) continue;
      await cleanup();
      throw error;
    }
  }

  const server: TestServer = {
    impl: IMPL,
    url,
    ports,
    domain: env.EMAIL_DOMAIN ?? "mydomain",
    production: env.NODE_ENV === "production",
    env,
    cwd,
    db,
    mailgun,
    alarms,
    certificate,
    client: (clientOptions) => new ApiClient(server, clientOptions),
    logs: () => output.join(""),
    kill: (signal) => terminate(child, signal),
    restart: async (overrides = {}) => {
      await terminate(child);
      const next: Record<string, string | undefined> = { ...env, ...overrides };
      env = Object.fromEntries(
        Object.entries(next).filter((entry): entry is [string, string] => entry[1] !== undefined)
      );
      server.env = env;
      server.production = env.NODE_ENV === "production";
      child = await launch({ env, cwd, url, startupTimeoutMs, output });
    },
    stop: async () => {
      await terminate(child);
      await cleanup();
    },
  };
  if (options.seedAdmin !== false) {
    // A failed seed must not leave the process running past the test file.
    await seedAdmin(server, ADMIN_PASSWORD).catch(async (error) => {
      await server.stop();
      throw error;
    });
  }
  return server;
};

export interface BootFailure {
  exitCode: number | null;
  logs: string;
}

/**
 * Starts the server with a configuration it is expected to refuse, and
 * reports how the process ended. Resolves with `exitCode: null` if it is
 * still running after `waitMs`, in which case it is killed.
 */
export const bootExpectingExit = async (
  options: ServerOptions = {},
  waitMs = 20_000
): Promise<BootFailure> => {
  const [http, smtp, smtps, submission, imap, imaps] = await freePorts(6);
  const ports: ServerPorts = { http, smtp, smtps, submission, imap, imaps };
  const cwd = await fs.promises.mkdtemp(path.join(os.tmpdir(), "mail-e2e-"));
  const db = await createTestDatabase();
  const mailgun = startMailgunStub();
  const alarms = startAlarmStub();
  const env = buildEnv(options, { ports, db, mailgun, alarms });
  const output: string[] = [];
  const child = Bun.spawn(await command(), { cwd, env, stdout: "pipe", stderr: "pipe", stdin: "ignore" });
  live.add(child);
  const pumps = [
    pump(child.stdout as ReadableStream<Uint8Array>, output, IMPL),
    pump(child.stderr as ReadableStream<Uint8Array>, output, IMPL),
  ];
  const exitCode = await Promise.race([child.exited, sleep(waitMs).then(() => null)]);
  if (exitCode === null) child.kill("SIGKILL");
  await child.exited;
  live.delete(child);
  await Promise.all(pumps).catch(() => undefined);
  mailgun.stop();
  alarms.stop();
  await db.drop().catch(() => undefined);
  await fs.promises.rm(cwd, { recursive: true, force: true }).catch(() => undefined);
  return { exitCode, logs: output.join("") };
};

export { REPO_ROOT };
