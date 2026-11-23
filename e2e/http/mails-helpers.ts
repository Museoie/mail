/**
 * Shared helpers for the `/api/mails/*` black-box files (`mails-*.e2e.ts`).
 * Nothing here knows which server is under test.
 */
import fs from "fs";
import path from "path";
import {
  ADMIN_PASSWORD,
  addr,
  adminUser,
  createUser,
  insertMail,
  type AddressValue,
  type ApiClient,
  type InsertedMail,
  type MailRow,
  type TestServer,
  type TestUser,
} from "../harness";

export const DOMAIN = "mail.test";

/** `{ status: "failed", message }`, the body of every refusal a route words itself. */
export const failed = (message: string) => ({ status: "failed", message });

export const NOT_LOGGED_IN = failed("Request user is not logged in.");
export const NOT_FOUND = failed("Not found");
export const BAD_REQUEST = { status: "error", message: "Bad Request" };
export const INTERNAL_ERROR = { status: "error", message: "Internal server error" };
export const JSON_TYPE = "application/json; charset=utf-8";

/** A weak ETag over an in-memory JSON body: `W/"<hex byte length>-<27 base64 chars>"`. */
export const BODY_ETAG = /^W\/"[0-9a-f]+-[A-Za-z0-9+/]{27}"$/;
export const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
export const ISO_MS = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/;

/** One signed-in account and the addresses it owns. */
export interface Mailbox {
  id: string;
  username: string;
  /** The domain the account's mail arrives at: `<username>.mail.test`, or `mail.test` for admin. */
  domain: string;
  client: ApiClient;
  /** `<local>@<domain>`; the default local part is `box`. */
  address(local?: string): string;
  /** Inserts a received mail addressed to `address()` unless the row says otherwise. */
  mail(row?: SeedRow): Promise<InsertedMail>;
}

export type SeedRow = MailRow & {
  from?: AddressValue[];
  to?: AddressValue[];
  cc?: AddressValue[];
  bcc?: AddressValue[];
};

let userSequence = 0;

const mailboxOf = (server: TestServer, id: string, username: string, domain: string, client: ApiClient): Mailbox => {
  const address = (local = "box") => `${local}@${domain}`;
  return {
    id,
    username,
    domain,
    client,
    address,
    mail: (row = {}) => {
      const addressed =
        row.to !== undefined || row.to_address !== undefined || row.envelope_to !== undefined
          ? row
          : { to: [addr(address())], ...row };
      return insertMail(server, id, addressed);
    },
  };
};

/**
 * A fresh registered user with its own signed-in client, so a test's rows,
 * counters and allowlist are its own.
 */
export const newMailbox = async (server: TestServer, prefix = "user"): Promise<Mailbox> => {
  const user: TestUser = await createUser(server, `${prefix}${++userSequence}`);
  const client = server.client();
  await client.login(user.username, user.password);
  return mailboxOf(server, user.id, user.username, `${user.username}.${DOMAIN}`, client);
};

/** The built-in admin account with a fresh signed-in client. */
export const adminMailbox = async (server: TestServer): Promise<Mailbox> => {
  const admin = await adminUser(server);
  const client = server.client();
  await client.login("admin", ADMIN_PASSWORD);
  return mailboxOf(server, admin.id, "admin", DOMAIN, client);
};

const iso = (value: unknown) => new Date(value as string | Date).toISOString();

const addressField = (value: unknown, text: unknown) =>
  value === null || value === undefined ? undefined : { value, text: (text as string | null) ?? "" };

/**
 * The list row (`MailHeaderData`) the server answers for a stored mail, built
 * from the inserted row. Fields the server leaves out are left undefined here,
 * so `toEqual` fails on a key that should be absent but came back `null`.
 *
 * `snippet` defaults to the stored text, which is right for a short one-line
 * body; pass `{ snippet: undefined }` or the expected preview otherwise.
 */
export const headerOf = (row: InsertedMail, overrides: Record<string, unknown> = {}): Record<string, unknown> => {
  const references = row.reference_ids as string[] | null | undefined;
  return {
    id: row.mail_id,
    subject: row.subject ?? "",
    date: iso(row.date),
    from: addressField(row.from_address, row.from_text),
    to: addressField(row.to_address, row.to_text),
    cc: addressField(row.cc_address, row.cc_text),
    bcc: addressField(row.bcc_address, row.bcc_text),
    envelopeTo: row.envelope_to ? { value: row.envelope_to, text: "" } : undefined,
    read: row.read,
    saved: row.saved,
    sent: row.sent,
    insight: row.insight ?? null,
    snippet: row.text ? row.text : undefined,
    messageId: row.message_id ? row.message_id : undefined,
    inReplyTo: row.in_reply_to ? row.in_reply_to : undefined,
    references:
      references && references.length
        ? references.length > 3
          ? [references[0], references[references.length - 2], references[references.length - 1]]
          : references
        : undefined,
    ...overrides,
  };
};

/** The subjects of a list of header rows, the usual way to assert an order. */
export const subjects = (headers: Record<string, any>[]): string[] => headers.map((header) => header.subject);
export const ids = (headers: Record<string, any>[]): string[] => headers.map((header) => header.id);

/** A paging cursor as the server encodes one: base64url of `{"date","id"}`. */
export const encodeCursor = (cursor: unknown) => Buffer.from(JSON.stringify(cursor)).toString("base64url");
export const decodeCursor = (cursor: string) =>
  JSON.parse(Buffer.from(cursor, "base64url").toString()) as { date: string; id: string };

/** Writes a stored attachment where the server reads them: `<cwd>/attachments/<id>`. */
export const writeAttachment = (
  server: TestServer,
  id: string,
  content: string | Uint8Array,
  mtime?: Date
): string => {
  const directory = path.join(server.cwd, "attachments");
  fs.mkdirSync(directory, { recursive: true });
  const file = path.join(directory, id);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, content);
  if (mtime) fs.utimesSync(file, mtime, mtime);
  return file;
};

export const attachmentPath = (server: TestServer, id: string) => path.join(server.cwd, "attachments", id);

export interface FormPart {
  name: string;
  /** A text field's value, or a file's bytes. */
  value: string | Uint8Array;
  /** Present for a file part. */
  filename?: string;
  contentType?: string;
  /** Replaces the whole `Content-Disposition` value when a test needs an unusual one. */
  disposition?: string;
}

/**
 * A `multipart/form-data` body built byte for byte, so a test controls the
 * part headers a browser or `FormData` would normalize.
 */
export const multipart = (parts: FormPart[], boundary = "----e2e-mails-boundary-7MA4YWxkTrZu0gW") => {
  const encoder = new TextEncoder();
  const chunks: Uint8Array[] = [];
  for (const part of parts) {
    const disposition =
      part.disposition ??
      `form-data; name="${part.name}"${part.filename === undefined ? "" : `; filename="${part.filename}"`}`;
    let head = `--${boundary}\r\nContent-Disposition: ${disposition}\r\n`;
    if (part.contentType !== undefined) head += `Content-Type: ${part.contentType}\r\n`;
    chunks.push(encoder.encode(`${head}\r\n`));
    chunks.push(typeof part.value === "string" ? encoder.encode(part.value) : part.value);
    chunks.push(encoder.encode("\r\n"));
  }
  chunks.push(encoder.encode(`--${boundary}--\r\n`));
  const body = new Uint8Array(chunks.reduce((total, chunk) => total + chunk.byteLength, 0));
  let offset = 0;
  for (const chunk of chunks) {
    body.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return { body, headers: { "Content-Type": `multipart/form-data; boundary=${boundary}` } };
};

/** Text fields as form parts. */
export const fields = (values: Record<string, string>): FormPart[] =>
  Object.entries(values).map(([name, value]) => ({ name, value }));

/** The newest mail a user holds, by insertion. */
export const latestMail = async (server: TestServer, userId: string): Promise<Record<string, any>> => {
  const [row] = await server.db.query(
    "SELECT * FROM mails WHERE user_id = $1 ORDER BY updated DESC, mail_id DESC LIMIT 1",
    [userId]
  );
  if (!row) throw new Error(`User ${userId} holds no mail`);
  return row;
};

export const countMails = async (server: TestServer, userId: string): Promise<number> => {
  const [row] = await server.db.query<{ n: string }>("SELECT count(*) AS n FROM mails WHERE user_id = $1", [userId]);
  return Number(row!.n);
};

/** Every JSON value type a body field can carry, with a label for the test output. */
export const JSON_VALUES: [string, unknown][] = [
  ["a string", "abc"],
  ["an empty string", ""],
  ["a number", 5],
  ["zero", 0],
  ["true", true],
  ["false", false],
  ["null", null],
  ["an array", ["abc"]],
  ["an empty array", []],
  ["an object", { a: 1 }],
];

export interface RawBody {
  label: string;
  body?: BodyInit;
  headers?: Record<string, string>;
}

/** Request bodies that are not a JSON object, and what the body parser makes of each. */
export const NON_OBJECT_BODIES: (RawBody & { parsed: "empty" | "array" | "bad-request" })[] = [
  { label: "no body at all", parsed: "empty" },
  { label: "an empty JSON body", body: "", headers: { "Content-Type": "application/json" }, parsed: "empty" },
  { label: "a JSON array", body: "[]", headers: { "Content-Type": "application/json" }, parsed: "array" },
  { label: "a JSON string", body: '"abc"', headers: { "Content-Type": "application/json" }, parsed: "bad-request" },
  { label: "a JSON number", body: "5", headers: { "Content-Type": "application/json" }, parsed: "bad-request" },
  { label: "JSON null", body: "null", headers: { "Content-Type": "application/json" }, parsed: "bad-request" },
  { label: "JSON true", body: "true", headers: { "Content-Type": "application/json" }, parsed: "bad-request" },
  { label: "whitespace only", body: "  ", headers: { "Content-Type": "application/json" }, parsed: "bad-request" },
  { label: "truncated JSON", body: '{"mail_id":', headers: { "Content-Type": "application/json" }, parsed: "bad-request" },
  { label: "unquoted keys", body: "{mail_id: 1}", headers: { "Content-Type": "application/json" }, parsed: "bad-request" },
];
