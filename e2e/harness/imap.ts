import net from "net";
import tls from "tls";
import type { TestServer } from "./server";

/**
 * A scripted IMAP client for the black-box suite.
 *
 * It works on bytes: everything read off the socket is kept as a latin1
 * string, one character per octet, so a `{n}` literal is exactly `n`
 * characters and a test can assert the precise octets the server sent. Use
 * `utf8()` to spell an expected non-ASCII value and `decodeUtf8()` to read
 * one back.
 *
 * ```ts
 * const imap = await ImapClient.connect(server);
 * await imap.login("admin", ADMIN_PASSWORD);
 * const { untagged, status, text } = await imap.command("SELECT INBOX");
 * await imap.logout();
 * ```
 */

/** The octets of `text` in UTF-8, as a latin1 string (one char per octet). */
export const utf8 = (text: string): string => Buffer.from(text, "utf8").toString("latin1");

/** Reads a latin1 octet string back as UTF-8 text. */
export const decodeUtf8 = (octets: string): string => Buffer.from(octets, "latin1").toString("utf8");

/** Standard base64 of the UTF-8 octets of `text`. */
export const b64 = (text: string | Buffer): string =>
  (typeof text === "string" ? Buffer.from(text, "utf8") : text).toString("base64");

/** The SASL PLAIN initial response for a user and password. */
export const saslPlain = (username: string, password: string, authzid = ""): string =>
  b64(`${authzid}\0${username}\0${password}`);

/** A literal argument of a command, built with `lit()`. */
export interface LiteralArg {
  literal: Buffer;
  /** True sends `{n}` and waits for the continuation; false sends `{n+}` (LITERAL+). */
  sync: boolean;
}

/**
 * A literal to place inside `command([...])`. Synchronizing by default.
 *
 * ```ts
 * await imap.command(["APPEND INBOX ", lit(message)]);
 * await imap.command(["LOGIN ", lit("admin", false), " ", lit(password, false)]);
 * ```
 */
export const lit = (data: string | Buffer, sync = true): LiteralArg => ({
  literal: typeof data === "string" ? Buffer.from(data, "latin1") : data,
  sync,
});

export type ResponseKind = "untagged" | "continuation" | "tagged";

/** One complete server response: a line, with any literals it carries inline. */
export interface ImapResponse {
  kind: ResponseKind;
  /**
   * Every octet of the response except its final CRLF. A literal appears as
   * the server sent it: `{n}`, CRLF, then the `n` octets.
   */
  raw: string;
  /** The literal payloads of this response, in order. */
  literals: string[];
  /** The tag of a tagged response. */
  tag?: string;
  /** `OK`, `NO` or `BAD` (as sent) of a tagged response. */
  status?: string;
  /** What follows the status of a tagged response. */
  text?: string;
}

/** Everything the server sent in answer to one command. */
export interface CommandResult {
  tag: string;
  /** `OK`, `NO` or `BAD`; undefined when the command was left unanswered (`until`). */
  status?: string;
  /** The text after the status, response code included. */
  text?: string;
  /** The complete tagged line, without its CRLF. */
  tagged?: string;
  /** Each untagged response, in order, without its final CRLF. */
  untagged: string[];
  /** Each continuation request (`+ ...`) the server sent, without its CRLF. */
  continuations: string[];
  /** All responses, in the order they arrived. */
  responses: ImapResponse[];
  /** `raw` of every response, in order: untagged, continuations and the tagged line. */
  lines: string[];
}

export class ImapTimeoutError extends Error {}
export class ImapClosedError extends Error {
  constructor(
    message: string,
    /** Responses read before the connection closed. */
    readonly responses: ImapResponse[]
  ) {
    super(message);
  }
}

export interface ConnectOptions {
  /** The port to dial; defaults to `server.ports.imap`, or `imaps` with `tls`. */
  port?: number;
  /** Implicit TLS (the certificate is not verified). */
  tls?: boolean;
  /** Bytes written before anything else, such as a PROXY protocol header. */
  preface?: string | Buffer;
  /** Set false to skip waiting for the greeting. */
  greeting?: boolean;
  /** Default wait for any response, in milliseconds. */
  timeoutMs?: number;
}

const DEFAULT_TIMEOUT_MS = 15_000;

const describe = (responses: ImapResponse[]) =>
  responses.map((response) => response.raw.slice(0, 200)).join("\n") || "(nothing)";

export class ImapClient {
  /** The greeting line, without its CRLF (empty when `greeting: false`). */
  greeting = "";
  /** True once the server (or this client) has closed the connection. */
  closed = false;
  /** Everything received on the connection, undecoded. */
  transcript = "";

  private socket: net.Socket;
  private buffer: Buffer = Buffer.alloc(0);
  private queue: ImapResponse[] = [];
  private wake: (() => void)[] = [];
  private closeWaiters: (() => void)[] = [];
  private tagCounter = 0;
  private parseLiterals = true;
  private readonly timeoutMs: number;
  private socketError: Error | undefined;

  private constructor(socket: net.Socket, timeoutMs: number) {
    this.socket = socket;
    this.timeoutMs = timeoutMs;
    this.attach(socket);
  }

  /**
   * Opens a connection and, unless told otherwise, reads the greeting.
   *
   * ```ts
   * const plain = await ImapClient.connect(server);
   * const secure = await ImapClient.connect(server, { tls: true });
   * ```
   */
  static async connect(server: TestServer | number, options: ConnectOptions = {}): Promise<ImapClient> {
    const port =
      options.port ??
      (typeof server === "number" ? server : options.tls ? server.ports.imaps : server.ports.imap);
    const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    const socket = await new Promise<net.Socket>((resolve, reject) => {
      const onError = (error: Error) => reject(error);
      if (options.tls && options.preface === undefined) {
        const secure = tls.connect({ host: "127.0.0.1", port, rejectUnauthorized: false }, () => {
          secure.off("error", onError);
          resolve(secure);
        });
        secure.once("error", onError);
        return;
      }
      const plain = net.createConnection({ host: "127.0.0.1", port }, () => {
        plain.off("error", onError);
        if (options.preface !== undefined) plain.write(options.preface);
        if (!options.tls) {
          resolve(plain);
          return;
        }
        // A preface (PROXY header) goes out in the clear, then TLS starts.
        const secure = tls.connect({ socket: plain, rejectUnauthorized: false }, () => {
          secure.off("error", onError);
          resolve(secure);
        });
        secure.once("error", onError);
      });
      plain.once("error", onError);
    });
    const client = new ImapClient(socket, timeoutMs);
    if (options.greeting !== false) {
      const greeting = await client.next();
      client.greeting = greeting.raw;
    }
    return client;
  }

  private attach(socket: net.Socket) {
    socket.on("data", (chunk: Buffer) => {
      this.transcript += chunk.toString("latin1");
      this.buffer = this.buffer.length ? Buffer.concat([this.buffer, chunk]) : chunk;
      this.parse();
      this.notify();
    });
    const onClose = () => {
      if (this.socket !== socket) return;
      this.closed = true;
      this.notify();
      for (const waiter of this.closeWaiters.splice(0)) waiter();
    };
    socket.on("close", onClose);
    socket.on("end", onClose);
    socket.on("error", (error: Error) => {
      this.socketError = error;
      onClose();
    });
  }

  private notify() {
    for (const waiter of this.wake.splice(0)) waiter();
  }

  /** Splits the buffer into complete responses, following `{n}` literals. */
  private parse() {
    for (;;) {
      let cursor = 0;
      const literals: string[] = [];
      let complete = false;
      for (;;) {
        const lineEnd = this.buffer.indexOf("\r\n", cursor);
        if (lineEnd === -1) break;
        const line = this.buffer.toString("latin1", cursor, lineEnd);
        const literal = this.parseLiterals ? /\{(\d+)\}$/.exec(line) : null;
        if (!literal) {
          cursor = lineEnd;
          complete = true;
          break;
        }
        const size = Number(literal[1]);
        const start = lineEnd + 2;
        if (this.buffer.length < start + size) break;
        literals.push(this.buffer.toString("latin1", start, start + size));
        cursor = start + size;
      }
      if (!complete) return;
      const raw = this.buffer.toString("latin1", 0, cursor);
      this.buffer = this.buffer.subarray(cursor + 2);
      this.queue.push(ImapClient.classify(raw, literals));
    }
  }

  private static classify(raw: string, literals: string[]): ImapResponse {
    if (raw.startsWith("* ") || raw === "*") return { kind: "untagged", raw, literals };
    if (raw.startsWith("+ ") || raw === "+") return { kind: "continuation", raw, literals };
    const match = /^(\S+) (\S+)(?: ([\s\S]*))?$/.exec(raw);
    return {
      kind: "tagged",
      raw,
      literals,
      tag: match?.[1] ?? raw,
      status: match?.[2],
      text: match?.[3] ?? "",
    };
  }

  /**
   * Treat `{n}` at the end of a response line as text rather than a literal
   * announcement. For commands whose answer echoes such text back.
   */
  setLiteralParsing(enabled: boolean) {
    this.parseLiterals = enabled;
  }

  private waitForData(deadline: number, what: string, seen: ImapResponse[]): Promise<void> {
    return new Promise((resolve, reject) => {
      const remaining = deadline - Date.now();
      if (remaining <= 0) {
        reject(new ImapTimeoutError(`Timed out waiting for ${what}. Received so far:\n${describe(seen)}`));
        return;
      }
      const timer = setTimeout(() => {
        const index = this.wake.indexOf(wake);
        if (index !== -1) this.wake.splice(index, 1);
        reject(
          new ImapTimeoutError(
            `Timed out waiting for ${what}. Received so far:\n${describe(seen)}\nUnparsed: ${JSON.stringify(
              this.buffer.toString("latin1").slice(0, 200)
            )}`
          )
        );
      }, remaining);
      const wake = () => {
        clearTimeout(timer);
        resolve();
      };
      this.wake.push(wake);
    });
  }

  /** The next response from the server, whatever its kind. */
  async next(timeoutMs = this.timeoutMs, what = "a response"): Promise<ImapResponse> {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const response = this.queue.shift();
      if (response) return response;
      if (this.closed) throw new ImapClosedError(`The connection closed while waiting for ${what}`, []);
      await this.waitForData(deadline, what, []);
    }
  }

  /** The next free tag: `A1`, `A2`, ... */
  nextTag(): string {
    return `A${++this.tagCounter}`;
  }

  /** Writes bytes exactly as given: no tag, no CRLF added. */
  write(data: string | Buffer): void {
    this.socket.write(typeof data === "string" ? Buffer.from(data, "latin1") : data);
  }

  /**
   * Reads responses until the tagged completion for `tag`.
   *
   * With `stopAtContinuation`, returns early (no `status`) at the first
   * continuation request, so the caller can answer it.
   */
  async readResult(
    tag: string,
    options: { timeoutMs?: number; stopAtContinuation?: boolean } = {}
  ): Promise<CommandResult> {
    const deadline = Date.now() + (options.timeoutMs ?? this.timeoutMs);
    const result: CommandResult = { tag, untagged: [], continuations: [], responses: [], lines: [] };
    for (;;) {
      const response = this.queue.shift();
      if (!response) {
        if (this.closed) {
          throw new ImapClosedError(
            `The connection closed before the completion of ${tag}. Received:\n${describe(result.responses)}`,
            result.responses
          );
        }
        await this.waitForData(deadline, `the completion of ${tag}`, result.responses);
        continue;
      }
      result.responses.push(response);
      result.lines.push(response.raw);
      if (response.kind === "untagged") result.untagged.push(response.raw);
      else if (response.kind === "continuation") {
        result.continuations.push(response.raw);
        if (options.stopAtContinuation) return result;
      } else if (response.tag === tag) {
        result.status = response.status;
        result.text = response.text;
        result.tagged = response.raw;
        return result;
      } else {
        // A tagged line for some other tag: keep it where a test can see it.
        result.untagged.push(response.raw);
      }
    }
  }

  /**
   * Sends one command under a fresh tag and reads through its tagged
   * completion. `command` is the text after the tag; pass an array to
   * include literals built with `lit()`.
   *
   * A synchronizing literal is sent only after the server's continuation
   * request; if the server answers with a tagged completion instead (a
   * refusal), the rest of the command is not sent and that completion is
   * returned.
   *
   * ```ts
   * const { status, text, untagged } = await imap.command("FETCH 1 (FLAGS)");
   * const append = await imap.command(["APPEND INBOX (\\Seen) ", lit(message)]);
   * ```
   *
   * With `stopAtContinuation`, a continuation request that is not for a
   * literal (AUTHENTICATE, IDLE) ends the read: the result has no `status`.
   */
  async command(
    command: string | (string | LiteralArg)[],
    options: { tag?: string; timeoutMs?: number; stopAtContinuation?: boolean } = {}
  ): Promise<CommandResult> {
    const tag = options.tag ?? this.nextTag();
    const parts = typeof command === "string" ? [command] : command;
    const collected: CommandResult = { tag, untagged: [], continuations: [], responses: [], lines: [] };
    const merge = (partial: CommandResult) => {
      collected.untagged.push(...partial.untagged);
      collected.continuations.push(...partial.continuations);
      collected.responses.push(...partial.responses);
      collected.lines.push(...partial.lines);
      collected.status = partial.status;
      collected.text = partial.text;
      collected.tagged = partial.tagged;
    };

    let pending = `${tag} `;
    for (const part of parts) {
      if (typeof part === "string") {
        pending += part;
        continue;
      }
      if (part.sync) {
        this.write(`${pending}{${part.literal.length}}\r\n`);
        pending = "";
        const partial = await this.readResult(tag, { timeoutMs: options.timeoutMs, stopAtContinuation: true });
        merge(partial);
        if (partial.status !== undefined) return collected;
        this.write(part.literal);
      } else {
        this.write(`${pending}{${part.literal.length}+}\r\n`);
        this.write(part.literal);
        pending = "";
      }
    }
    this.write(`${pending}\r\n`);
    merge(
      await this.readResult(tag, {
        timeoutMs: options.timeoutMs,
        stopAtContinuation: options.stopAtContinuation,
      })
    );
    return collected;
  }

  /**
   * Answers a continuation request with one line (CRLF added) and reads
   * through the tagged completion of `tag`: the second half of
   * `AUTHENTICATE PLAIN` without an initial response.
   *
   * ```ts
   * const first = await imap.command("AUTHENTICATE PLAIN", { stopAtContinuation: true });
   * const done = await imap.respond(first.tag, saslPlain("admin", ADMIN_PASSWORD));
   * ```
   */
  async respond(tag: string, line: string, timeoutMs = this.timeoutMs): Promise<CommandResult> {
    this.write(`${line}\r\n`);
    return this.readResult(tag, { timeoutMs });
  }

  /** Sends `<tag> <text>CRLF` without waiting for anything. Returns the tag. */
  send(text: string, tag = this.nextTag()): string {
    this.write(`${tag} ${text}\r\n`);
    return tag;
  }

  /**
   * Reads responses until `match` accepts one, and returns everything read,
   * the match last. For unsolicited responses, such as during IDLE.
   */
  async waitFor(
    match: RegExp | ((response: ImapResponse) => boolean),
    options: { timeoutMs?: number; what?: string } = {}
  ): Promise<ImapResponse[]> {
    const deadline = Date.now() + (options.timeoutMs ?? this.timeoutMs);
    const what = options.what ?? (match instanceof RegExp ? `a response matching ${match}` : "a matching response");
    const accept = match instanceof RegExp ? (response: ImapResponse) => match.test(response.raw) : match;
    const seen: ImapResponse[] = [];
    for (;;) {
      const response = this.queue.shift();
      if (!response) {
        if (this.closed) {
          throw new ImapClosedError(`The connection closed while waiting for ${what}. Received:\n${describe(seen)}`, seen);
        }
        await this.waitForData(deadline, what, seen);
        continue;
      }
      seen.push(response);
      if (accept(response)) return seen;
    }
  }

  /** The next continuation request (`+ ...`), failing on anything else. */
  async continuation(timeoutMs = this.timeoutMs): Promise<string> {
    const response = await this.next(timeoutMs, "a continuation request");
    if (response.kind !== "continuation") {
      throw new Error(`Expected a continuation request, got: ${response.raw}`);
    }
    return response.raw;
  }

  /**
   * Responses that have already arrived and that nothing has read yet. Does
   * not wait: pair it with a command on the same connection (whose completion
   * proves everything before it was delivered) rather than with a sleep.
   */
  drain(): ImapResponse[] {
    return this.queue.splice(0);
  }

  /** Signs in with LOGIN and fails loudly if the server refuses. */
  async login(username: string, password: string): Promise<CommandResult> {
    const result = await this.command(`LOGIN ${quote(username)} ${quote(password)}`);
    if (result.status !== "OK") throw new Error(`IMAP LOGIN as ${username} failed: ${result.tagged}`);
    return result;
  }

  /**
   * Enters IDLE: sends the command and reads through `+ idling`. Returns the
   * tag (for `done`) and anything untagged that arrived first.
   */
  async idle(): Promise<{ tag: string; result: CommandResult }> {
    const tag = this.send("IDLE");
    const result = await this.readResult(tag, { stopAtContinuation: true });
    return { tag, result };
  }

  /** Ends IDLE with `DONE` and reads through the tagged completion. */
  async done(tag: string): Promise<CommandResult> {
    this.write("DONE\r\n");
    return this.readResult(tag);
  }

  /**
   * Sends STARTTLS and, when the server agrees, upgrades the connection.
   * Returns the server's answer; the connection is encrypted iff `status` is `OK`.
   */
  async startTls(): Promise<CommandResult> {
    const result = await this.command("STARTTLS");
    if (result.status === "OK") await this.upgradeToTls();
    return result;
  }

  /** Runs the TLS handshake on the open connection (after a `STARTTLS` OK). */
  async upgradeToTls(): Promise<void> {
    const plain = this.socket;
    plain.removeAllListeners("data");
    plain.removeAllListeners("close");
    plain.removeAllListeners("end");
    plain.removeAllListeners("error");
    this.buffer = Buffer.alloc(0);
    const secure = await new Promise<tls.TLSSocket>((resolve, reject) => {
      const socket = tls.connect({ socket: plain, rejectUnauthorized: false }, () => {
        socket.off("error", reject);
        resolve(socket);
      });
      socket.once("error", reject);
    });
    this.socket = secure;
    this.attach(secure);
  }

  /** True when the connection is inside TLS. */
  get encrypted(): boolean {
    return this.socket instanceof tls.TLSSocket;
  }

  /**
   * Resolves once the server has closed the connection, with every response
   * that was still unread. Throws if it stays open for `timeoutMs`.
   */
  async waitForClose(timeoutMs = this.timeoutMs): Promise<ImapResponse[]> {
    if (!this.closed) {
      await new Promise<void>((resolve, reject) => {
        const timer = setTimeout(
          () => reject(new ImapTimeoutError(`The connection was still open after ${timeoutMs}ms`)),
          timeoutMs
        );
        this.closeWaiters.push(() => {
          clearTimeout(timer);
          resolve();
        });
      });
    }
    return this.queue.splice(0);
  }

  /** Bytes received that do not yet form a complete response line. */
  get unparsed(): string {
    return this.buffer.toString("latin1");
  }

  /** The error the socket reported, if it closed on one. */
  get error(): Error | undefined {
    return this.socketError;
  }

  /** Sends LOGOUT and waits for the server to close. Never throws. */
  async logout(): Promise<void> {
    if (this.closed) return;
    try {
      const tag = this.send("LOGOUT");
      await this.readResult(tag, { timeoutMs: 5000 });
      await this.waitForClose(5000);
    } catch {
      // Already gone, or not answering: drop the socket.
    }
    this.close();
  }

  /** Drops the connection without a LOGOUT. */
  close(): void {
    this.closed = true;
    this.socket.destroy();
  }

  /** Half-closes the connection (FIN), leaving the read side open. */
  end(): void {
    this.socket.end();
  }
}

/** An IMAP quoted string: `"` and `\` escaped. */
export const quote = (value: string): string => `"${value.replace(/[\\"]/g, "\\$&")}"`;

/**
 * Opens a connection, signs in and (optionally) selects a mailbox.
 *
 * ```ts
 * const imap = await openSession(server, "admin", ADMIN_PASSWORD, "INBOX");
 * ```
 */
export const openSession = async (
  server: TestServer,
  username: string,
  password: string,
  mailbox?: string,
  options: ConnectOptions = {}
): Promise<ImapClient> => {
  const client = await ImapClient.connect(server, options);
  await client.login(username, password);
  if (mailbox !== undefined) {
    const selected = await client.command(`SELECT ${quote(mailbox)}`);
    if (selected.status !== "OK") throw new Error(`SELECT ${mailbox} failed: ${selected.tagged}`);
  }
  return client;
};

/** The first capture group of the first untagged response matching `pattern`. */
export const capture = (result: CommandResult, pattern: RegExp): string | undefined => {
  for (const line of result.untagged) {
    const match = pattern.exec(line);
    if (match) return match[1];
  }
  const tagged = result.tagged === undefined ? null : pattern.exec(result.tagged);
  return tagged?.[1];
};
