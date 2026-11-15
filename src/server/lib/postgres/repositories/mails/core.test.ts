/**
 * What no emitted string can show: each mark-mail mutation stamps a freshly
 * reserved mod-sequence, guards the update to rows that do not already hold
 * the target flag, and — for the Table-written pair — probes the current
 * flag before reserving anything, so a re-mark costs one SELECT and burns
 * no modseq. Scoped to the one file the functions live in; the spam
 * builder's SQL shape is pinned in `core-query.test.ts`, and the Table
 * calls' shape by the Table layer itself.
 */
import { describe, it, expect, beforeAll } from "bun:test";

const BUILDER_FNS: [string, string][] = [
  ["markMailSpam", "buildMarkMailSpamQuery"],
];

// The Table-written pair: no builder, so the reservation, the guard filter
// and the probe-before-reservation ordering are asserted on the function
// source instead. `targetCheck` is the early return that makes the no-op
// path reserve nothing.
const TABLE_FNS: { name: string; guard: string; targetCheck: string }[] = [
  {
    name: "markMailRead",
    guard: "read: false",
    targetCheck: "if (current.read) return true;",
  },
  {
    name: "markMailSaved",
    guard: "saved: !saved",
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

  for (const { name, guard, targetCheck } of TABLE_FNS) {
    it(`${name} stamps a freshly reserved mod-sequence rather than a constant`, () => {
      // Same obligation as the builder path, carried in the updateWhere
      // data bag: a constant here still matches, still refreshes `updated`
      // and still answers success, while HIGHESTMODSEQ never moves.
      const fn = fnSource(name);
      expect(fn).toContain("[MODSEQ]: await getNextModseq(user_id)");
    });

    it(`${name} guards the update to rows that do not already hold the target flag`, () => {
      // Without the guard the reserved modseq is stamped onto a row whose
      // flag never changed, and a CONDSTORE client is told a mail changed
      // when nothing did.
      const fn = fnSource(name);
      expect(fn).toContain(guard);
    });

    it(`${name} probes the current flag before reserving a mod-sequence`, () => {
      // The no-op path must cost one SELECT and reserve nothing: the probe
      // has to come first, and a mail already in the target state has to
      // return before any reservation happens.
      const fn = fnSource(name);
      const probeAt = fn.indexOf("mailsTable.queryOne");
      const reserveAt = fn.indexOf("await getNextModseq(user_id)");
      expect(probeAt).toBeGreaterThan(-1);
      expect(reserveAt).toBeGreaterThan(-1);
      expect(probeAt).toBeLessThan(reserveAt);
      expect(fn).toContain(targetCheck);
    });
  }
});
