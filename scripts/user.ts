/**
 * Manages password accounts, for a server that runs without single sign-on.
 * Reads the same environment as the server (the database settings and
 * EMAIL_DOMAIN). The password is read from standard input, never from the
 * command line, so it stays out of the shell history and the process list.
 *
 *   bun run user create <username> [--email <address>] [--domain <name>]...
 *   bun run user set-password <username>
 *   bun run user add-domain <username> <domain>
 *   bun run user disable <username>
 *
 * Examples:
 *   bun run user create alice --domain example.com   # alice also receives *@example.com
 *   printf '%s\n' "$PASSWORD" | bun run user create bob
 */

import { initializePostgres, initializeDomains } from "../src/server/lib/postgres/initialize";
import { pool } from "../src/server/lib/postgres/client";
import {
  addDomain,
  createUser,
  disableUser,
  setPassword,
  UserAdminError,
} from "../src/server/lib/user-admin";

const USAGE = `Usage:
  bun run user create <username> [--email <address>] [--domain <name>]...
  bun run user set-password <username>
  bun run user add-domain <username> <domain>
  bun run user disable <username>`;

const fail = (message: string): never => {
  console.error(message);
  process.exit(1);
};

/** One line of standard input; on a terminal, typed without echo. */
const readPassword = async (prompt: string): Promise<string> => {
  const stdin = process.stdin;
  if (!stdin.isTTY) {
    // The first line, without waiting for the end of input: a password typed
    // into a session with no terminal (`docker exec -i`) ends at Enter too.
    let text = "";
    for await (const chunk of stdin) {
      text += (chunk as Buffer).toString("utf8");
      if (/\r?\n/.test(text)) break;
    }
    return text.split(/\r?\n/)[0] ?? "";
  }
  process.stdout.write(prompt);
  stdin.setRawMode(true);
  stdin.resume();
  let value = "";
  return new Promise((resolve) => {
    const onData = (data: Buffer) => {
      for (const char of data.toString("utf8")) {
        if (char === "\r" || char === "\n") {
          stdin.setRawMode(false);
          stdin.pause();
          stdin.off("data", onData);
          process.stdout.write("\n");
          resolve(value);
          return;
        }
        if (char === "\u0003") {
          process.stdout.write("\n");
          process.exit(130);
        }
        if (char === "\u007f") value = value.slice(0, -1);
        else value += char;
      }
    };
    stdin.on("data", onData);
  });
};

const readNewPassword = async (): Promise<string> => {
  const password = await readPassword("Password: ");
  if (process.stdin.isTTY && (await readPassword("Repeat password: ")) !== password) {
    fail("The passwords don't match.");
  }
  return password;
};

const parseOptions = (args: string[]) => {
  const positional: string[] = [];
  const domains: string[] = [];
  let email: string | undefined;
  for (let i = 0; i < args.length; i++) {
    const arg = args[i];
    if (arg === "--domain") domains.push(args[++i] ?? fail(USAGE));
    else if (arg === "--email") email = args[++i] ?? fail(USAGE);
    else if (arg.startsWith("--")) fail(`Unknown option ${arg}\n${USAGE}`);
    else positional.push(arg);
  }
  return { positional, domains, email };
};

const main = async () => {
  const [command, ...rest] = process.argv.slice(2);
  const { positional, domains, email } = parseOptions(rest);
  const [username, domain] = positional;
  if (!command || !username) fail(USAGE);

  // The tables are the server's; make sure they exist, as its boot does.
  await initializePostgres();
  await initializeDomains();

  if (command === "create") {
    const password = await readNewPassword();
    const created = await createUser({ username, password, email, domains });
    console.info(
      `Created ${created.username} (${created.email}), receiving mail for ${created.domains.join(", ")}.`
    );
  } else if (command === "set-password") {
    await setPassword(username, await readNewPassword());
    console.info(`Set a new password for ${username}.`);
  } else if (command === "add-domain") {
    if (!domain) fail(USAGE);
    console.info(`${username} now owns ${await addDomain(username, domain)}.`);
  } else if (command === "disable") {
    await disableUser(username);
    console.info(`${username} can no longer sign in with a password; its sessions ended.`);
  } else {
    fail(USAGE);
  }
};

main()
  .catch((error) => {
    console.error(error instanceof UserAdminError ? error.message : error);
    process.exitCode = 1;
  })
  // The server modules this imports keep timers of their own, so the process
  // is ended explicitly once the pool has closed.
  .finally(async () => {
    await pool.end();
    process.exit();
  });
