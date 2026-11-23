/**
 * Shared pieces of the IMAP black-box files: how their servers are started,
 * how mail gets into a mailbox, and the strings every file compares against.
 * Not a test file (no `.e2e.ts`), so the runner never picks it up on its own.
 */
import net from "net";
import {
  deliverMail,
  ReplyReader,
  rfc822,
  startServer,
  type MessageParts,
  type ServerOptions,
  type TestServer,
} from "../harness";
import { ImapClient, type CommandResult, type ImapResponse } from "../harness/imap";
import { message, type FixtureName } from "./messages";

/** CAPABILITY on a TLS connection, and on a plain one when no certificate is configured. */
export const CAPABILITIES =
  "IMAP4rev1 LITERAL+ SASL-IR LOGIN-REFERRALS ID ENABLE IDLE MOVE CONDSTORE AUTH=PLAIN";

/** CAPABILITY on a plain connection of a server that has a certificate. */
export const CAPABILITIES_STARTTLS = `${CAPABILITIES} STARTTLS`;

/** The untagged block every successful SELECT / EXAMINE ends with. */
export const FLAGS_LINE = "* FLAGS (\\Seen \\Flagged \\Deleted \\Draft \\Answered)";
export const PERMANENTFLAGS_LINE =
  "* OK [PERMANENTFLAGS (\\Seen \\Flagged \\Deleted \\Draft \\Answered \\*)] Flags permitted";

/**
 * Starts the server under test in UTC. INTERNALDATE is written in the
 * server's local zone and SEARCH dates are read in it, so the zone has to be
 * fixed for those bytes to be the same on every machine.
 */
export const startImapServer = (options: ServerOptions = {}): Promise<TestServer> =>
  startServer({ ...options, env: { TZ: "UTC", ...options.env } });

/** Lines joined with CRLF, the way a header block or a message is written. */
export const crlf = (...lines: string[]): string => lines.join("\r\n");

export interface DeliveredMail {
  mail_id: string;
  message_id: string;
  uid_domain: number;
  [column: string]: any;
}

/** The stored row of the mail `userId` holds under `messageId`. */
export const mailByMessageId = async (
  server: TestServer,
  messageId: string,
  username?: string
): Promise<DeliveredMail> => {
  const rows = await server.db.query<DeliveredMail>(
    username === undefined
      ? "SELECT * FROM mails WHERE message_id = $1 ORDER BY uid_domain"
      : "SELECT m.* FROM mails m JOIN users u ON u.user_id = m.user_id WHERE m.message_id = $1 AND u.username = $2",
    username === undefined ? [messageId] : [messageId, username]
  );
  if (rows.length !== 1) {
    throw new Error(`Expected one mail with Message-ID ${messageId}, found ${rows.length}`);
  }
  return rows[0]!;
};

/**
 * Delivers one message over SMTP, the way a relay would, and returns the row
 * it was stored as. `data` is the message as it travels; `messageId` is the
 * `Message-ID` it carries, used to find the row.
 */
export const deliverRaw = async (
  server: TestServer,
  delivery: { from?: string; to: string[]; data: string; messageId: string }
): Promise<DeliveredMail> => {
  const result = await deliverMail(server, {
    from: delivery.from ?? "sender@remote.example",
    to: delivery.to,
    data: delivery.data,
  });
  if (!result.accepted) {
    throw new Error(`The server refused a test delivery: ${result.final}\n${result.replies.join("\n")}`);
  }
  return mailByMessageId(server, delivery.messageId);
};

/**
 * Delivers a message given as octets (a latin1 string, one char per octet)
 * over SMTP and returns its stored row. `deliverMail` writes a JS string as
 * UTF-8, which cannot carry a raw 8-bit body in another charset; this writes
 * the bytes as they are.
 */
export const deliverOctets = async (
  server: TestServer,
  delivery: { from?: string; to: string[]; data: string; messageId: string; username?: string }
): Promise<DeliveredMail> => {
  const socket = net.createConnection({ port: server.ports.smtp, host: "127.0.0.1" });
  const reader = new ReplyReader(socket);
  const step = async (bytes: string | undefined, what: string) => {
    if (bytes !== undefined) socket.write(Buffer.from(bytes, "latin1"));
    const reply = await reader.next(30_000);
    if (reply === undefined || !/^[23]/.test(reply)) {
      throw new Error(`The server refused a test delivery at ${what}: ${reply ?? "(connection closed)"}`);
    }
  };
  try {
    await step(undefined, "the greeting");
    await step("EHLO client.remote.example\r\n", "EHLO");
    await step(`MAIL FROM:<${delivery.from ?? "sender@remote.example"}>\r\n`, "MAIL FROM");
    for (const recipient of delivery.to) await step(`RCPT TO:<${recipient}>\r\n`, "RCPT TO");
    await step("DATA\r\n", "DATA");
    const stuffed = delivery.data.replace(/^\./gm, "..").replace(/(\r\n)?$/, "\r\n");
    await step(`${stuffed}.\r\n`, "the end of DATA");
    socket.write("QUIT\r\n");
  } finally {
    socket.destroy();
  }
  return mailByMessageId(server, delivery.messageId, delivery.username);
};

/** The mail domain of an account: `<name>.<domain>`, or the bare domain for the admin. */
export const mailDomain = (server: TestServer, username: string): string =>
  username === "admin" ? server.domain : `${username}.${server.domain}`;

/**
 * Delivers the fixture `name` from `messages.ts` to `hello@` the account of
 * `username`, and returns its stored row.
 */
export const deliverFixture = (
  server: TestServer,
  name: FixtureName,
  username: string
): Promise<DeliveredMail> => {
  const domain = mailDomain(server, username);
  const fixture = message(name, domain);
  return deliverOctets(server, {
    from: "carol@remote.example",
    to: [`hello@${domain}`],
    data: fixture.data,
    messageId: fixture.messageId,
    username,
  });
};

let counter = 0;

/**
 * Delivers a small message built by `rfc822` and returns its stored row. The
 * `Message-ID` and `Date` default to fixed, distinct values so what IMAP
 * serves for the message is the same on every run.
 *
 * ```ts
 * const mail = await deliver(server, { to: "hello@mail.test", subject: "Hi", text: "Hello" });
 * ```
 */
export const deliver = async (
  server: TestServer,
  parts: Partial<MessageParts> & { to: string; rcpt?: string[]; envelopeFrom?: string }
): Promise<DeliveredMail> => {
  const n = ++counter;
  const { rcpt, envelopeFrom, ...message } = parts;
  const messageId = message.messageId ?? `<fixture-${n}-${process.pid}@remote.example>`;
  const data = rfc822({
    from: "Sender <sender@remote.example>",
    // One minute apart, so a later delivery always carries a later Date.
    date: new Date(Date.UTC(2026, 0, 1, 12, 0, 0) + n * 60_000),
    ...message,
    messageId,
  });
  return deliverRaw(server, { from: envelopeFrom, to: rcpt ?? [parts.to], data, messageId });
};

/** The UIDVALIDITY the server has stored for `username` (it is set on first use). */
export const storedUidValidity = async (server: TestServer, username: string): Promise<number> => {
  const [row] = await server.db.query<{ imap_uid_validity: number | null }>(
    "SELECT imap_uid_validity FROM users WHERE username = $1",
    [username]
  );
  if (!row || row.imap_uid_validity === null) {
    throw new Error(`No UIDVALIDITY is stored for ${username} yet`);
  }
  return Number(row.imap_uid_validity);
};

/** `raw` of each response in a list. */
export const raws = (responses: ImapResponse[]): string[] => responses.map((response) => response.raw);

/** Writes bytes as given and returns the next response line. */
export const exchange = async (client: ImapClient, bytes: string): Promise<string> => {
  client.write(bytes);
  return (await client.next()).raw;
};

/** The number in the first `* <n> EXISTS` of a result. */
export const existsCount = (result: CommandResult): number | undefined => {
  for (const line of result.untagged) {
    const match = /^\* (\d+) EXISTS$/.exec(line);
    if (match) return Number(match[1]);
  }
  return undefined;
};

/** The value of a `[CODE n]` response code among a result's untagged lines. */
export const responseCode = (result: CommandResult, code: string): number | undefined => {
  for (const line of [...result.untagged, result.tagged ?? ""]) {
    const match = new RegExp(`\\[${code} (\\d+)\\]`).exec(line);
    if (match) return Number(match[1]);
  }
  return undefined;
};

/**
 * Sends one mail through the HTTP API as `user`, which is how a row lands in
 * the sent views (`Sent Messages`, `Sent Messages/accounts/<sender>`).
 * `sender` is the local part the mail is sent from.
 */
export const sendOverHttp = async (
  server: TestServer,
  user: { username: string; password: string },
  mail: { sender: string; to: string; subject: string; html: string; senderFullName?: string; cc?: string }
): Promise<void> => {
  const http = server.client();
  await http.login(user.username, user.password);
  const { body, text } = await http.post("/api/mails/send", { senderFullName: "", ...mail });
  if (body?.status !== "success") throw new Error(`Sending a test mail over HTTP failed: ${text}`);
};

/** Closes every client, ignoring the ones already gone. */
export const closeAll = (...clients: (ImapClient | undefined)[]): void => {
  for (const client of clients) client?.close();
};
