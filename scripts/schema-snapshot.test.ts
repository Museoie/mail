import { describe, expect, it } from "bun:test";
import fs from "fs";

import { buildSnapshot, serializeSnapshot, SNAPSHOT_PATH } from "./schema-snapshot";

describe("src/server-rs/schema.json", () => {
  it("matches the DDL the Bun server issues", () => {
    // The Rust server embeds this file. If this fails, the schema changed:
    // run `bun scripts/schema-snapshot.ts` and commit the result.
    expect(fs.readFileSync(SNAPSHOT_PATH, "utf8")).toBe(serializeSnapshot(buildSnapshot()));
  });
});
