/**
 * The SQL these read paths emit is pinned in `http-query.test.ts`, on the
 * builders' output. What is left here is the part no emitted string can show:
 * the order `getMailHeadersDelta` does its two reads in, and what it returns
 * when one of them fails. Scoped to the one file the function lives in.
 */
import { describe, it, expect, beforeAll } from "bun:test";
import { rowsInIdOrder } from "./http";

describe("getMailHeadersDelta — cursor safety (#457)", () => {
  let fnSource: string;

  beforeAll(async () => {
    const fs = await import("fs/promises");
    const path = await import("path");
    const source = await fs.readFile(
      path.join(import.meta.dir, "http.ts"),
      "utf8"
    );
    const fnMatch = source.match(
      /export const getMailHeadersDelta[\s\S]*?\n};/
    );
    if (!fnMatch) throw new Error("getMailHeadersDelta not found in http.ts");
    // Comments restate both properties in prose; strip them so the guard is
    // about the code.
    fnSource = fnMatch[0].replace(/^\s*\/\/.*$/gm, "");
  });

  it("reads as_of before the data queries", () => {
    // Captured first, `as_of` is a safe lower bound: a row committed while the
    // data queries run is re-sent next call (at-least-once, deduped by the
    // client) rather than skipped forever. Captured after, it would skip.
    const asOfIdx = fnSource.indexOf("buildDeltaAsOfQuery(");
    const headersIdx = fnSource.indexOf("getMailHeaders(");
    expect(asOfIdx).toBeGreaterThanOrEqual(0);
    expect(headersIdx).toBeGreaterThanOrEqual(0);
    expect(asOfIdx).toBeLessThan(headersIdx);
  });

  it("awaits as_of rather than racing it against the data queries", () => {
    // Inside the Promise.all it would no longer be a lower bound.
    expect(fnSource).toMatch(/await pool\.query[\s\S]*?asOf\.sql[\s\S]*?Promise\.all/);
  });

  it("echoes `since` as as_of on failure so the cursor cannot advance", () => {
    // Returning a fresh as_of from a failed call would advance the client past
    // mutations it never received.
    expect(fnSource).toMatch(/return\s*\{\s*as_of:\s*since/);
  });
});

describe("getUnifiedMailHeadersDelta — cursor safety (#457)", () => {
  let fnSource: string;

  beforeAll(async () => {
    const fs = await import("fs/promises");
    const path = await import("path");
    const source = await fs.readFile(
      path.join(import.meta.dir, "http.ts"),
      "utf8"
    );
    const fnMatch = source.match(
      /export const getUnifiedMailHeadersDelta[\s\S]*?\n};/
    );
    if (!fnMatch)
      throw new Error("getUnifiedMailHeadersDelta not found in http.ts");
    fnSource = fnMatch[0].replace(/^\s*\/\/.*$/gm, "");
  });

  it("reads as_of before the data queries", () => {
    const asOfIdx = fnSource.indexOf("buildDeltaAsOfQuery(");
    const headersIdx = fnSource.indexOf("getUnifiedMailHeaders(");
    expect(asOfIdx).toBeGreaterThanOrEqual(0);
    expect(headersIdx).toBeGreaterThanOrEqual(0);
    expect(asOfIdx).toBeLessThan(headersIdx);
  });

  it("awaits as_of rather than racing it against the data queries", () => {
    expect(fnSource).toMatch(/await pool\.query[\s\S]*?asOf\.sql[\s\S]*?Promise\.all/);
  });

  it("echoes `since` as as_of on failure so the cursor cannot advance", () => {
    expect(fnSource).toMatch(/return\s*\{\s*as_of:\s*since/);
  });
});

describe("rowsInIdOrder", () => {
  const ids = Array.from({ length: 23 }, (_, i) => `mail-${i}`);
  const row = (mail_id: string) => ({ mail_id, subject: `Subject of ${mail_id}` });

  const collect = async <R>(batches: AsyncIterable<R[]>) => {
    const sizes: number[] = [];
    const rows: R[] = [];
    for await (const batch of batches) {
      sizes.push(batch.length);
      rows.push(...batch);
    }
    return { sizes, rows };
  };

  it("asks for every id once, batchSize at a time", async () => {
    const asked: string[][] = [];
    await collect(
      rowsInIdOrder(ids, 5, async (batch) => {
        asked.push(batch);
        return batch.map(row);
      })
    );
    expect(asked).toEqual([
      ids.slice(0, 5),
      ids.slice(5, 10),
      ids.slice(10, 15),
      ids.slice(15, 20),
      ids.slice(20, 23),
    ]);
  });

  it("yields rows in id order whatever order the fetch returns them in", async () => {
    const { rows, sizes } = await collect(
      rowsInIdOrder(ids, 5, async (batch) => batch.map(row).reverse())
    );
    expect(rows).toEqual(ids.map(row));
    expect(sizes).toEqual([5, 5, 5, 5, 3]);
  });

  it("skips an id the fetch has no row for", async () => {
    const gone = new Set(["mail-0", "mail-7", "mail-22"]);
    const { rows } = await collect(
      rowsInIdOrder(ids, 5, async (batch) => batch.filter((id) => !gone.has(id)).map(row))
    );
    expect(rows.map((r) => r.mail_id)).toEqual(ids.filter((id) => !gone.has(id)));
  });

  it("fetches nothing for no ids", async () => {
    let calls = 0;
    const { rows } = await collect(
      rowsInIdOrder([], 5, async () => {
        calls++;
        return [];
      })
    );
    expect(rows).toEqual([]);
    expect(calls).toBe(0);
  });
});
