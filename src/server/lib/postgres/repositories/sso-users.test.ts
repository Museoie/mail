/**
 * The single sign-on account statements against a real Postgres.
 *
 * Their point is what the database does with them — a case-insensitive
 * match, a link that only lands once, a conflict that inserts nothing — which
 * no fake pool can show. Opt-in, like the search-vector probe: set
 * `INBOX_TEST_PG_URL` to a scratch database to run them.
 *
 *   INBOX_TEST_PG_URL=postgres://localhost:5432/mail_test \
 *     bun test src/server/lib/postgres/repositories/sso-users.test.ts
 *
 * The probe builds its own `users` table inside a throwaway schema from the
 * model's DDL, and drops it, so it never touches `public.users`.
 */
import { afterAll, describe, expect, it } from "bun:test";
import { buildCreateTable } from "../database";
import { usersTable } from "../models";
import {
  buildBumpPasswordVersionByAuthSub,
  buildFindUnlinkedUsersByEmail,
  buildInsertSsoUser,
  buildLinkUserAuthSub,
} from "./users";

const PG_URL = process.env.INBOX_TEST_PG_URL;
const SCHEMA = "sso_users_probe";

if (PG_URL) describe("single sign-on account statements against a real Postgres", () => {
  const poolModule = ((globalThis as Record<string, unknown>).__REAL_POOL ??
    require("../pool")) as typeof import("../pool");
  const url = new URL(PG_URL);
  const pool = new poolModule.Pool({
    host: url.hostname,
    port: Number(url.port || 5432),
    user: decodeURIComponent(url.username),
    password: url.password ? decodeURIComponent(url.password) : undefined,
    database: url.pathname.slice(1),
    max: 2,
    idleTimeoutMillis: 30_000,
    connectionTimeoutMillis: 5_000,
    statementTimeoutMillis: 0,
    queryTimeoutMillis: 0,
  });

  const q = async (sql: string, params: unknown[] = []) => {
    const client = await pool.connect();
    try {
      await client.query(`SET search_path TO ${SCHEMA}`);
      return await client.query(sql, params as never);
    } finally {
      client.release();
    }
  };
  const run = ({ sql, values }: { sql: string; values: unknown[] }) => q(sql, values);

  const setup = (async () => {
    await pool.query(`DROP SCHEMA IF EXISTS ${SCHEMA} CASCADE`);
    await pool.query(`CREATE SCHEMA ${SCHEMA}`);
    await q(buildCreateTable(usersTable.name, usersTable.schema, usersTable.constraints));
  })();

  afterAll(async () => {
    await pool.query(`DROP SCHEMA IF EXISTS ${SCHEMA} CASCADE`);
    await pool.end();
  });

  const fresh = async () => {
    await setup;
    await q("DELETE FROM users");
  };

  const addUser = async (username: string, columns: Record<string, unknown> = {}) => {
    const row = { username, password: "$2b$10$x", ...columns };
    const names = Object.keys(row);
    const result = await q(
      `INSERT INTO users (${names.join(", ")}) VALUES (${names.map((_, i) => `$${i + 1}`).join(", ")}) RETURNING user_id`,
      Object.values(row)
    );
    return result.rows[0].user_id as string;
  };

  const usernames = (rows: { username: string }[]) => rows.map((row) => row.username).sort();

  it("finds unlinked signed-up accounts by address ignoring case", async () => {
    await fresh();
    await addUser("bob", { email: "Alice@Example.com" });
    await addUser("carol", { email: "alice@example.com", auth_sub: "usr_carol" });
    await addUser("user_1234abcd", { email: "alice@example.com", password: null });
    await addUser("dave", { email: "alice@example.com", is_deleted: true });
    await addUser("erin", { email: "erin@example.com" });

    const found = await run(buildFindUnlinkedUsersByEmail("ALICE@example.COM"));
    expect(usernames(found.rows)).toEqual(["bob"]);
  });

  it("links an account once, and a subject to one account", async () => {
    await fresh();
    const bob = await addUser("bob");
    const carol = await addUser("carol");

    const linked = await run(buildLinkUserAuthSub(bob, "usr_alice"));
    expect(linked.rows.map((row) => [row.username, row.auth_sub])).toEqual([["bob", "usr_alice"]]);
    expect((await run(buildLinkUserAuthSub(bob, "usr_other"))).rows).toEqual([]);
    await expect(run(buildLinkUserAuthSub(carol, "usr_alice"))).rejects.toMatchObject({
      code: "23505",
    });
    const rows = await q("SELECT username, auth_sub FROM users ORDER BY username");
    expect(rows.rows).toEqual([
      { username: "bob", auth_sub: "usr_alice" },
      { username: "carol", auth_sub: null },
    ]);
  });

  it("creates an account, and nothing when its username or subject is taken", async () => {
    await fresh();
    await addUser("bob", { auth_sub: "usr_bob" });
    const account = { email: "alice@example.com", passwordHash: "$2b$10$y" };

    const created = await run(buildInsertSsoUser({ ...account, username: "alice", sub: "usr_alice" }));
    expect(created.rows).toHaveLength(1);
    expect(created.rows[0]).toMatchObject({
      username: "alice",
      email: "alice@example.com",
      auth_sub: "usr_alice",
      password: "$2b$10$y",
      password_version: 1,
    });
    expect(
      (await run(buildInsertSsoUser({ ...account, username: "bob", sub: "usr_new" }))).rows
    ).toEqual([]);
    expect(
      (await run(buildInsertSsoUser({ ...account, username: "frank", sub: "usr_bob" }))).rows
    ).toEqual([]);
    expect((await q("SELECT COUNT(*)::int AS n FROM users")).rows[0].n).toBe(2);
  });

  it("moves the linked account's generation on, from the default for an old row", async () => {
    await fresh();
    await addUser("alice", { auth_sub: "usr_alice", password_version: null });
    await addUser("bob", { auth_sub: "usr_bob", password_version: 5 });

    const first = await run(buildBumpPasswordVersionByAuthSub("usr_alice"));
    expect(first.rows).toEqual([
      expect.objectContaining({ username: "alice", password_version: 2 }),
    ]);
    const second = await run(buildBumpPasswordVersionByAuthSub("usr_alice"));
    expect(second.rows[0].password_version).toBe(3);
    expect((await run(buildBumpPasswordVersionByAuthSub("usr_nobody"))).rows).toEqual([]);
    const bob = await q("SELECT password_version FROM users WHERE username = 'bob'");
    expect(bob.rows[0].password_version).toBe(5);
  });
});
