/**
 * Cross-server check of the database-backed spam code.
 *
 * Runs the Bun server's own training, classification, allowlist and spam-check
 * code against a PostgreSQL database, stage by stage as
 * `crosscheck_input.json` describes, and records what the tables then hold and
 * what the code answers. `src/server-rs/tests/spam_database.rs` performs the
 * same stages with the Rust server and requires the same record.
 *
 * From the repository root:
 *
 *   # Both stages on a fresh database, written as the fixture the Rust test reads
 *   bun src/server-rs/tests/golden/spam/crosscheck.ts --database mail_spam_ts_check --create --stages a,b --write --drop
 *
 *   # Stage a only, leaving the database for the Rust server to continue on
 *   bun src/server-rs/tests/golden/spam/crosscheck.ts --database mail_spam_shared --create --stages a
 *
 *   # Read a database both servers wrote and compare it with the fixture
 *   bun src/server-rs/tests/golden/spam/crosscheck.ts --database mail_spam_shared --check --drop
 *
 * The server is `MAIL_TEST_POSTGRES_HOST`, `_PORT`, `_USER`, `_PASSWORD`
 * (default `postgres:postgres@127.0.0.1:5432`). Nothing here reaches the
 * network: every connecting address in the input is private or absent.
 */

import { readFileSync, writeFileSync } from "fs";
import { join } from "path";

import type { EmailContext } from "../../../../server/lib/spam/types";

interface Stage {
  name: string;
  train: { isSpam: boolean; email: EmailContext }[];
  allowlistAdd: string[];
  allowlistRemove: string[];
}

interface Input {
  userId: string;
  username: string;
  stages: Stage[];
  classify: EmailContext[];
  allowlisted: string[];
  checkSpam: EmailContext[];
}

const DIR = import.meta.dir;
const FIXTURE = join(DIR, "crosscheck.json");
const input: Input = JSON.parse(readFileSync(join(DIR, "crosscheck_input.json"), "utf8"));
const schema: { tables: { name: string; createSql: string }[] } = JSON.parse(
  readFileSync(join(DIR, "../../../schema.json"), "utf8")
);

const args = process.argv.slice(2);
const flag = (name: string): boolean => args.includes(`--${name}`);
const option = (name: string): string | undefined => {
  const index = args.indexOf(`--${name}`);
  return index >= 0 ? args[index + 1] : undefined;
};

const database = option("database");
if (!database || !/^mail_spam_[a-z0-9_]+$/.test(database)) {
  console.error("--database <name> is required and must start with mail_spam_");
  process.exit(2);
}
const stageNames = (option("stages") ?? "").split(",").filter(Boolean);

// The server's pool reads its settings when the module loads, so they are in
// place before anything of the server is imported.
process.env.POSTGRES_HOST = process.env.MAIL_TEST_POSTGRES_HOST ?? "127.0.0.1";
process.env.POSTGRES_PORT = process.env.MAIL_TEST_POSTGRES_PORT ?? "5432";
process.env.POSTGRES_USER = process.env.MAIL_TEST_POSTGRES_USER ?? "postgres";
process.env.POSTGRES_PASSWORD = process.env.MAIL_TEST_POSTGRES_PASSWORD ?? "postgres";
process.env.POSTGRES_DATABASE = database;
process.env.LOG_LEVEL = "error";

const { Pool } = await import("../../../../server/lib/postgres/pool");
const admin = async (sql: string): Promise<void> => {
  const pool = new Pool({
    host: process.env.POSTGRES_HOST as string,
    port: parseInt(process.env.POSTGRES_PORT as string, 10),
    user: process.env.POSTGRES_USER as string,
    password: process.env.POSTGRES_PASSWORD,
    database: "postgres",
    max: 1,
    idleTimeoutMillis: 1000,
    connectionTimeoutMillis: 2000,
    statementTimeoutMillis: 30_000,
    queryTimeoutMillis: 30_000,
  });
  try {
    await pool.query(sql);
  } finally {
    await pool.end();
  }
};

if (flag("create")) await admin(`CREATE DATABASE ${database}`);

const { pool } = await import("../../../../server/lib/postgres/client");
const { trainWithEmail, classifyEmail } = await import("../../../../server/lib/spam/classifier");
const { checkSpam } = await import("../../../../server/lib/spam/service");
const { getClassifierDocCounts } = await import("../../../../server/lib/postgres/repositories/spam_training");
const { addAllowlistEntry, getAllowlistForUser, isAllowlisted, removeAllowlistEntry } = await import(
  "../../../../server/lib/postgres/repositories/spam_allowlists"
);

for (const table of ["users", "spam_allowlist", "spam_training"]) {
  const spec = schema.tables.find((candidate) => candidate.name === table);
  if (!spec) throw new Error(`schema.json has no table ${table}`);
  await pool.query(spec.createSql);
}
await pool.query("INSERT INTO users (user_id, username) VALUES ($1, $2) ON CONFLICT DO NOTHING", [
  input.userId,
  input.username,
]);

/** What the tables hold and what the code answers, in a form both servers can produce. */
const snapshot = async () => {
  const training = await pool.query<{ word: string; spam_count: number; ham_count: number }>(
    "SELECT word, spam_count, ham_count FROM spam_training WHERE user_id = $1 ORDER BY word COLLATE \"C\"",
    [input.userId]
  );
  const classify: { score: number; reason: string | null }[] = [];
  for (const email of input.classify) classify.push(await classifyEmail(input.userId, email));
  const allowlisted: boolean[] = [];
  for (const address of input.allowlisted) allowlisted.push(await isAllowlisted(input.userId, address));
  const spam: unknown[] = [];
  for (const email of input.checkSpam) spam.push(await checkSpam(input.userId, email));
  return {
    training: training.rows.map((row) => [row.word, row.spam_count, row.ham_count]),
    docCounts: await getClassifierDocCounts(input.userId),
    classify,
    // Newest first, as the route lists them.
    allowlist: (await getAllowlistForUser(input.userId)).map((entry) => entry.pattern),
    allowlisted,
    checkSpam: spam,
  };
};

const record: Record<string, unknown> = {};
for (const name of stageNames) {
  const stage = input.stages.find((candidate) => candidate.name === name);
  if (!stage) throw new Error(`crosscheck_input.json has no stage ${name}`);
  for (const { email, isSpam } of stage.train) await trainWithEmail(input.userId, email, isSpam);
  const removed: boolean[] = [];
  for (const pattern of stage.allowlistRemove) removed.push(await removeAllowlistEntry(input.userId, pattern));
  const added: string[] = [];
  for (const pattern of stage.allowlistAdd) added.push((await addAllowlistEntry(input.userId, pattern)).status);
  record[name] = { allowlistRemove: removed, allowlistAdd: added, snapshot: await snapshot() };
}

let failed = false;
if (flag("write")) {
  const lines = Object.entries(record).map(([name, value]) => `${JSON.stringify(name)}:${JSON.stringify(value)}`);
  writeFileSync(FIXTURE, `{\n${lines.join(",\n")}\n}\n`);
  console.log(`wrote ${FIXTURE}`);
}
if (flag("check")) {
  // The state after the last stage, whichever server ran it.
  const fixture: Record<string, { snapshot: unknown }> = JSON.parse(readFileSync(FIXTURE, "utf8"));
  const last = input.stages[input.stages.length - 1].name;
  const expected = JSON.stringify(fixture[last].snapshot);
  const actual = JSON.stringify(await snapshot());
  failed = expected !== actual;
  console.log(failed ? `MISMATCH\nexpected ${expected}\nactual   ${actual}` : "the database matches the fixture");
}
if (!flag("write") && !flag("check")) console.log(JSON.stringify(record));

await pool.end();
if (flag("drop")) await admin(`DROP DATABASE IF EXISTS ${database} WITH (FORCE)`);
process.exit(failed ? 1 : 0);
