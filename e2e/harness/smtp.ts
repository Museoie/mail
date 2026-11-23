import net from "net";
import tls from "tls";

/** One complete SMTP reply: every `NNN-text` line up to and including the `NNN text` line. */
export interface SmtpReply {
  code: number;
  /** The text of each line, without its code and separator. */
  lines: string[];
  /** `lines` joined with a line feed. */
  text: string;
  /**
   * The reply as it came off the wire, lines joined with a line feed and the
   * final line break dropped: `"250-mx Nice to meet you\n250 SIZE 1"`. This is
   * what a test compares when the exact text matters.
   */
  raw: string;
}

export interface SmtpConnectOptions {
  port: number;
  host?: string;
  /**
   * Implicit TLS: the handshake runs before the greeting (the 465 role). Pass
   * options instead of `true` to pin the handshake, such as `{ maxVersion: "TLSv1.2" }`.
   */
  tls?: boolean | tls.ConnectionOptions;
  /**
   * A PROXY protocol v1 line (without its CRLF) written before anything else,
   * the TLS handshake included: `"PROXY TCP4 10.1.2.3 10.0.0.1 40000 25"`.
   */
  proxy?: string;
  /** Set false to return without waiting for the `220` greeting. */
  greeting?: boolean;
  /** How long any single wait (connect, handshake, reply) may take. Default 10s. */
  timeoutMs?: number;
}

const CLOSED = Symbol("closed");

/** Thrown by a read when the server hangs up instead of answering. */
export class SmtpClosedError extends Error {
  constructor(what: string) {
    super(`The SMTP connection closed while waiting for ${what}`);
    this.name = "SmtpClosedError";
  }
}

/** CRLF line endings and SMTP dot-stuffing, without the terminating dot line. */
export const dotStuff = (message: string): string =>
  message
    .replace(/\r?\n/g, "\r\n")
    .replace(/^\./gm, "..")
    .replace(/(\r\n)?$/, "\r\n");

/** The same for bytes: every line that starts with a dot gets a second one. */
export const dotStuffBytes = (message: Uint8Array): Buffer => {
  const source = Buffer.from(message);
  const out: number[] = [];
  let lineStart = true;
  for (const byte of source) {
    if (lineStart && byte === 0x2e) out.push(0x2e);
    out.push(byte);
    lineStart = byte === 0x0a;
  }
  const stuffed = Buffer.from(out);
  const endsWithCrlf = stuffed.length >= 2 && stuffed[stuffed.length - 2] === 0x0d && stuffed[stuffed.length - 1] === 0x0a;
  return endsWithCrlf ? stuffed : Buffer.concat([stuffed, Buffer.from("\r\n")]);
};

/** `base64("\0user\0password")`, the AUTH PLAIN initial response. */
export const plainToken = (username: string, password: string, authzid = ""): string =>
  Buffer.from(`${authzid}\0${username}\0${password}`).toString("base64");

export const b64 = (text: string): string => Buffer.from(text).toString("base64");

/** A PROXY protocol v1 line naming `source` as the client, without its CRLF. */
export const proxyLine = (
  source: string,
  { sourcePort = 40000, destination, destinationPort = 25 }: { sourcePort?: number; destination?: string; destinationPort?: number } = {}
): string => {
  const v6 = source.includes(":");
  return `PROXY ${v6 ? "TCP6" : "TCP4"} ${source} ${destination ?? (v6 ? "fd00::2" : "10.0.0.2")} ${sourcePort} ${destinationPort}`;
};

/**
 * A scripted SMTP client. Every command is written exactly as given and every
 * reply is returned whole, so a test can pin the server's side of the
 * conversation byte for byte.
 *
 * ```ts
 * const smtp = await SmtpClient.connect({ port: server.ports.smtp });
 * expect(smtp.greeting.code).toBe(220);
 * expect((await smtp.send("EHLO client.example")).code).toBe(250);
 * expect((await smtp.send("MAIL FROM:<a@remote.example>")).raw).toBe("250 Accepted");
 * await smtp.quit();
 * ```
 */
export class SmtpClient {
  /** The `220` greeting, unless `greeting: false` was passed to `connect`. */
  greeting!: SmtpReply;
  /** True once the connection is encrypted, implicitly or through STARTTLS. */
  secure = false;
  /** Every reply read so far, in order. */
  readonly transcript: SmtpReply[] = [];

  private socket: net.Socket;
  private buffer = "";
  private queue: SmtpReply[] = [];
  private waiters: ((reply: SmtpReply | typeof CLOSED) => void)[] = [];
  private isClosed = false;
  private closeWaiters: (() => void)[] = [];
  private readonly timeoutMs: number;

  private constructor(socket: net.Socket, timeoutMs: number) {
    this.socket = socket;
    this.timeoutMs = timeoutMs;
  }

  /** Opens a connection and, unless told not to, reads the greeting. */
  static async connect(options: SmtpConnectOptions): Promise<SmtpClient> {
    const host = options.host ?? "127.0.0.1";
    const timeoutMs = options.timeoutMs ?? 10_000;
    const raw = net.createConnection({ port: options.port, host });
    raw.on("error", () => {});
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => {
        raw.destroy();
        reject(new Error(`Timed out connecting to ${host}:${options.port}`));
      }, timeoutMs);
      raw.once("connect", () => {
        clearTimeout(timer);
        resolve();
      });
      raw.once("error", (error) => {
        clearTimeout(timer);
        reject(error);
      });
    });
    raw.setNoDelay(true);
    if (options.proxy !== undefined) raw.write(`${options.proxy}\r\n`);

    const client = new SmtpClient(raw, timeoutMs);
    if (options.tls) {
      try {
        await client.upgrade(options.tls === true ? {} : options.tls);
      } catch (error) {
        raw.destroy();
        throw error;
      }
    } else client.attach(raw);
    if (options.greeting !== false) client.greeting = await client.read();
    return client;
  }

  private attach(socket: net.Socket) {
    socket.on("data", (chunk: Buffer) => {
      if (socket !== this.socket) return;
      this.buffer += chunk.toString("latin1");
      this.drain();
    });
    const end = () => {
      if (socket !== this.socket || this.isClosed) return;
      this.isClosed = true;
      for (const waiter of this.waiters.splice(0)) waiter(CLOSED);
      for (const waiter of this.closeWaiters.splice(0)) waiter();
    };
    socket.on("close", end);
    socket.on("end", end);
    socket.on("error", () => {});
  }

  private drain() {
    for (;;) {
      const match = /^(?:\d{3}-[^\r\n]*\r\n)*\d{3}(?: [^\r\n]*)?\r\n/.exec(this.buffer);
      if (!match) return;
      this.buffer = this.buffer.slice(match[0].length);
      const wire = Buffer.from(match[0], "latin1").toString("utf8");
      const rawLines = wire.replace(/\r\n$/, "").split("\r\n");
      const lines = rawLines.map((line) => line.slice(4));
      const reply: SmtpReply = {
        code: Number(rawLines[rawLines.length - 1]!.slice(0, 3)),
        lines,
        text: lines.join("\n"),
        raw: rawLines.join("\n"),
      };
      this.transcript.push(reply);
      const waiter = this.waiters.shift();
      if (waiter) waiter(reply);
      else this.queue.push(reply);
    }
  }

  /** Bytes received that do not (yet) form a complete reply. */
  get unparsed(): string {
    return this.buffer;
  }

  /** True once the server has closed the connection (or `close` was called). */
  get closed(): boolean {
    return this.isClosed;
  }

  /** The next reply, or `undefined` if the server hangs up first. */
  readOrClose(timeoutMs = this.timeoutMs): Promise<SmtpReply | undefined> {
    const queued = this.queue.shift();
    if (queued) return Promise.resolve(queued);
    if (this.isClosed) return Promise.resolve(undefined);
    return new Promise((resolve, reject) => {
      const waiter = (reply: SmtpReply | typeof CLOSED) => {
        clearTimeout(timer);
        resolve(reply === CLOSED ? undefined : reply);
      };
      const timer = setTimeout(() => {
        const index = this.waiters.indexOf(waiter);
        if (index >= 0) this.waiters.splice(index, 1);
        reject(new Error(`Timed out after ${timeoutMs}ms waiting for an SMTP reply (unparsed: ${JSON.stringify(this.buffer)})`));
      }, timeoutMs);
      this.waiters.push(waiter);
    });
  }

  /** The next reply; throws if the server hangs up or stays silent. */
  async read(timeoutMs = this.timeoutMs): Promise<SmtpReply> {
    const reply = await this.readOrClose(timeoutMs);
    if (!reply) throw new SmtpClosedError("a reply");
    return reply;
  }

  /** Resolves true if no reply arrives within `quietMs`; for asserting silence. */
  async staysQuiet(quietMs = 300): Promise<boolean> {
    if (this.queue.length) return false;
    await new Promise((resolve) => setTimeout(resolve, quietMs));
    return this.queue.length === 0 && this.buffer === "";
  }

  /** Writes bytes exactly as given: no line ending is added. */
  write(data: string | Uint8Array): void {
    if (this.socket.destroyed || !this.socket.writable) return;
    this.socket.write(data);
  }

  /** Writes `data` and resolves once it has been handed to the kernel. */
  writeFlushed(data: string | Uint8Array): Promise<void> {
    return new Promise((resolve) => {
      if (this.socket.destroyed || !this.socket.writable) return resolve();
      this.socket.write(data, () => resolve());
    });
  }

  /** Sends one command line (CRLF added) and returns its reply. */
  async send(line: string, timeoutMs = this.timeoutMs): Promise<SmtpReply> {
    this.write(`${line}\r\n`);
    return this.read(timeoutMs);
  }

  /** Like `send`, for a command the server may answer by hanging up. */
  async sendOrClose(line: string, timeoutMs = this.timeoutMs): Promise<SmtpReply | undefined> {
    this.write(`${line}\r\n`);
    return this.readOrClose(timeoutMs);
  }

  ehlo(name = "client.remote.example") {
    return this.send(`EHLO ${name}`);
  }

  helo(name = "client.remote.example") {
    return this.send(`HELO ${name}`);
  }

  mailFrom(address: string, parameters = "") {
    return this.send(`MAIL FROM:<${address}>${parameters ? ` ${parameters}` : ""}`);
  }

  rcptTo(address: string, parameters = "") {
    return this.send(`RCPT TO:<${address}>${parameters ? ` ${parameters}` : ""}`);
  }

  /**
   * Sends `DATA`, and on `354` the message (dot-stuffed, CRLF line endings)
   * with its terminator. Returns the reply that ended the exchange: the one to
   * the terminator, or DATA's own refusal.
   */
  async data(message: string | Uint8Array, timeoutMs = 30_000): Promise<SmtpReply> {
    const go = await this.send("DATA");
    if (go.code !== 354) return go;
    if (typeof message === "string") this.write(`${dotStuff(message)}.\r\n`);
    else this.write(Buffer.concat([dotStuffBytes(message), Buffer.from(".\r\n")]));
    return this.read(timeoutMs);
  }

  /**
   * One whole transaction on an open, greeted session: MAIL, each RCPT, DATA.
   * Stops at the first reply that is not 2xx/3xx and returns every reply.
   */
  async transaction(envelope: { from: string; to: string[]; data: string | Uint8Array }): Promise<{
    accepted: boolean;
    replies: SmtpReply[];
    final: SmtpReply;
  }> {
    const replies: SmtpReply[] = [];
    const ok = (reply: SmtpReply) => {
      replies.push(reply);
      return reply.code >= 200 && reply.code < 400;
    };
    const refused = () => ({ accepted: false, replies, final: replies[replies.length - 1]! });
    if (!ok(await this.mailFrom(envelope.from))) return refused();
    for (const recipient of envelope.to) {
      if (!ok(await this.rcptTo(recipient))) return refused();
    }
    const final = await this.data(envelope.data);
    replies.push(final);
    return { accepted: final.code === 250, replies, final };
  }

  /** `AUTH PLAIN <token>` in one line, or with the token after a `334` when `continued`. */
  async authPlain(
    username: string,
    password: string,
    { continued = false, authzid = "" }: { continued?: boolean; authzid?: string } = {}
  ): Promise<SmtpReply> {
    const token = plainToken(username, password, authzid);
    if (!continued) return this.send(`AUTH PLAIN ${token}`);
    const challenge = await this.send("AUTH PLAIN");
    if (challenge.code !== 334) return challenge;
    return this.send(token);
  }

  /** `AUTH LOGIN`, answering the username and password challenges. */
  async authLogin(
    username: string,
    password: string,
    { initial = false }: { initial?: boolean } = {}
  ): Promise<SmtpReply> {
    const first = initial ? await this.send(`AUTH LOGIN ${b64(username)}`) : await this.send("AUTH LOGIN");
    if (first.code !== 334) return first;
    if (initial) return this.send(b64(password));
    const second = await this.send(b64(username));
    if (second.code !== 334) return second;
    return this.send(b64(password));
  }

  /** Sends `STARTTLS` and, on `220`, runs the handshake. Returns STARTTLS's reply. */
  async startTls(options: tls.ConnectionOptions = {}): Promise<SmtpReply> {
    const reply = await this.send("STARTTLS");
    if (reply.code === 220) await this.upgrade(options);
    return reply;
  }

  /**
   * Runs a TLS handshake over the connection as it stands, accepting any
   * certificate. For a test that writes `STARTTLS` itself, to pipeline
   * something behind it. Rejects if the handshake fails.
   */
  async upgrade(options: tls.ConnectionOptions = {}): Promise<void> {
    const plain = this.socket;
    plain.removeAllListeners("data");
    plain.removeAllListeners("close");
    plain.removeAllListeners("end");
    this.buffer = "";
    const secure = tls.connect({ socket: plain, rejectUnauthorized: false, ...options });
    secure.on("error", () => {});
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => {
        secure.destroy();
        reject(new Error("Timed out in the TLS handshake"));
      }, this.timeoutMs);
      secure.once("secureConnect", () => {
        clearTimeout(timer);
        resolve();
      });
      secure.once("error", (error) => {
        clearTimeout(timer);
        reject(error);
      });
      secure.once("close", () => {
        clearTimeout(timer);
        reject(new SmtpClosedError("the TLS handshake"));
      });
    });
    this.socket = secure;
    this.secure = true;
    this.attach(secure);
  }

  /** The negotiated protocol version (`TLSv1.3`), once encrypted. */
  get tlsProtocol(): string | null {
    return this.secure ? (this.socket as tls.TLSSocket).getProtocol() : null;
  }

  /** The certificate the server presented, once encrypted. */
  get peerCertificate(): tls.PeerCertificate | undefined {
    return this.secure ? (this.socket as tls.TLSSocket).getPeerCertificate() : undefined;
  }

  /** Resolves true once the server has closed the connection, false after `timeoutMs`. */
  waitForClose(timeoutMs = this.timeoutMs): Promise<boolean> {
    if (this.isClosed) return Promise.resolve(true);
    return new Promise((resolve) => {
      const done = () => {
        clearTimeout(timer);
        resolve(true);
      };
      const timer = setTimeout(() => {
        const index = this.closeWaiters.indexOf(done);
        if (index >= 0) this.closeWaiters.splice(index, 1);
        resolve(false);
      }, timeoutMs);
      this.closeWaiters.push(done);
    });
  }

  /**
   * Stops reading from the socket, the way a stalled client does, so the
   * server's replies back up; `resume` reads again.
   */
  pause(): void {
    this.socket.pause();
  }

  resume(): void {
    this.socket.resume();
  }

  /** Sends `QUIT`, reads `221` if it comes, and closes. */
  async quit(): Promise<SmtpReply | undefined> {
    if (this.isClosed) return undefined;
    const reply = await this.sendOrClose("QUIT").catch(() => undefined);
    this.close();
    return reply;
  }

  /** Half-closes the connection the way a client that is done writing does. */
  end(): void {
    this.socket.end();
  }

  /** Drops the connection without a word. */
  close(): void {
    this.socket.destroy();
  }
}

/**
 * Opens a connection to `port`, runs `fn`, and closes the connection whatever
 * happens, so a failing assertion never leaks a socket into the next test.
 *
 * ```ts
 * await withSmtp({ port: server.ports.smtp }, async (smtp) => {
 *   await smtp.ehlo();
 *   expect((await smtp.send("NOOP")).raw).toBe("250 OK");
 * });
 * ```
 */
export const withSmtp = async <T>(
  options: SmtpConnectOptions,
  fn: (client: SmtpClient) => Promise<T>
): Promise<T> => {
  const client = await SmtpClient.connect(options);
  try {
    return await fn(client);
  } finally {
    client.close();
  }
};
