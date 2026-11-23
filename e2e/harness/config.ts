import path from "path";

/** Which backend the suite drives: the Bun server or the Rust server. */
export type ServerImpl = "bun" | "rust";

export const REPO_ROOT = path.resolve(import.meta.dir, "..", "..");

const readImpl = (): ServerImpl => {
  const value = (process.env.MAIL_E2E_SERVER ?? "bun").toLowerCase();
  if (value !== "bun" && value !== "rust") {
    throw new Error(`MAIL_E2E_SERVER must be "bun" or "rust", not "${value}"`);
  }
  return value;
};

/** The backend under test, from `MAIL_E2E_SERVER` (default `bun`). */
export const IMPL: ServerImpl = readImpl();

/** The Rust server binary; `scripts/e2e.ts` builds it before a Rust run. */
export const RUST_BINARY =
  process.env.MAIL_E2E_RUST_BIN ??
  path.join(REPO_ROOT, "src", "server-rs", "target", "release", "mail-server");

/** The Bun server's entry point, run from source. */
export const BUN_ENTRY = path.join(REPO_ROOT, "src", "server", "start.ts");

/** The built web client both servers serve. */
export const CLIENT_DIR = path.join(REPO_ROOT, "build", "client");

/** The Postgres server the suite creates its throwaway databases on. */
export const POSTGRES = {
  host: process.env.MAIL_E2E_POSTGRES_HOST ?? "127.0.0.1",
  port: Number(process.env.MAIL_E2E_POSTGRES_PORT ?? 5432),
  user: process.env.MAIL_E2E_POSTGRES_USER ?? "postgres",
  password: process.env.MAIL_E2E_POSTGRES_PASSWORD ?? "postgres",
};

/** Set `MAIL_E2E_LOGS=1` to stream each server's output while the tests run. */
export const STREAM_LOGS = process.env.MAIL_E2E_LOGS === "1";

/** The mail domain every test server serves unless a test overrides it. */
export const DOMAIN = "mail.test";
