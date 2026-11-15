/**
 * What no emitted string can show: each mark-mail mutation stamps a freshly
 * reserved mod-sequence, guards the update to rows that do not already hold
 * the target flag, and — for the Table-written pair — probes the current
 * flag before reserving anything, so a re-mark reserves no modseq. The
 * spam builder's SQL shape is pinned in `core-query.test.ts`; the Table
 * calls' shape is pinned here, on the call sites themselves: each
 * assertion extracts the `updateWhere(` / `queryOne(` argument list and
 * compares it, so a mutant that moves a predicate or a column to another
 * call still fails.
 */
import { describe, it, expect, beforeAll } from "bun:test";

const BUILDER_FNS: [string, string][] = [
  ["markMailSpam", "buildMarkMailSpamQuery"],
];

// The Table-written pair: no builder, so the reservation, the guard filter,
// the probe column list and the probe-before-reservation ordering are
// asserted on the call sites in the function source. `filter` is the exact
// updateWhere filter object; `columns` is the exact probe column list;
// `targetCheck` is the early return that makes the no-op path reserve
// nothing.
const TABLE_FNS: {
  name: string;
  filter: string;
  columns: string;
  targetCheck: string;
}[] = [
  {
    name: "markMailRead",
    filter: "{ [MAIL_ID]: mail_id, [USER_ID]: user_id, read: false }",
    columns: "[READ]",
    targetCheck: "if (current.read) return true;",
  },
  {
    name: "markMailSaved",
    filter: "{ [MAIL_ID]: mail_id, [USER_ID]: user_id, saved: !saved }",
    columns: "[SAVED]",
    targetCheck: "if (current.saved === saved) {",
  },
];

describe("mark-mail mutations — mod-sequence reservation", () => {
  let source: string;

  beforeAll(async () => {
    const fs = await import("fs/promises");
    const path = await import("path");
    source = await fs.readFile(path.join(import.meta.dir, "core.ts"), "utf8");
  });

  const fnSource = (name: string): string => {
    const match = source.match(
      new RegExp(`export const ${name}[\\s\\S]*?\\n};`)
    );
    if (!match) throw new Error(`${name} not found in core.ts`);
    // A comment inside the argument list would land in the parsed argument
    // and fail for the wrong reason.
    return match[0].replace(/^\s*\/\/.*$/gm, "");
  };

  const norm = (s: string): string => s.replace(/\s+/g, " ").trim();

  /** Split a call's argument list on top-level commas. */
  const splitTopLevel = (args: string): string[] => {
    const out: string[] = [];
    let depth = 0;
    let cur = "";
    for (const ch of args) {
      if (ch === "{" || ch === "[" || ch === "(") depth++;
      if (ch === "}" || ch === "]" || ch === ")") depth--;
      if (ch === "," && depth === 0) {
        out.push(cur);
        cur = "";
        continue;
      }
      cur += ch;
    }
    if (cur.trim()) out.push(cur);
    return out.map((a) => a.trim());
  };

  /** The `updateWhere(` argument list of the function, as [filter, data, returning]. */
  const updateWhereArgs = (fn: string): string[] => {
    const match = fn.match(/mailsTable\.updateWhere\(([\s\S]*?)\n  \);/);
    if (!match) throw new Error("updateWhere call not found");
    return splitTopLevel(match[1]);
  };

  /** The column-list argument of every `queryOne(` call in the function. */
  const probeColumns = (fn: string): string[] => {
    const matches = [...fn.matchAll(/mailsTable\.queryOne\([\s\S]*?,\s*(\[[^\]]+\])\s*\n  \);/g)];
    if (matches.length === 0) throw new Error("no queryOne call found");
    return matches.map((m) => m[1]);
  };

  for (const [fnName, builderName] of BUILDER_FNS) {
    it(`${fnName} stamps a freshly reserved mod-sequence rather than a constant`, () => {
      // A read, a save and a spam flip are all flag changes: each has to
      // advance HIGHESTMODSEQ. With a constant the UPDATE still matches,
      // `updated` still refreshes and the route still answers success —
      // while a CONDSTORE client reads an unmoved HIGHESTMODSEQ and never
      // resyncs the flag.
      const fn = fnSource(fnName);
      const call = fn.match(
        new RegExp(`${builderName}\\(([\\s\\S]*?)\\)\\s*;`)
      );
      expect(call, `${builderName} call not found in ${fnName}`).not.toBeNull();
      const args = call![1].split(",").map((argument) => argument.trim());
      expect(args[args.length - 1]).toBe("await getNextModseq(user_id)");
    });
  }

  for (const { name, filter, columns, targetCheck } of TABLE_FNS) {
    it(`${name} stamps a freshly reserved mod-sequence rather than a constant`, () => {
      // Same obligation as the builder path, carried in the updateWhere
      // data bag: a constant here still matches, still refreshes `updated`
      // and still answers success, while HIGHESTMODSEQ never moves. The
      // reservation is asserted inside the updateWhere call, not merely
      // somewhere in the function.
      const fn = fnSource(name);
      const args = updateWhereArgs(fn);
      expect(args).toHaveLength(3);
      expect(norm(args[1])).toContain("[MODSEQ]: await getNextModseq(user_id)");
    });

    it(`${name} guards the update to rows that do not already hold the target flag`, () => {
      // Without the guard the reserved modseq is stamped onto a row whose
      // flag never changed, and a CONDSTORE client is told a mail changed
      // when nothing did. The filter is compared as an object, so moving
      // the guard predicate to another call still fails.
      const fn = fnSource(name);
      const args = updateWhereArgs(fn);
      expect(args).toHaveLength(3);
      expect(norm(args[0])).toBe(filter);
    });

    it(`${name} probes the current flag before reserving a mod-sequence`, () => {
      // The no-op path must reserve nothing: the probe has to come first,
      // the early return has to sit between the probe and the update, and
      // the update has to be reachable from the early return — an
      // unconditional `return true` after the check would answer success
      // without ever writing. The slice starts after the check's own block,
      // so the early return itself is not counted against reachability.
      const fn = fnSource(name);
      const probeAt = fn.indexOf("mailsTable.queryOne");
      const checkAt = fn.indexOf(targetCheck);
      const updateAt = fn.indexOf("mailsTable.updateWhere");
      expect(probeAt).toBeGreaterThan(-1);
      expect(checkAt).toBeGreaterThan(probeAt);
      expect(updateAt).toBeGreaterThan(checkAt);
      const checkEnd = targetCheck.endsWith("{")
        ? fn.indexOf("\n  }", checkAt) + "\n  }".length
        : checkAt + targetCheck.length;
      expect(checkEnd).toBeGreaterThan(checkAt);
      const between = fn.slice(checkEnd, updateAt);
      expect(between).not.toMatch(/\breturn\b/);
    });

    it(`${name} projects only the flag in the probe, never the full row`, () => {
      // The probe runs on every call, including the IMAP body-only loop and
      // every mail open (whose route already loaded the row). Selecting the
      // full row would detoast the body columns for a boolean. Both the
      // probe and the re-probe carry the same single-column list.
      const fn = fnSource(name);
      expect(probeColumns(fn)).toEqual([columns, columns]);
    });
  }
});
