/**
 * The SQL these mutations emit is pinned in `core-query.test.ts`, on the
 * builders' output. What is left here is the part no emitted string can show:
 * each `buildMarkMail*Query` takes `modseq` as a parameter, so every assertion
 * on its output holds for any value the caller passes, and where that value
 * comes from is visible only at the call site. Scoped to the one file the
 * functions live in.
 */
import { describe, it, expect, beforeAll } from "bun:test";

const FUNCTIONS: [string, string][] = [
  ["markMailSpam", "buildMarkMailSpamQuery"],
  ["markMailRead", "buildMarkMailReadQuery"],
  ["markMailSaved", "buildMarkMailSavedQuery"]
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

  for (const [fnName, builderName] of FUNCTIONS) {
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
});
