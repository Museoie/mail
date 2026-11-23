import { createHash } from "node:crypto";
import { pool } from "./client";
import { backfillAccountDomains, findDomain } from "./repositories";
import { buildCreateTable, buildCreateIndex, buildIndexName } from "./database";
import { runBootMaintenance, MaintenanceWork, Statement } from "./maintenance";
import { sendAlarm } from "../alarm";
import {
  checkSchemaAtTarget,
  runMigrations,
  writeSchemaMarker,
  MarkerKey,
} from "./migration";
import { searchVectorDdl, searchVectorReindexSql } from "./search-vector";
import {
  HEADERLESS_SQL,
  SUBJECT_KEY_SQL,
} from "./repositories/mails/thread";
import { logger } from "../logger";
import { getDomain, withTimeout } from "../util";
import {
  Table,
  Schema,
  usersTable,
  sessionsTable,
  mailsTable,
  mailboxesTable,
  pushSubscriptionsTable,
  spamAllowlistTable,
  spamTrainingTable,
  mailUidCountersTable,
  mailMailboxUidTable,
  mailgunEventsTable,
  domainsTable,
  domainClaimsTable,
  domainRolesTable,
  formerDomainsTable,
} from "./models";

export const version = "1";
export const index = "mail" + (version ? `-${version}` : "");

/** Every table the app owns, in DDL-creation order (FK dependencies first). */
export const tables: Table<unknown, Schema>[] = [
  usersTable,
  sessionsTable,
  mailboxesTable, // Must be before mails due to foreign key reference
  mailsTable,
  pushSubscriptionsTable,
  spamAllowlistTable,
  spamTrainingTable,
  mailUidCountersTable,
  // Depends on mails (FK to mails.mail_id ON DELETE CASCADE).
  mailMailboxUidTable,
  mailgunEventsTable,
  domainsTable,
  domainClaimsTable,
  // Depends on domains (FK to domains.domain_id ON DELETE CASCADE).
  domainRolesTable,
  formerDomainsTable,
];

// The full-text search index predates `table.indexes` and keeps its original
// name — regenerating it as `idx_mails_search_vector_gin` would leave the
// existing index in place and build a duplicate alongside it.
const MAILS_SEARCH_INDEX_NAME = "idx_mails_search";

/** Gates the row-scaled phase independently of the fatal schema DDL. */
const MAINTENANCE_MARKER_KEY: MarkerKey = "maintenance_hash";

/** Ceiling on alarm delivery, so a wedged webhook cannot delay a stop. */
const MAINTENANCE_ALARM_TIMEOUT_MS = 5_000;

// Raw DDL that isn't captured by `table.schema` / `table.indexes` /
// `searchVector*`.
const IDX_MAILS_SEARCH_SQL = buildCreateIndex("mails", "search_vector", {
  indexName: MAILS_SEARCH_INDEX_NAME,
  using: "gin",
  concurrently: true,
});

// The thread view's subject match (repositories/mails/thread.ts): header-less
// mail looked up by a digest of its normalized subject. An expression index,
// so it can't be a `table.indexes` entry; the expression and predicate must
// match `buildThreadSubjectQuery` exactly for the planner to use it.
const THREAD_SUBJECT_INDEX_NAME = "idx_mails_thread_subject_key";
const IDX_MAILS_THREAD_SUBJECT_SQL = `CREATE INDEX CONCURRENTLY IF NOT EXISTS ${THREAD_SUBJECT_INDEX_NAME}
  ON mails (user_id, (${SUBJECT_KEY_SQL}))
  WHERE ${HEADERLESS_SQL}`;

/**
 * Every index the app owns, as (name, statement) pairs. Names are exposed
 * alongside the SQL because the maintenance phase has to identify invalid
 * leftovers by name before it can rebuild them.
 */
export const indexSpecs = (): Statement[] => {
  const specs: Statement[] = [];
  for (const table of tables) {
    for (const idx of table.indexes) {
      const options = { using: idx.using, opclass: idx.opclass, concurrently: true };
      specs.push({
        name: buildIndexName(table.name, idx.column, options),
        sql: buildCreateIndex(table.name, idx.column, options),
      });
    }
  }
  specs.push({ name: MAILS_SEARCH_INDEX_NAME, sql: IDX_MAILS_SEARCH_SQL });
  specs.push({
    name: THREAD_SUBJECT_INDEX_NAME,
    sql: IDX_MAILS_THREAD_SUBJECT_SQL,
  });
  return specs;
};

/** The statements `bootMaintenance` hands to the long-budget session. */
export const maintenanceWork = (): MaintenanceWork => ({
  indexes: indexSpecs(),
  statements: [
    { name: "search_vector reindex", sql: searchVectorReindexSql(), drain: true },
  ],
});

const digest = (parts: string[]): string =>
  createHash("sha256").update(parts.join("\n")).digest("hex").slice(0, 16);

// Digest of every input the slow-path DDL consumes, and of nothing else. Any
// code change to a table schema, its constraints, or `searchVectorDdl()`
// changes it — the fast path only short-circuits when the DB reflects THIS
// exact code's DDL, so a trigger-body-only PR (which a name-check would
// silently miss) correctly falls through to the slow path.
//
// The maintenance inputs are deliberately absent. This marker gates a block
// that throws and exits the process, so folding an index or a tuning constant
// into it would send every container back through that block for a change it
// does not describe.
export const CURRENT_SCHEMA_HASH: string = ((): string => {
  const parts: string[] = [];
  for (const t of tables) {
    parts.push(`t:${t.name}`);
    parts.push(JSON.stringify(t.schema));
    parts.push(JSON.stringify(t.constraints ?? []));
  }
  parts.push(...searchVectorDdl());
  return digest(parts);
})();

// The same discipline for the row-scaled phase: every index statement it
// issues plus the reindex it drains, so an index-only or chunk-size-only
// change re-runs that phase alone.
export const CURRENT_MAINTENANCE_HASH: string = ((): string => {
  const { indexes, statements } = maintenanceWork();
  return digest(
    [...indexes, ...statements].map((s) => `${s.name}:${s.sql}:${s.drain ?? false}`),
  );
})();

export const postgresIsAvailable = async (): Promise<void> => {
  const maxRetries = 30;
  let retries = 0;

  while (retries < maxRetries) {
    try {
      const client = await pool.connect();
      client.release();
      logger.info("PostgreSQL connection established.");
      return;
    } catch (error: unknown) {
      retries++;
      const message = error instanceof Error ? error.message : String(error);
      logger.info(`PostgreSQL connection attempt ${retries}/${maxRetries} failed: ${message}`);
      
      if (retries >= maxRetries) {
        throw new Error("Failed to connect to PostgreSQL after maximum retries");
      }
      
      await new Promise((resolve) => setTimeout(resolve, 2000));
    }
  }
};

export const initializePostgres = async (): Promise<void> => {
  logger.info("PostgreSQL initialization started.");

  await postgresIsAvailable();

  if (await checkSchemaAtTarget(CURRENT_SCHEMA_HASH)) {
    logger.info("[Fast-path] Schema already at target — skipping DDL.");
    logger.info("Database tables created/verified successfully.");
    return;
  }

  try {
    // Create tables if they don't exist
    for (const table of tables) {
      const createTableSql = buildCreateTable(
        table.name,
        table.schema,
        table.constraints
      );
      await pool.query(createTableSql);
    }

    // Run automatic schema migrations for existing tables
    // This must happen BEFORE index creation - new columns from schema
    // must exist before we try to create indexes on them
    await runMigrations(
      tables.map((t) => ({ name: t.name, schema: t.schema }))
    );

    // Trigger function + the INSERT / column-scoped UPDATE trigger pair. The
    // reindex they share an expression with runs in `bootMaintenance` — it is
    // the one statement here whose cost scales with the table.
    for (const sql of searchVectorDdl()) await pool.query(sql);

    // Record the marker so subsequent boots can fast-path. Written AFTER every
    // statement in this block succeeded, and gated only on them: the
    // row-scaled work in `bootMaintenance` has its own marker, because sending
    // every boot back through this throwing block for as long as an index
    // can't build would just move the crashloop rather than remove it.
    await writeSchemaMarker(CURRENT_SCHEMA_HASH);
  } catch (error: unknown) {
    logger.error("Failed to create tables", {}, error);
    throw new Error("Failed to setup PostgreSQL tables.");
  }

  logger.info("Database tables created/verified successfully.");
};

/**
 * The row-count-scaled half of the boot DDL: index builds and the search-vector
 * reindex. Deliberately NOT part of `initializePostgres`, on two counts.
 *
 * It must not be fatal. A statement here failing means "too slow for its
 * budget", not "the schema is broken" — the app serves correctly without an
 * index, and with a stale search vector on rows nothing has touched. Exiting
 * for it just crashloops the container into the same doomed statement, with
 * no port ever bound to diagnose from.
 *
 * And it must not gate the listeners. Index builds are `CONCURRENTLY` so the
 * table stays writable while they run, which buys nothing if HTTP/SMTP/IMAP
 * aren't up yet. `start.ts` calls this after binding and does not await it.
 *
 * It carries its own marker, separate from the schema one. That marker is what
 * lets a steady-state boot skip the phase entirely — without it every restart
 * would re-run a full-table tsvector recompute that changes nothing. Writing
 * it while a statement is still outstanding would strand that work forever,
 * with every later boot fast-pathing past it and no error anywhere, so it is
 * written only on a clean sweep.
 *
 * Never rejects: `start.ts` does not await it, so a rejection here would
 * surface as a contextless unhandled-rejection page.
 */
export const bootMaintenance = async (signal?: AbortSignal): Promise<void> => {
  try {
    if (await checkSchemaAtTarget(CURRENT_MAINTENANCE_HASH, MAINTENANCE_MARKER_KEY)) {
      logger.info("[Maintenance] Already at target — skipping.");
      return;
    }

    const result = await runBootMaintenance(maintenanceWork(), signal);
    if (result === "complete") {
      await writeSchemaMarker(CURRENT_MAINTENANCE_HASH, MAINTENANCE_MARKER_KEY);
      return;
    }
    // `skipped` means another instance is doing the work, or shutdown
    // cancelled it. Every rolling deploy produces one of those; paging for it
    // would train the alarm to be ignored.
    if (result === "skipped") return;

    const message =
      "Boot maintenance did not complete — an index or the search-vector reindex " +
      "is outstanding. The maintenance marker is withheld, so the next boot retries.";
    logger.warn(`[Maintenance] ${message}`);
    // Degrading instead of exiting must not also mean degrading silently. The
    // delivery is raced rather than awaited: `start.ts` awaits this phase on
    // the graceful-shutdown path, and `sendAlarm`'s fetch carries no timeout of
    // its own, so a wedged webhook would hold the stop open until compose's
    // grace period expired and SIGKILL replaced the clean exit.
    await withTimeout(
      sendAlarm("Boot Maintenance Incomplete", message).catch(() => undefined),
      MAINTENANCE_ALARM_TIMEOUT_MS
    );
  } catch (error: unknown) {
    logger.error("[Maintenance] Phase failed unexpectedly.", {}, error);
  }
};

/**
 * Checks the domain rows against the accounts, at every boot before any
 * listener binds.
 *
 * Every account that can sign in owns its own name under the served zone.
 * New accounts get that row where they are created (sign-up, single sign-on,
 * the user script), and delivery creates a missing one on first mail. This
 * covers the accounts neither reaches before they need it: an account made by
 * a previous version, including one made during a rolling deploy, or a row
 * restored or imported straight into `users`. Without its row such an account
 * cannot send, list its accounts or share its domain until mail arrives for
 * it. The statement is idempotent: a name already taken stays with its owner.
 *
 * It also warns when no account owns the served zone itself, since mail to
 * `*@<EMAIL_DOMAIN>` is then refused.
 */
export const initializeDomains = async (): Promise<void> => {
  const zone = getDomain().toLowerCase();

  if (!process.env.EMAIL_DOMAIN) {
    logger.warn(
      "[CONFIG WARNING] EMAIL_DOMAIN is not set. Defaulting to 'mydomain'.\n" +
        "  Accounts receive mail at <username>.<EMAIL_DOMAIN>, so set EMAIL_DOMAIN=yourdomain.com\n" +
        "  in your .env file to receive mail."
    );
  }

  const backfilled = await backfillAccountDomains(zone);
  if (backfilled > 0) {
    logger.info("Gave accounts their own domain under the served zone.", { backfilled });
  }

  if (!(await findDomain(zone))) {
    logger.warn(
      `[CONFIG WARNING] No account owns ${zone}, so mail to *@${zone} is refused.\n` +
        `  Give it to an account with: bun run user add-domain <username> ${zone}\n` +
        `  (from the built files, as in the Docker image: bun ./build/cli/user.js add-domain <username> ${zone})`
    );
  }
};
