import { describe, expect, test } from "bun:test";
import fs from "fs";
import net from "net";
import path from "path";
import {
  ADMIN_PASSWORD,
  REPO_ROOT,
  bootExpectingExit,
  createUser,
  seedAdmin,
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

describe("accounts at boot", () => {
  // An operator creates accounts with `bun run user` (or single sign-on does);
  // the server itself seeds none, and ADMIN_PASSWORD, ADMIN_PASSWORD_RESET and
  // ADMIN_RO_PASSWORD are not read.
  test("a first boot creates no account, whatever ADMIN_* say", async () => {
    await withServer(
      { seedAdmin: false, env: { ADMIN_PASSWORD: "boot-admin", ADMIN_PASSWORD_RESET: "1", ADMIN_RO_PASSWORD: "boot-ro" } },
      async (server) => {
        expect(await server.db.query("SELECT username FROM users")).toEqual([]);
        expect(await server.db.query("SELECT name FROM domains")).toEqual([]);
        expect(await canLogin(server, "admin", "boot-admin")).toBe(false);
        expect(await canLogin(server, "admin-ro", "boot-ro")).toBe(false);
      }
    );
  }, BOOT_TIMEOUT);

  test("warns at each boot while no account owns EMAIL_DOMAIN, and stops once one does", async () => {
    const WARNING = "No account owns mail.test, so mail to *@mail.test is refused.";
    await withServer({ seedAdmin: false }, async (server) => {
      const count = () => server.logs().split(WARNING).length - 1;
      expect(count()).toBe(1);
      await server.restart();
      expect(count()).toBe(2);
      await seedAdmin(server, ADMIN_PASSWORD);
      await server.restart();
      expect(count()).toBe(2);
    });
  }, BOOT_TIMEOUT);

  test("gives an account with no domain its own name under EMAIL_DOMAIN, and leaves a taken name alone", async () => {
    await withServer({}, async (server) => {
      const bare = async (username: string) =>
        (
          await server.db.query<{ user_id: string }>(
            "INSERT INTO users (username, password, email) VALUES ($1, 'x', $2) RETURNING user_id",
            [username, `${username}@elsewhere.example`]
          )
        )[0]!.user_id;
      const bob = await bare("bob");
      const carol = await bare("carol");
      // Someone else holds carol's name already.
      await server.db.query("INSERT INTO domains (name, user_id) VALUES ('carol.mail.test', $1)", [bob]);
      // A sign-up that never set a password gets nothing.
      await server.db.query("INSERT INTO users (username, email, token) VALUES ('user_pending1', 'p@elsewhere.example', 't')");
      await server.restart();
      const owners = await server.db.query<{ name: string; user_id: string }>(
        "SELECT name, user_id FROM domains WHERE name IN ('bob.mail.test', 'carol.mail.test', 'user_pending1.mail.test') ORDER BY name"
      );
      expect(owners).toEqual([
        { name: "bob.mail.test", user_id: bob },
        { name: "carol.mail.test", user_id: bob },
      ]);
      expect(carol).not.toBe(bob);
      // Idempotent: another boot changes nothing.
      await server.restart();
      expect(
        await server.db.query("SELECT name FROM domains WHERE name IN ('bob.mail.test', 'carol.mail.test') ORDER BY name")
      ).toHaveLength(2);
    });
  }, BOOT_TIMEOUT);

  test("an account keeps its password and sessions across restarts with any ADMIN_*", async () => {
    await withServer({}, async (server) => {
      const client = server.client();
      await client.login("admin", ADMIN_PASSWORD);
      await server.restart({ ADMIN_PASSWORD: "rotated", ADMIN_PASSWORD_RESET: "1" });
      expect(await whoIs(client)).toBe("admin");
      expect(await canLogin(server, "admin", ADMIN_PASSWORD)).toBe(true);
      expect(await canLogin(server, "admin", "rotated")).toBe(false);
      expect((await userRow(server, "admin"))!.password_version).toBe(1);
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
