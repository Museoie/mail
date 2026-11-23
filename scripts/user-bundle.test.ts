/**
 * The Docker image carries only build/, so the account script has to run as a
 * bundle on its own: no source tree and no node_modules next to it. Without it
 * an operator has no way to create an account or give one a domain there.
 */
import { describe, expect, it } from "bun:test";
import fs from "fs";
import os from "os";
import path from "path";

const root = path.resolve(import.meta.dir, "..");

describe("the account script bundle", () => {
  it("builds from scripts/user.ts and runs with nothing beside it", async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "user-bundle-"));
    try {
      const result = await Bun.build({
        entrypoints: [path.join(root, "scripts", "user.ts")],
        outdir: dir,
        naming: "user.js",
        target: "bun",
      });
      expect(result.success).toBe(true);
      expect(fs.readdirSync(dir)).toEqual(["user.js"]);

      // No arguments: it answers its usage and exits 1, before any database work.
      const run = Bun.spawnSync(["bun", path.join(dir, "user.js")], { cwd: dir, env: { PATH: process.env.PATH } });
      expect(run.exitCode).toBe(1);
      expect(run.stderr.toString()).toContain("bun run user create <username>");
    } finally {
      fs.rmSync(dir, { recursive: true, force: true });
    }
  }, 60_000);

  it("is built by the server build, next to the server bundle", () => {
    const pack = fs.readFileSync(path.join(root, "src", "server", "pack.ts"), "utf8");
    expect(pack).toContain('path.resolve(root, "..", "scripts", "user.ts")');
    expect(pack).toContain('naming: "user.js"');
    expect(fs.readFileSync(path.join(root, "Dockerfile"), "utf8")).toContain("COPY --from=builder /app/build ./build");
  });
});
