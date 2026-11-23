/**
 * Shared pieces of the SMTP black-box tests: opening sessions against a test
 * server's listeners, building fixed messages, and reading back what a
 * delivery stored. Not a test file (the runner only picks up `*.e2e.ts`).
 */
import crypto from "crypto";
import fs from "fs";
import os from "os";
import path from "path";
import { sleep, startServer, type ServerOptions, type TestServer } from "../harness";
import { SmtpClient, type SmtpConnectOptions, type SmtpReply } from "../harness/smtp";

/** The name the server greets with: the machine's host name, which the tests share. */
export const HOST = os.hostname();

/** The fixed message ceiling: 35 MiB, advertised as `SIZE` and enforced in DATA. */
export const MAX_MESSAGE_BYTES = 36_700_160;

export type Listener = "smtp" | "smtps" | "submission";

/**
 * `startServer`, tried again when the machine, not the server, got in the way:
 *
 * - a listener could not bind. The harness picks ports by probing for free
 *   ones, and another process (or another server starting at the same moment)
 *   can take one before the server binds it; the server then never becomes
 *   healthy.
 * - PostgreSQL had no connection to spare. Every server under test keeps ten
 *   open, so suites running side by side can reach `max_connections`.
 */
export const boot = async (options: ServerOptions = {}): Promise<TestServer> => {
  let failure: unknown;
  for (const startupTimeoutMs of [20_000, 30_000, 60_000, 60_000]) {
    try {
      return await startServer({ startupTimeoutMs, ...options });
    } catch (error) {
      failure = error;
      const text = `${String(error)} ${(error as { code?: string; errno?: string })?.errno ?? ""}`;
      if (/too many clients|53300/.test(text)) await sleep(3000);
      else if (!/EADDRINUSE|was not healthy/.test(text)) break;
    }
  }
  throw failure;
};

/** Boots one server per option set, one after the other so their port probes cannot collide. */
export const bootAll = async (...optionSets: ServerOptions[]): Promise<TestServer[]> => {
  const servers: TestServer[] = [];
  try {
    for (const options of optionSets) servers.push(await boot(options));
  } catch (error) {
    await Promise.all(servers.map((server) => server.stop().catch(() => undefined)));
    throw error;
  }
  return servers;
};

const clients = new Set<SmtpClient>();

/** Opens a connection to one of the server's SMTP listeners and reads the greeting. */
export const open = async (
  server: TestServer,
  listener: Listener = "smtp",
  options: Partial<SmtpConnectOptions> = {}
): Promise<SmtpClient> => {
  const client = await SmtpClient.connect({
    port: server.ports[listener],
    tls: listener === "smtps",
    ...options,
  });
  clients.add(client);
  return client;
};

/** `open`, then `EHLO`. */
export const session = async (
  server: TestServer,
  listener: Listener = "smtp",
  options: Partial<SmtpConnectOptions> = {}
): Promise<SmtpClient> => {
  const client = await open(server, listener, options);
  const reply = await client.ehlo();
  if (reply.code !== 250) throw new Error(`EHLO was refused: ${reply.raw}`);
  return client;
};

/** Drops every connection `open` made; call from `afterEach`. */
export const closeClients = () => {
  for (const client of clients) client.close();
  clients.clear();
};

/** The EHLO reply the server gives, as `SmtpReply.raw`. */
export const ehloReply = ({
  address = "127.0.0.1",
  auth = true,
  starttls = false,
}: { address?: string; auth?: boolean; starttls?: boolean } = {}): string =>
  [
    `250-${HOST} Nice to meet you, [${address}]`,
    "250-PIPELINING",
    "250-8BITMIME",
    "250-SMTPUTF8",
    ...(auth ? ["250-AUTH LOGIN PLAIN"] : []),
    ...(starttls ? ["250-STARTTLS"] : []),
    `250 SIZE ${MAX_MESSAGE_BYTES}`,
  ].join("\n");

export const raws = (replies: SmtpReply[]): string[] => replies.map((reply) => reply.raw);

/** Joins header and body lines with CRLF and ends the message with one. */
export const crlf = (...lines: string[]): string => `${lines.join("\r\n")}\r\n`;

/** Bytes from text (latin1, so `\xe9` is one octet) and buffers, concatenated. */
export const octets = (...parts: (string | Uint8Array)[]): Buffer =>
  Buffer.concat(parts.map((part) => (typeof part === "string" ? Buffer.from(part, "latin1") : Buffer.from(part))));

let counter = 0;

/** A Message-ID no other test uses, of a fixed length so sizes stay comparable. */
export const messageId = (tag = "m"): string =>
  `<${tag}-${String(++counter).padStart(4, "0")}-${crypto.randomBytes(6).toString("hex")}@remote.example>`;

/** A username no other test in the file uses: a lower-case DNS label. */
export const uniqueName = (prefix: string): string =>
  `${prefix}${crypto.randomBytes(4).toString("hex")}`;

export const FIXED_DATE = "Tue, 03 Mar 2026 10:20:30 +0000";
export const FIXED_DATE_ISO = "2026-03-03T10:20:30.000Z";

export interface Plain {
  from?: string | null;
  to?: string | null;
  subject?: string | null;
  date?: string | null;
  messageId?: string | null;
  /** Extra header lines, written before the blank line. */
  headers?: string[];
  body?: string;
}

/** A small text/plain message with fixed headers; pass `null` to leave one out. */
export const plain = (parts: Plain = {}): string => {
  const header = (name: string, value: string | null | undefined, fallback: string) =>
    value === null ? [] : [`${name}: ${value ?? fallback}`];
  return crlf(
    ...header("From", parts.from, "Remote Sender <sender@remote.example>"),
    ...header("To", parts.to, "hello@mail.test"),
    ...header("Subject", parts.subject, "Test message"),
    ...header("Date", parts.date, FIXED_DATE),
    ...header("Message-ID", parts.messageId, messageId()),
    ...(parts.headers ?? []),
    "",
    parts.body ?? "Hello"
  );
};

/**
 * Runs one transaction on a fresh connection to the plain listener, the way
 * another mail server delivers, and returns every reply after EHLO.
 */
export const deliver = async (
  server: TestServer,
  envelope: { from?: string; to: string[]; data: string | Uint8Array },
  options: Partial<SmtpConnectOptions> = {}
) => {
  const client = await session(server, "smtp", options);
  try {
    return await client.transaction({ from: envelope.from ?? "bounce@remote.example", to: envelope.to, data: envelope.data });
  } finally {
    await client.quit();
    clients.delete(client);
  }
};

export type Row = Record<string, any>;

/**
 * Delivers messages over one long-lived connection and reads back the row each
 * one became. Every message goes to its own address under `domain`, so a
 * fixture needs no Message-ID of its own to be found again.
 *
 * ```ts
 * const mailer = new Mailer(server);
 * const mail = await mailer.store(crlf("Subject: Hi", "", "Hello"));
 * expect(mail.subject).toBe("Hi");
 * ```
 */
export class Mailer {
  private client?: SmtpClient;

  constructor(
    private readonly server: TestServer,
    private readonly domain = "mail.test"
  ) {}

  private async connection(): Promise<SmtpClient> {
    if (!this.client || this.client.closed) {
      this.client = await SmtpClient.connect({ port: this.server.ports.smtp });
      await this.client.ehlo();
    }
    return this.client;
  }

  /** Runs the transaction and returns its final reply and the address it went to. */
  async send(data: string | Uint8Array, { from = "bounce@remote.example" }: { from?: string } = {}) {
    const recipient = `${uniqueName("t")}@${this.domain}`;
    const client = await this.connection();
    const result = await client.transaction({ from, to: [recipient], data });
    return { ...result, recipient };
  }

  /** Delivers `data`, expecting `250`, and returns the stored row. */
  async store(data: string | Uint8Array, options: { from?: string } = {}): Promise<Row> {
    const { final, recipient } = await this.send(data, options);
    if (final.code !== 250) throw new Error(`The delivery was refused: ${final.raw}`);
    const rows = await this.server.db.query("SELECT * FROM mails WHERE envelope_to @> $1", [[{ address: recipient }]]);
    if (rows.length !== 1) throw new Error(`Expected one stored mail for ${recipient}, found ${rows.length}`);
    return rows[0]!;
  }

  close() {
    this.client?.close();
  }
}

export const userIdOf = async (server: TestServer, username: string): Promise<string> => {
  const [row] = await server.db.query<{ user_id: string }>("SELECT user_id FROM users WHERE username = $1", [username]);
  if (!row) throw new Error(`No user named ${username}`);
  return row.user_id;
};

/** Every mail stored for `username`, oldest write first. */
export const mailsOf = (server: TestServer, username: string): Promise<Row[]> =>
  server.db.query(
    `SELECT m.* FROM mails m JOIN users u USING (user_id)
     WHERE u.username = $1 ORDER BY m.sent, m.uid_domain, m.updated`,
    [username]
  );

/** The mails stored under one Message-ID, across users, in username order. */
export const mailsByMessageId = (server: TestServer, id: string): Promise<Row[]> =>
  server.db.query(
    `SELECT m.*, u.username FROM mails m JOIN users u USING (user_id)
     WHERE m.message_id = $1 ORDER BY u.username`,
    [id]
  );

/** The one mail stored under a Message-ID; fails when there is none or several. */
export const mailByMessageId = async (server: TestServer, id: string): Promise<Row> => {
  const rows = await mailsByMessageId(server, id);
  if (rows.length !== 1) throw new Error(`Expected one mail with Message-ID ${id}, found ${rows.length}`);
  return rows[0]!;
};

/** The `mail_mailbox_uid` rows of one mail as `{ mailbox, uid }`, by mailbox name. */
export const mappingsOf = async (server: TestServer, mailId: string): Promise<{ mailbox: string; uid: number }[]> => {
  const rows = await server.db.query<{ mailbox: string; uid: string }>(
    "SELECT mailbox, uid FROM mail_mailbox_uid WHERE mail_id = $1 ORDER BY mailbox",
    [mailId]
  );
  return rows.map((row) => ({ mailbox: row.mailbox, uid: Number(row.uid) }));
};

/** The `mail_uid_counters` rows of a user, as plain values in a stable order. */
export const countersOf = async (server: TestServer, username: string) => {
  const rows = await server.db.query<{ uid_kind: string; uid_scope: string; sent: boolean; last_uid: string }>(
    `SELECT c.uid_kind, c.uid_scope, c.sent, c.last_uid FROM mail_uid_counters c JOIN users u USING (user_id)
     WHERE u.username = $1 ORDER BY c.uid_kind, c.uid_scope, c.sent`,
    [username]
  );
  return rows.map((row) => ({ kind: row.uid_kind, scope: row.uid_scope, sent: row.sent, last: Number(row.last_uid) }));
};

export const countMails = async (server: TestServer): Promise<number> => {
  const [row] = await server.db.query<{ n: string }>("SELECT COUNT(*) AS n FROM mails");
  return Number(row!.n);
};

export const sha256 = (bytes: Uint8Array | string): string =>
  crypto.createHash("sha256").update(bytes).digest("hex");

/** The stored bytes of an attachment, by the id in `attachments[].content.data`. */
export const attachmentBytes = (server: TestServer, id: string): Buffer =>
  fs.readFileSync(path.join(server.cwd, "attachments", id));

/** Names under `<cwd>/attachments`, without the transaction working directory. */
export const attachmentFiles = (server: TestServer): string[] => {
  const dir = path.join(server.cwd, "attachments");
  if (!fs.existsSync(dir)) return [];
  return fs.readdirSync(dir).filter((name) => name !== ".incoming").sort();
};

/** What is left in the per-transaction working directory; empty once DATA has answered. */
export const incomingLeftovers = (server: TestServer): string[] => {
  const dir = path.join(server.cwd, "attachments", ".incoming");
  return fs.existsSync(dir) ? fs.readdirSync(dir) : [];
};

/**
 * A stored attachment as the tests compare it: the JSON the row carries, with
 * the generated file id replaced by facts about the file it names.
 */
export const describeAttachments = (server: TestServer, row: Row) =>
  (row.attachments as { filename: string; contentType: string; size: number; content: { data: string } }[]).map(
    (attachment) => {
      const bytes = attachmentBytes(server, attachment.content.data);
      return {
        filename: attachment.filename,
        contentType: attachment.contentType,
        size: attachment.size,
        fileSize: bytes.length,
        sha256: sha256(bytes),
        keys: Object.keys(attachment).sort().join(","),
        idIsUuid: /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(attachment.content.data),
      };
    }
  );
