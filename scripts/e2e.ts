/**
 * Runs the black-box suite in `e2e/` against the Bun server, the Rust server,
 * or both — the same test files each time.
 *
 *   bun scripts/e2e.ts                      # both servers
 *   bun scripts/e2e.ts --server bun         # one of them
 *   bun scripts/e2e.ts --server rust users  # only files whose path contains "users"
 *   bun scripts/e2e.ts --debug              # Rust debug build (faster to compile)
 *
 * Needs a reachable Postgres (see `e2e/README.md`). Everything after the
 * recognized flags that starts with `-` is passed to `bun test`.
 */
import fs from "fs";
import path from "path";

const root = path.resolve(import.meta.dir, "..");
const e2eDir = path.join(root, "e2e");
const crateDir = path.join(root, "src", "server-rs");

const args = process.argv.slice(2);
let servers = ["bun", "rust"];
let debugBuild = false;
const filters: string[] = [];
const passthrough: string[] = [];

for (let i = 0; i < args.length; i++) {
  const arg = args[i]!;
  if (arg === "--server") {
    const value = args[++i];
    if (value === "both") servers = ["bun", "rust"];
    else if (value === "bun" || value === "rust") servers = [value];
    else {
      console.error(`--server takes bun, rust or both, not "${value}"`);
      process.exit(2);
    }
  } else if (arg === "--debug") debugBuild = true;
  else if (arg.startsWith("-")) passthrough.push(arg);
  else filters.push(arg);
}

const run = async (command: string[], cwd: string, env: Record<string, string | undefined> = {}) => {
  const child = Bun.spawn(command, {
    cwd,
    env: { ...process.env, ...env },
    stdout: "inherit",
    stderr: "inherit",
    stdin: "inherit",
  });
  return child.exited;
};

const files = [...new Bun.Glob("**/*.e2e.ts").scanSync({ cwd: e2eDir })]
  .sort()
  .filter((file) => !filters.length || filters.some((filter) => file.includes(filter)))
  .map((file) => `./${file}`);

if (!files.length) {
  console.error("No test file matches.");
  process.exit(2);
}

// Both servers serve the built web client, and the static-file tests read it.
if (!fs.existsSync(path.join(root, "build", "client", "index.html"))) {
  console.info("Building the web client for the static-file tests…");
  if ((await run(["bun", "run", "build-client"], root)) !== 0) process.exit(1);
}

let rustBinary: string | undefined;
if (servers.includes("rust")) {
  const profile = debugBuild ? "debug" : "release";
  const build = ["cargo", "build", "--locked", ...(debugBuild ? [] : ["--release"])];
  console.info(`Building the Rust server (${profile})…`);
  if ((await run(build, crateDir)) !== 0) process.exit(1);
  rustBinary = path.join(crateDir, "target", profile, "mail-server");
}

let failed = false;
for (const server of servers) {
  console.info(`\n━━━ ${files.length} file(s) against the ${server} server ━━━`);
  const exitCode = await run(
    ["bun", "test", "--timeout", "30000", ...passthrough, ...files],
    e2eDir,
    {
      MAIL_E2E_SERVER: server,
      ...(server === "rust" && !process.env.MAIL_E2E_RUST_BIN ? { MAIL_E2E_RUST_BIN: rustBinary } : {}),
    }
  );
  if (exitCode !== 0) failed = true;
}

process.exit(failed ? 1 : 0);
