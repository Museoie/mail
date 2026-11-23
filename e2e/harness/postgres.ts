import { SQL } from "bun";
import crypto from "crypto";
import { POSTGRES } from "./config";

const connect = (database: string) =>
  new SQL({
    hostname: POSTGRES.host,
    port: POSTGRES.port,
    username: POSTGRES.user,
    password: POSTGRES.password,
    database,
    max: 2,
    prepare: false,
  });

/** A throwaway database, and a connection to it for seeding and assertions. */
export interface TestDatabase {
  name: string;
  /**
   * Runs one statement with `$1`-style parameters and returns its rows.
   *
   * ```ts
   * const [user] = await db.query("SELECT * FROM users WHERE username = $1", ["admin"]);
   * ```
   */
  query<R = Record<string, any>>(text: string, values?: unknown[]): Promise<R[]>;
  /** Closes the connection and drops the database. */
  drop(): Promise<void>;
}

const toParam = (value: unknown): unknown => {
  if (value === null || value === undefined) return null;
  if (value instanceof Date) return value.toISOString();
  if (value instanceof Uint8Array) return value;
  if (typeof value === "object") return JSON.stringify(value);
  return String(value);
};

/** Creates an empty database with a unique name. The server under test builds the schema. */
export const createTestDatabase = async (): Promise<TestDatabase> => {
  const name = `mail_e2e_${crypto.randomBytes(8).toString("hex")}`;
  const admin = connect("postgres");
  try {
    await admin.unsafe(`CREATE DATABASE ${name}`);
  } finally {
    await admin.close();
  }

  const sql = connect(name);
  return {
    name,
    query: async (text, values) => {
      const rows = values?.length
        ? await sql.unsafe(text, values.map(toParam) as never[])
        : await sql.unsafe(text);
      return Array.from(rows) as never;
    },
    drop: async () => {
      await sql.close();
      const dropper = connect("postgres");
      try {
        await dropper.unsafe(`DROP DATABASE IF EXISTS ${name} WITH (FORCE)`);
      } finally {
        await dropper.close();
      }
    },
  };
};
