import { describe, expect, test } from "bun:test";
import fs from "fs";
import net from "net";
import path from "path";
import {
  ADMIN_PASSWORD,
  REPO_ROOT,
  bootExpectingExit,
  createUser,
  waitFor,
  type ApiClient,
  type ServerOptions,
  type TestServer,
} from "../harness";
import { startServer } from "./core-helpers";
import {
  ANONYMOUS_LOGIN_TEXT,
  INVALID_CREDENTIALS,
  UUID,
  sessionCount,
  sessionIdOf,
  sessionRow,
  signSessionId,
  userRow,
} from "./core-helpers";

/** What both servers must agree the schema is: generated from the Bun server's table definitions. */
const schema = JSON.parse(fs.readFileSync(path.join(REPO_ROOT, "src", "server-rs", "schema.json"), "utf8")) as {
  schemaHash: string;
  maintenanceHash: string;
  tables: { name: string; columns: [string, string][] }[];
  indexes: { name: string }[];
};

const BOOT_TIMEOUT = 120_000;

/** Boots a server for one test and stops it whatever happens. */
const withServer = async (options: ServerOptions, run: (server: TestServer) => Promise<void>) => {
  const server = await startServer(options);
  try {
    await run(server);
  } finally {
    await server.stop();
  }
};

const canLogin = async (server: TestServer, username: string, password: string) => {
  const { body } = await server.client().post("/api/users/login", { username, password });
  if (body.status !== "success") expect(body).toEqual(INVALID_CREDENTIALS);
  return body.status === "success";
};

const whoIs = async (client: ApiClient) => (await client.get("/api/users/login")).body.body.user?.username as string | undefined;

const markers = async (server: TestServer) => {
  const rows = await server.db.query<{ key: string; value: string }>("SELECT key, value FROM schema_meta ORDER BY key");
  return Object.fromEntries(rows.map((row) => [row.key, row.value]));
};

/**
 * The markers once the boot's background maintenance has written its own:
 * that one follows the listeners, so a server is healthy a moment before it.
 */
const bothMarkers = (server: TestServer) =>
  waitFor(
    async () => {
      const found = await markers(server);
      return found.maintenance_hash === schema.maintenanceHash ? found : undefined;
    },
    { what: "the maintenance marker of this boot", timeoutMs: 30_000 }
  );

describe("SECRET in production", () => {
  test.each([
    ["unset", undefined],
    ["empty", ""],
    ["only whitespace", "   "],
    ["the development fallback", "secret"],
    ["the value in .env.example's history", "inbox"],
    ["a published value with spaces around it", "  secret "],
    ["a published value with a line break after it", "inbox\n"],
  ])("the server refuses to boot when it is %s", async (_label, SECRET) => {
    const { exitCode } = await bootExpectingExit({ production: true, env: { SECRET } });
    expect(exitCode).toBe(1);
  }, BOOT_TIMEOUT);

  test.each([
    ["short", "x"],
    ["a published value in another letter case", "Secret"],
    ["a published value with something added", "secret1"],
    ["long", "a-private-value-of-a-good-length-0123456789"],
  ])("the server boots, and signs with it, when it is %s", async (_label, SECRET) => {
    await withServer({ production: true, env: { SECRET } }, async (server) => {
      const client = server.client();
      await client.login("admin", ADMIN_PASSWORD);
      expect(decodeURIComponent(client.cookies.get("connect.sid")!)).toBe(signSessionId(sessionIdOf(client), SECRET));
    });
  }, BOOT_TIMEOUT);

  test("outside production none of these stop the boot", async () => {
    for (const SECRET of [undefined, "secret", "inbox"]) {
      await withServer({ env: { SECRET } }, async (server) => {
        expect((await server.client().get("/api/ping")).status).toBe(200);
      });
    }
  }, BOOT_TIMEOUT);

  test("only NODE_ENV=production is the production posture", async () => {
    for (const NODE_ENV of ["Production", "prod", "development", "test", ""]) {
      await withServer({ env: { NODE_ENV, SECRET: undefined } }, async (server) => {
        const result = await server.client().get("/api/ping");
        expect({ NODE_ENV, status: result.status, hsts: result.headers.get("strict-transport-security") }).toEqual({
          NODE_ENV,
          status: 200,
          hsts: null,
        });
      });
    }
  }, BOOT_TIMEOUT);
});

describe("the admin account at boot", () => {
  test("is created on the first boot with ADMIN_PASSWORD", async () => {
    await withServer({}, async (server) => {
      const admin = (await userRow(server, "admin"))!;
      expect(admin.user_id).toMatch(UUID);
      expect(admin.password).toMatch(/^\$2b\$10\$[./A-Za-z0-9]{53}$/);
      expect(admin).toMatchObject({
        username: "admin",
        email: "admin@mail.test",
        password_version: 1,
        token: null,
        expiry: null,
        is_deleted: false,
        imap_uid_validity: null,
        auth_sub: null,
      });
      expect((await server.db.query("SELECT username FROM users")).map((row) => row.username)).toEqual(["admin"]);
      expect(await canLogin(server, "admin", ADMIN_PASSWORD)).toBe(true);
      expect(await canLogin(server, "admin", "inbox")).toBe(false);
    });
  }, BOOT_TIMEOUT);

  test("is created with the password 'inbox' when ADMIN_PASSWORD is unset or empty", async () => {
    for (const password of [undefined, ""]) {
      await withServer({ env: { ADMIN_PASSWORD: password } }, async (server) => {
        expect(await canLogin(server, "admin", "inbox")).toBe(true);
        expect(await canLogin(server, "admin", ADMIN_PASSWORD)).toBe(false);
      });
    }
  }, BOOT_TIMEOUT);

  test("only the first 72 bytes of ADMIN_PASSWORD count", async () => {
    const long = `${"p".repeat(72)}-and-more`;
    await withServer({ env: { ADMIN_PASSWORD: long } }, async (server) => {
      expect(await canLogin(server, "admin", long)).toBe(true);
      expect(await canLogin(server, "admin", "p".repeat(72))).toBe(true);
      expect(await canLogin(server, "admin", "p".repeat(71))).toBe(false);
    });
  }, BOOT_TIMEOUT);

  test("keeps its stored password on a later boot with another ADMIN_PASSWORD", async () => {
    await withServer({}, async (server) => {
      const before = (await userRow(server, "admin"))!;
      const client = server.client();
      await client.login("admin", ADMIN_PASSWORD);

      await server.restart({ ADMIN_PASSWORD: "a-different-password" });
      const after = (await userRow(server, "admin"))!;
      expect(after).toMatchObject({
        user_id: before.user_id,
        password: before.password,
        password_version: 1,
        email: "admin@mail.test",
      });
      expect(await canLogin(server, "admin", ADMIN_PASSWORD)).toBe(true);
      expect(await canLogin(server, "admin", "a-different-password")).toBe(false);
      // Nobody was signed out.
      expect(await whoIs(client)).toBe("admin");

      await server.restart({ ADMIN_PASSWORD: undefined });
      expect((await userRow(server, "admin"))!.password).toBe(before.password);
      expect(await canLogin(server, "admin", ADMIN_PASSWORD)).toBe(true);
      expect(await canLogin(server, "admin", "inbox")).toBe(false);
    });
  }, BOOT_TIMEOUT);

  test("takes ADMIN_PASSWORD again with ADMIN_PASSWORD_RESET=1, which signs admin out everywhere", async () => {
    await withServer({}, async (server) => {
      const before = (await userRow(server, "admin"))!;
      const alice = await createUser(server, "alice");
      const adminOne = server.client();
      const adminTwo = server.client();
      const aliceClient = server.client();
      await adminOne.login("admin", ADMIN_PASSWORD);
      await adminTwo.login("admin", ADMIN_PASSWORD);
      await aliceClient.login(alice.username, alice.password);
      const adminSessions = [sessionIdOf(adminOne), sessionIdOf(adminTwo)];

      await server.restart({ ADMIN_PASSWORD: "the-new-password", ADMIN_PASSWORD_RESET: "1" });
      const after = (await userRow(server, "admin"))!;
      expect(after.user_id).toBe(before.user_id);
      expect(after.password).not.toBe(before.password);
      expect(after.password).toMatch(/^\$2b\$10\$/);
      expect(after.password_version).toBe(2);
      expect(await canLogin(server, "admin", "the-new-password")).toBe(true);
      expect(await canLogin(server, "admin", ADMIN_PASSWORD)).toBe(false);

      for (const id of adminSessions) expect(await sessionRow(server, id)).toBeUndefined();
      expect(await whoIs(adminOne)).toBeUndefined();
      expect(await whoIs(aliceClient)).toBe("alice");

      // While the variable stays set, every boot applies it again.
      const again = server.client();
      await again.login("admin", "the-new-password");
      await server.restart();
      expect((await userRow(server, "admin"))!.password_version).toBe(3);
      expect(await whoIs(again)).toBeUndefined();
      expect(await canLogin(server, "admin", "the-new-password")).toBe(true);

      // Without it the stored password stays, and so do the sessions.
      const kept = server.client();
      await kept.login("admin", "the-new-password");
      await server.restart({ ADMIN_PASSWORD_RESET: undefined, ADMIN_PASSWORD: "ignored-now" });
      expect((await userRow(server, "admin"))!.password_version).toBe(3);
      expect(await whoIs(kept)).toBe("admin");
      expect(await canLogin(server, "admin", "the-new-password")).toBe(true);
    });
  }, BOOT_TIMEOUT);

  test("a session stamped before the reset is refused even if its row comes back", async () => {
    await withServer({}, async (server) => {
      const client = server.client();
      await client.login("admin", ADMIN_PASSWORD);
      const id = sessionIdOf(client);
      const [saved] = await server.db.query("SELECT * FROM sessions WHERE session_id = $1", [id]);

      await server.restart({ ADMIN_PASSWORD: "rotated", ADMIN_PASSWORD_RESET: "1" });
      expect(await sessionRow(server, id)).toBeUndefined();
      // The row restored as it was, generation 1 against the account's 2.
      const columns = Object.keys(saved!);
      await server.db.query(
        `INSERT INTO sessions (${columns.join(", ")}) VALUES (${columns.map((_, i) => `$${i + 1}`).join(", ")})`,
        columns.map((column) => saved![column])
      );
      expect((await client.get("/api/users/login")).text).toBe(ANONYMOUS_LOGIN_TEXT);
      expect(await sessionRow(server, id)).toBeUndefined();
    });
  }, BOOT_TIMEOUT);

  test("ADMIN_PASSWORD_RESET=1 without a password changes nothing", async () => {
    await withServer({}, async (server) => {
      const before = (await userRow(server, "admin"))!;
      const client = server.client();
      await client.login("admin", ADMIN_PASSWORD);
      for (const password of [undefined, ""]) {
        await server.restart({ ADMIN_PASSWORD: password, ADMIN_PASSWORD_RESET: "1" });
        expect(await userRow(server, "admin")).toMatchObject({ password: before.password, password_version: 1 });
        expect(await canLogin(server, "admin", ADMIN_PASSWORD)).toBe(true);
        expect(await canLogin(server, "admin", "inbox")).toBe(false);
        expect(await whoIs(client)).toBe("admin");
      }
    });
  }, BOOT_TIMEOUT);

  test("any other value of ADMIN_PASSWORD_RESET changes nothing", async () => {
    await withServer({}, async (server) => {
      const before = (await userRow(server, "admin"))!;
      const client = server.client();
      await client.login("admin", ADMIN_PASSWORD);
      for (const value of ["true", "yes", "0", "2", "01", "1 ", " 1", "one", ""]) {
        await server.restart({ ADMIN_PASSWORD: "would-be-new", ADMIN_PASSWORD_RESET: value });
        expect({ value, row: await userRow(server, "admin") }).toMatchObject({
          value,
          row: { password: before.password, password_version: 1 },
        });
        expect(await whoIs(client)).toBe("admin");
      }
      expect(await canLogin(server, "admin", ADMIN_PASSWORD)).toBe(true);
    });
  }, BOOT_TIMEOUT);

  test("ADMIN_PASSWORD_RESET=1 on the very first boot just creates the account", async () => {
    await withServer({ env: { ADMIN_PASSWORD_RESET: "1" } }, async (server) => {
      expect((await userRow(server, "admin"))!.password_version).toBe(1);
      expect(await canLogin(server, "admin", ADMIN_PASSWORD)).toBe(true);
    });
  }, BOOT_TIMEOUT);

  test("a reset of a row that predates credential generations moves it to 2", async () => {
    await withServer({}, async (server) => {
      await server.db.query("UPDATE users SET password_version = NULL WHERE username = 'admin'");
      await server.restart({ ADMIN_PASSWORD: "rotated", ADMIN_PASSWORD_RESET: "1" });
      expect((await userRow(server, "admin"))!.password_version).toBe(2);
    });
  }, BOOT_TIMEOUT);

  test("a password changed through the reset flow survives a restart", async () => {
    await withServer({}, async (server) => {
      const alice = await createUser(server, "alice");
      await server.db.query("UPDATE users SET token = 'boot-reset-token' WHERE user_id = $1", [alice.id]);
      const result = await server.client().post("/api/users/set-info", {
        email: alice.email,
        username: "alice",
        password: "set-by-the-user",
        token: "boot-reset-token",
      });
      expect(result.body.status).toBe("success");
      await server.restart();
      expect(await canLogin(server, "alice", "set-by-the-user")).toBe(true);
      expect((await userRow(server, "alice"))!.password_version).toBe(2);
    });
  }, BOOT_TIMEOUT);
});

describe("admin's address", () => {
  test("is admin@EMAIL_DOMAIN, and follows the domain on a later boot", async () => {
    await withServer({ env: { EMAIL_DOMAIN: "first.example" } }, async (server) => {
      expect((await userRow(server, "admin"))!.email).toBe("admin@first.example");
      const id = (await userRow(server, "admin"))!.user_id;
      await server.restart({ EMAIL_DOMAIN: "Second.Example" });
      expect(await userRow(server, "admin")).toMatchObject({ user_id: id, email: "admin@Second.Example", password_version: 1 });
      expect(await canLogin(server, "admin", ADMIN_PASSWORD)).toBe(true);
    });
  }, BOOT_TIMEOUT);

  test("is admin@localhost when EMAIL_DOMAIN is unset or empty", async () => {
    for (const EMAIL_DOMAIN of [undefined, ""]) {
      await withServer({ env: { EMAIL_DOMAIN, ADMIN_RO_PASSWORD: "read-only-password" } }, async (server) => {
        expect((await userRow(server, "admin"))!.email).toBe("admin@localhost");
        expect((await userRow(server, "admin-ro"))!.email).toBe("admin-ro@localhost");
        const { body } = await server.client().post("/api/users/login", { username: "admin", password: ADMIN_PASSWORD });
        expect(body.body.email).toBe("admin@localhost");
      });
    }
  }, BOOT_TIMEOUT);

  test("a signed-in admin keeps the address the session was issued with", async () => {
    await withServer({ env: { EMAIL_DOMAIN: "first.example" } }, async (server) => {
      const client = server.client();
      await client.login("admin", ADMIN_PASSWORD);
      await server.restart({ EMAIL_DOMAIN: "second.example" });
      expect((await client.get("/api/users/login")).body.body.user.email).toBe("admin@first.example");
      const fresh = await server.client().post("/api/users/login", { username: "admin", password: ADMIN_PASSWORD });
      expect(fresh.body.body.email).toBe("admin@second.example");
    });
  }, BOOT_TIMEOUT);
});

describe("the read-only account at boot", () => {
  const RO = "read-only-password-one";

  test("does not exist unless ADMIN_RO_PASSWORD is set", async () => {
    for (const ADMIN_RO_PASSWORD of [undefined, ""]) {
      await withServer({ env: { ADMIN_RO_PASSWORD } }, async (server) => {
        expect(await userRow(server, "admin-ro")).toBeUndefined();
        await server.restart();
        expect(await userRow(server, "admin-ro")).toBeUndefined();
      });
    }
  }, BOOT_TIMEOUT);

  test("is created with ADMIN_RO_PASSWORD", async () => {
    await withServer({ env: { ADMIN_RO_PASSWORD: RO } }, async (server) => {
      const row = (await userRow(server, "admin-ro"))!;
      expect(row.user_id).toMatch(UUID);
      expect(row.password).toMatch(/^\$2b\$10\$[./A-Za-z0-9]{53}$/);
      expect(row).toMatchObject({ email: "admin-ro@mail.test", password_version: 1, token: null, is_deleted: false });
      const { body } = await server.client().post("/api/users/login", { username: "admin-ro", password: RO });
      expect(body.body).toMatchObject({ username: "admin", isReadOnly: true, authenticatedAs: "admin-ro" });
    });
  }, BOOT_TIMEOUT);

  test("a restart with the same password signs nobody out", async () => {
    await withServer({ env: { ADMIN_RO_PASSWORD: RO } }, async (server) => {
      const before = (await userRow(server, "admin-ro"))!;
      const readOnly = server.client();
      await readOnly.login("admin-ro", RO);
      await server.restart();
      const after = (await userRow(server, "admin-ro"))!;
      expect(after).toMatchObject({ user_id: before.user_id, password_version: 1, email: "admin-ro@mail.test" });
      expect((await readOnly.get("/api/users/login")).body.body.user).toMatchObject({ isReadOnly: true, authenticatedAs: "admin-ro" });
      expect(await canLogin(server, "admin-ro", RO)).toBe(true);
    });
  }, BOOT_TIMEOUT);

  test("a changed password withdraws the sessions the old one issued, and only those", async () => {
    await withServer({ env: { ADMIN_RO_PASSWORD: RO } }, async (server) => {
      const before = (await userRow(server, "admin-ro"))!;
      const alice = await createUser(server, "alice");
      const readOnlyOne = server.client();
      const readOnlyTwo = server.client();
      const admin = server.client();
      const aliceClient = server.client();
      await readOnlyOne.login("admin-ro", RO);
      await readOnlyTwo.login("admin-ro", RO);
      await admin.login("admin", ADMIN_PASSWORD);
      await aliceClient.login(alice.username, alice.password);
      const readOnlySessions = [sessionIdOf(readOnlyOne), sessionIdOf(readOnlyTwo)];

      await server.restart({ ADMIN_RO_PASSWORD: "read-only-password-two" });
      const after = (await userRow(server, "admin-ro"))!;
      expect(after.user_id).toBe(before.user_id);
      expect(after.password).not.toBe(before.password);
      expect(after.password_version).toBe(2);
      for (const id of readOnlySessions) expect(await sessionRow(server, id)).toBeUndefined();
      expect(await whoIs(readOnlyOne)).toBeUndefined();
      // The read-only sessions were admin's identity; admin's own are untouched.
      expect(await whoIs(admin)).toBe("admin");
      expect((await userRow(server, "admin"))!.password_version).toBe(1);
      expect(await whoIs(aliceClient)).toBe("alice");

      expect(await canLogin(server, "admin-ro", "read-only-password-two")).toBe(true);
      expect(await canLogin(server, "admin-ro", RO)).toBe(false);
    });
  }, BOOT_TIMEOUT);

  test("removing ADMIN_RO_PASSWORD revokes the account and its sessions, and keeps the row", async () => {
    await withServer({ env: { ADMIN_RO_PASSWORD: RO } }, async (server) => {
      const before = (await userRow(server, "admin-ro"))!;
      const readOnly = server.client();
      const admin = server.client();
      await readOnly.login("admin-ro", RO);
      await admin.login("admin", ADMIN_PASSWORD);
      const readOnlySession = sessionIdOf(readOnly);

      await server.restart({ ADMIN_RO_PASSWORD: undefined });
      const revoked = (await userRow(server, "admin-ro"))!;
      expect(revoked).toMatchObject({ user_id: before.user_id, email: "admin-ro@mail.test", password_version: 2, is_deleted: false });
      expect(revoked.password).toMatch(/^\$2b\$10\$[./A-Za-z0-9]{53}$/);
      expect(revoked.password).not.toBe(before.password);
      expect(await canLogin(server, "admin-ro", RO)).toBe(false);
      expect(await sessionRow(server, readOnlySession)).toBeUndefined();
      expect(await whoIs(readOnly)).toBeUndefined();
      expect(await whoIs(admin)).toBe("admin");

      // Every boot without it revokes again.
      await server.restart({ ADMIN_RO_PASSWORD: "" });
      const again = (await userRow(server, "admin-ro"))!;
      expect(again.password_version).toBe(3);
      expect(again.password).not.toBe(revoked.password);

      // Setting it again brings the same account back.
      await server.restart({ ADMIN_RO_PASSWORD: RO });
      const restored = (await userRow(server, "admin-ro"))!;
      expect(restored).toMatchObject({ user_id: before.user_id, password_version: 4 });
      expect(await canLogin(server, "admin-ro", RO)).toBe(true);
    });
  }, BOOT_TIMEOUT);

  test("its address follows EMAIL_DOMAIN while it is enabled", async () => {
    await withServer({ env: { ADMIN_RO_PASSWORD: RO, EMAIL_DOMAIN: "first.example" } }, async (server) => {
      expect((await userRow(server, "admin-ro"))!.email).toBe("admin-ro@first.example");
      await server.restart({ EMAIL_DOMAIN: "second.example" });
      expect(await userRow(server, "admin-ro")).toMatchObject({ email: "admin-ro@second.example", password_version: 1 });
      // Revoked, it keeps the address it had.
      await server.restart({ EMAIL_DOMAIN: "third.example", ADMIN_RO_PASSWORD: undefined });
      expect((await userRow(server, "admin-ro"))!.email).toBe("admin-ro@second.example");
      expect((await userRow(server, "admin"))!.email).toBe("admin@third.example");
    });
  }, BOOT_TIMEOUT);
});

describe("the schema", () => {
  test("a first boot creates the tables, their indexes, and both markers", async () => {
    await withServer({}, async (server) => {
      expect(await bothMarkers(server)).toEqual({
        maintenance_hash: schema.maintenanceHash,
        schema_hash: schema.schemaHash,
      });

      const tables = (
        await server.db.query("SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' ORDER BY table_name")
      ).map((row) => row.table_name);
      expect(tables).toEqual([...schema.tables.map((table) => table.name), "schema_meta"].sort());

      for (const table of schema.tables) {
        const columns = (
          await server.db.query(
            "SELECT column_name FROM information_schema.columns WHERE table_schema = 'public' AND table_name = $1 ORDER BY ordinal_position",
            [table.name]
          )
        ).map((row) => row.column_name);
        expect({ table: table.name, columns }).toEqual({ table: table.name, columns: table.columns.map(([name]) => name) });
      }

      const indexes = new Set(
        (await server.db.query("SELECT indexname FROM pg_indexes WHERE schemaname = 'public'")).map((row) => row.indexname)
      );
      for (const { name } of schema.indexes) expect({ name, exists: indexes.has(name) }).toEqual({ name, exists: true });
      const invalid = await server.db.query("SELECT 1 FROM pg_index WHERE NOT indisvalid");
      expect(invalid).toEqual([]);

      const triggers = (
        await server.db.query("SELECT DISTINCT trigger_name FROM information_schema.triggers WHERE event_object_table = 'mails' ORDER BY 1")
      ).map((row) => row.trigger_name);
      expect(triggers).toEqual(["mails_search_insert", "mails_search_update"]);
    });
  }, BOOT_TIMEOUT);

  test("a second boot on the same database changes neither the markers nor the data", async () => {
    await withServer({}, async (server) => {
      const before = await bothMarkers(server);
      const alice = await createUser(server, "alice");
      const client = server.client();
      await client.login(alice.username, alice.password);
      const admin = (await userRow(server, "admin"))!;
      const sessions = await sessionCount(server);

      await server.restart();
      expect(await bothMarkers(server)).toEqual(before);
      expect((await userRow(server, "admin"))!).toMatchObject({ user_id: admin.user_id, password: admin.password });
      expect((await userRow(server, "alice"))!.user_id).toBe(alice.id);
      expect(await sessionCount(server)).toBe(sessions);
      expect(await whoIs(client)).toBe("alice");

      await server.restart();
      expect(await bothMarkers(server)).toEqual(before);
    });
  }, BOOT_TIMEOUT);

  test("a boot that finds another schema marker brings the schema to its own and rewrites it", async () => {
    await withServer({}, async (server) => {
      await bothMarkers(server);
      await server.kill();
      // An older deployment: a column and an index this code expects are not there.
      await server.db.query("ALTER TABLE users DROP COLUMN imap_uid_validity");
      await server.db.query("DROP INDEX idx_users_email");
      await server.db.query("UPDATE schema_meta SET value = 'an-older-hash'");

      await server.restart();
      expect(await bothMarkers(server)).toEqual({
        maintenance_hash: schema.maintenanceHash,
        schema_hash: schema.schemaHash,
      });
      const columns = (
        await server.db.query("SELECT column_name FROM information_schema.columns WHERE table_name = 'users' AND table_schema = 'public'")
      ).map((row) => row.column_name);
      expect(columns).toContain("imap_uid_validity");
      expect(await server.db.query("SELECT 1 FROM pg_indexes WHERE indexname = 'idx_users_email'")).toHaveLength(1);
      expect(await canLogin(server, "admin", ADMIN_PASSWORD)).toBe(true);
    });
  }, BOOT_TIMEOUT);

  test("with matching markers a boot leaves the schema as it finds it", async () => {
    await withServer({}, async (server) => {
      await bothMarkers(server);
      await server.kill();
      await server.db.query("DROP INDEX idx_mailgun_events_event");
      await server.restart();
      await bothMarkers(server);
      expect((await server.client().get("/api/health")).status).toBe(200);
      // Nothing told this boot there was work to do.
      expect(await server.db.query("SELECT 1 FROM pg_indexes WHERE indexname = 'idx_mailgun_events_event'")).toEqual([]);
    });
  }, BOOT_TIMEOUT);

  test("a database that has rows but no markers is adopted, not recreated", async () => {
    await withServer({}, async (server) => {
      await bothMarkers(server);
      const alice = await createUser(server, "alice");
      await server.kill();
      await server.db.query("DROP TABLE schema_meta");
      await server.restart();
      expect(await bothMarkers(server)).toEqual({
        maintenance_hash: schema.maintenanceHash,
        schema_hash: schema.schemaHash,
      });
      expect((await userRow(server, "alice"))!.user_id).toBe(alice.id);
      expect(await canLogin(server, "alice", alice.password)).toBe(true);
    });
  }, BOOT_TIMEOUT);
});

describe("stopping and starting", () => {
  test.each(["SIGTERM", "SIGINT"] as const)("%s stops the server gracefully: exit code 0", async (signal) => {
    await withServer({}, async (server) => {
      const client = server.client();
      await client.login("admin", ADMIN_PASSWORD);
      expect(await server.kill(signal)).toBe(0);
      // Nothing listens any more.
      await expect(fetch(`${server.url}/api/ping`)).rejects.toThrow();
      // And everything is where it was when the server comes back.
      await server.restart();
      expect(await whoIs(client)).toBe("admin");
    });
  }, BOOT_TIMEOUT);

  test("a graceful stop lets a request that is being answered finish", async () => {
    await withServer({}, async (server) => {
      // Health probes every mail port, which takes long enough to overlap the signal.
      const pending = Array.from({ length: 5 }, () => fetch(`${server.url}/api/health`).then((response) => response.status));
      const exited = server.kill("SIGTERM");
      const statuses = await Promise.allSettled(pending);
      expect(await exited).toBe(0);
      for (const outcome of statuses) {
        if (outcome.status === "fulfilled") expect([200, 503]).toContain(outcome.value);
      }
    });
  }, BOOT_TIMEOUT);

  test("the server exits with 1 when its HTTP port is taken", async () => {
    const blocker = net.createServer();
    const port = await new Promise<number>((resolve) => {
      blocker.listen(0, "0.0.0.0", () => resolve((blocker.address() as net.AddressInfo).port));
    });
    try {
      const { exitCode } = await bootExpectingExit({ env: { PORT: String(port) } });
      expect(exitCode).toBe(1);
    } finally {
      await new Promise((resolve) => blocker.close(resolve));
    }
  }, BOOT_TIMEOUT);

  test("the rate limits do not survive a restart", async () => {
    await withServer({}, async (server) => {
      for (let i = 0; i < 5; i++) await server.client().post("/api/users/login", { username: "admin", password: "wrong" });
      expect((await server.client().post("/api/users/login", { username: "admin", password: ADMIN_PASSWORD })).status).toBe(429);
      await server.restart();
      expect(await canLogin(server, "admin", ADMIN_PASSWORD)).toBe(true);
    });
  }, BOOT_TIMEOUT);
});
