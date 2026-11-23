import crypto from "crypto";
import type { TestServer } from "./server";

/** A bcrypt digest both servers verify; cost 4 keeps a test run fast. */
export const hashPassword = (password: string): Promise<string> =>
  Bun.password.hash(new TextEncoder().encode(password).subarray(0, 72), {
    algorithm: "bcrypt",
    cost: 4,
  });

export interface TestUser {
  id: string;
  username: string;
  email: string;
  password: string;
}

/**
 * Inserts a registered account directly, the way a completed sign-up leaves
 * one. `username` must be a lower-case DNS label for its mail to route.
 *
 * ```ts
 * const alice = await createUser(server, "alice");
 * await client.login(alice.username, alice.password);
 * ```
 */
export const createUser = async (
  server: TestServer,
  username: string,
  overrides: { password?: string; email?: string } = {}
): Promise<TestUser> => {
  const password = overrides.password ?? `password-of-${username}`;
  const email = overrides.email ?? `${username}@elsewhere.example`;
  const [row] = await server.db.query<{ user_id: string }>(
    "INSERT INTO users (username, password, email) VALUES ($1, $2, $3) RETURNING user_id",
    [username, await hashPassword(password), email]
  );
  // A finished sign-up owns its own name under the served zone.
  await giveDomains(server, row!.user_id, [`${username}.${server.domain}`]);
  return { id: row!.user_id, username, email, password };
};

/**
 * Makes `userId` the owner of each of `domains`, as `bun run user add-domain`
 * does. A name someone already owns stays theirs.
 */
export const giveDomains = async (server: TestServer, userId: string, domains: string[]) => {
  for (const domain of domains) {
    await server.db.query(
      "INSERT INTO domains (name, user_id) VALUES ($1, $2) ON CONFLICT (name) DO NOTHING",
      [domain.toLowerCase(), userId]
    );
  }
};

/**
 * The account an operator sets up after installing: `admin`, owning the bare
 * served zone and its own `admin.<zone>`, as the upgrade steps leave it.
 * Idempotent, and a server that seeds `admin` itself keeps its row.
 */
export const seedAdmin = async (server: TestServer, password: string) => {
  await server.db.query(
    `INSERT INTO users (username, password, email) VALUES ('admin', $1, $2)
     ON CONFLICT (username) DO NOTHING`,
    [await hashPassword(password), `admin@${server.domain}`]
  );
  const admin = await adminUser(server);
  await giveDomains(server, admin.id, [server.domain, `admin.${server.domain}`]);
};

/** The `admin` account the harness seeds (see {@link seedAdmin}). */
export const adminUser = async (server: TestServer): Promise<{ id: string; email: string }> => {
  const [row] = await server.db.query<{ user_id: string; email: string }>(
    "SELECT user_id, email FROM users WHERE username = 'admin'"
  );
  return { id: row!.user_id, email: row!.email };
};

export interface AddressValue {
  address?: string;
  name?: string;
}

/** `{ address, name }`, the element of every stored address list. */
export const addr = (address: string, name?: string): AddressValue =>
  name === undefined ? { address } : { address, name };

const addressText = (list: AddressValue[]) =>
  list.map(({ address, name }) => (name ? `${name} <${address}>` : `${address}`)).join(", ");

/**
 * A row of the `mails` table. Every field is optional; `insertMail` fills the
 * ones a real delivery always sets. JSONB columns take plain values, and
 * `null` stores SQL NULL.
 */
export interface MailRow {
  mail_id?: string;
  message_id?: string;
  subject?: string;
  date?: string | Date;
  html?: string;
  text?: string;
  from_address?: unknown;
  from_text?: string | null;
  to_address?: unknown;
  to_text?: string | null;
  cc_address?: unknown;
  cc_text?: string | null;
  bcc_address?: unknown;
  bcc_text?: string | null;
  reply_to_address?: unknown;
  reply_to_text?: string | null;
  envelope_from?: unknown;
  envelope_to?: unknown;
  attachments?: unknown;
  read?: boolean;
  saved?: boolean;
  sent?: boolean;
  deleted?: boolean;
  draft?: boolean;
  answered?: boolean;
  expunged?: boolean;
  insight?: unknown;
  uid_domain?: number;
  modseq?: number;
  spam_score?: number;
  spam_reasons?: unknown;
  is_spam?: boolean;
  rfc822_size?: number | null;
  in_reply_to?: string | null;
  reference_ids?: unknown;
  updated?: string | Date;
}

export interface InsertedMail extends MailRow {
  mail_id: string;
  message_id: string;
  user_id: string;
}

let sequence = 0;

/**
 * Inserts one mail for `userId` straight into the table, so a test controls
 * every column, including shapes no delivery produces today but stored rows
 * carry. Addresses default to one sender outside the domain and the given
 * recipient; pass `from` / `to` lists for the usual cases.
 *
 * ```ts
 * const mail = await insertMail(server, admin.id, {
 *   to: [addr("hello@mail.test")], subject: "Hi", read: false,
 * });
 * ```
 */
export const insertMail = async (
  server: TestServer,
  userId: string,
  mail: MailRow & { from?: AddressValue[]; to?: AddressValue[]; cc?: AddressValue[]; bcc?: AddressValue[] } = {}
): Promise<InsertedMail> => {
  const { from, to, cc, bcc, ...columns } = mail;
  const n = ++sequence;
  const row: Record<string, unknown> = {
    user_id: userId,
    message_id: `<e2e-${n}-${crypto.randomBytes(6).toString("hex")}@fixtures.example>`,
    subject: `Fixture mail ${n}`,
    html: `<p>Fixture body ${n}</p>`,
    text: `Fixture body ${n}`,
    date: new Date(Date.UTC(2026, 0, 1, 0, 0, n)).toISOString(),
  };
  const lists: [string, AddressValue[] | undefined][] = [
    ["from", from ?? [addr("sender@remote.example", "Remote Sender")]],
    ["to", to],
    ["cc", cc],
    ["bcc", bcc],
  ];
  for (const [field, list] of lists) {
    if (!list) continue;
    row[`${field}_address`] = list;
    row[`${field}_text`] = addressText(list);
  }
  if (to && columns.envelope_to === undefined) row.envelope_to = to.map(({ address }) => ({ address }));
  Object.assign(row, columns);

  const names = Object.keys(row).filter((name) => row[name] !== undefined);
  const placeholders = names.map((_, i) => `$${i + 1}`);
  const [inserted] = await server.db.query<InsertedMail>(
    `INSERT INTO mails (${names.join(", ")}) VALUES (${placeholders.join(", ")}) RETURNING *`,
    names.map((name) => row[name])
  );
  return inserted!;
};

/** The current row for a mail, or undefined once it is gone. */
export const readMail = async (server: TestServer, mailId: string) => {
  const [row] = await server.db.query("SELECT * FROM mails WHERE mail_id = $1", [mailId]);
  return row;
};

/** A UUID no row has. */
export const missingId = () => crypto.randomUUID();
