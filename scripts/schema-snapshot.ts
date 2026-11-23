/**
 * The database DDL the Bun server issues at boot, as one JSON document.
 *
 * The Rust server (`src/server-rs`) embeds `src/server-rs/schema.json` and
 * issues exactly these statements, so the two servers create the same schema,
 * agree on the `schema_meta` hashes, and can be swapped on one database.
 *
 *   bun scripts/schema-snapshot.ts          # rewrite the snapshot
 *
 * `scripts/schema-snapshot.test.ts` fails when the file is out of date.
 */
import fs from "fs";
import path from "path";

import {
  tables,
  indexSpecs,
  maintenanceWork,
  CURRENT_SCHEMA_HASH,
  CURRENT_MAINTENANCE_HASH,
} from "../src/server/lib/postgres/initialize";
import { buildCreateTable } from "../src/server/lib/postgres/database";
import { searchVectorDdl } from "../src/server/lib/postgres/search-vector";

export const SNAPSHOT_PATH = path.resolve(import.meta.dir, "..", "src", "server-rs", "schema.json");

export interface SchemaSnapshot {
  /** Value of `schema_meta.schema_hash` once the DDL below has run. */
  schemaHash: string;
  /** Value of `schema_meta.maintenance_hash` once the maintenance phase completed. */
  maintenanceHash: string;
  /** Tables in creation order. `columns` keeps declaration order for the column migration. */
  tables: { name: string; createSql: string; columns: [string, string][] }[];
  /** Trigger function and triggers, run after the tables exist. */
  searchVectorDdl: string[];
  /** Index builds of the maintenance phase. */
  indexes: { name: string; sql: string }[];
  /** Row-scaled statements of the maintenance phase. */
  statements: { name: string; sql: string; drain: boolean }[];
}

export const buildSnapshot = (): SchemaSnapshot => ({
  schemaHash: CURRENT_SCHEMA_HASH,
  maintenanceHash: CURRENT_MAINTENANCE_HASH,
  tables: tables.map((table) => ({
    name: table.name,
    createSql: buildCreateTable(table.name, table.schema, table.constraints),
    columns: Object.entries(table.schema),
  })),
  searchVectorDdl: searchVectorDdl(),
  indexes: indexSpecs().map(({ name, sql }) => ({ name, sql })),
  statements: maintenanceWork().statements.map(({ name, sql, drain }) => ({
    name,
    sql,
    drain: drain ?? false,
  })),
});

export const serializeSnapshot = (snapshot: SchemaSnapshot): string =>
  `${JSON.stringify(snapshot, null, 2)}\n`;

if (import.meta.main) {
  fs.mkdirSync(path.dirname(SNAPSHOT_PATH), { recursive: true });
  fs.writeFileSync(SNAPSHOT_PATH, serializeSnapshot(buildSnapshot()));
  console.info(`Wrote ${path.relative(process.cwd(), SNAPSHOT_PATH)}`);
  // Importing the database layer leaves timers running.
  process.exit(0);
}
