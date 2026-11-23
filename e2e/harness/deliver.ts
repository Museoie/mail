import crypto from "crypto";
import net from "net";
import type { TestServer } from "./server";

export interface DeliveryResult {
  /** True when the server answered the end of DATA with a 2xx. */
  accepted: boolean;
  /** Every reply the server sent, one entry per complete reply. */
  replies: string[];
  /** The reply that ended the conversation: to DATA's end, or the first refusal. */
  final: string;
}

/** Reads SMTP replies off a socket, joining the lines of a multi-line reply. */
export class ReplyReader {
  private buffer = "";
  private waiters: ((reply: string | undefined) => void)[] = [];
  private queue: string[] = [];
  private closed = false;

  constructor(socket: net.Socket) {
    socket.on("data", (chunk) => {
      this.buffer += chunk.toString("latin1");
      this.drain();
    });
    const end = () => {
      this.closed = true;
      for (const waiter of this.waiters.splice(0)) waiter(undefined);
    };
    socket.on("close", end);
    socket.on("error", end);
  }

  private drain() {
    for (;;) {
      // A reply is one or more "NNN-text" lines closed by an "NNN text" line.
      const match = /^(?:\d{3}-[^\r\n]*\r?\n)*\d{3}(?: [^\r\n]*)?\r?\n/.exec(this.buffer);
      if (!match) return;
      this.buffer = this.buffer.slice(match[0].length);
      const reply = match[0].replace(/\r?\n$/, "");
      const waiter = this.waiters.shift();
      if (waiter) waiter(reply);
      else this.queue.push(reply);
    }
  }

  /** The next complete reply, or undefined once the server has hung up. */
  next(timeoutMs = 15_000): Promise<string | undefined> {
    const queued = this.queue.shift();
    if (queued !== undefined) return Promise.resolve(queued);
    if (this.closed) return Promise.resolve(undefined);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("Timed out waiting for an SMTP reply")), timeoutMs);
      this.waiters.push((reply) => {
        clearTimeout(timer);
        resolve(reply);
      });
    });
  }
}

/** CRLF line endings and SMTP dot-stuffing, without the terminating dot. */
export const smtpData = (message: string): string =>
  message
    .replace(/\r?\n/g, "\r\n")
    .replace(/^\./gm, "..")
    .replace(/(\r\n)?$/, "\r\n");

/**
 * Delivers one message over plain SMTP the way another mail server would:
 * no authentication, one transaction. Stops at the first reply that is not
 * 2xx/3xx and reports it.
 *
 * ```ts
 * const { accepted } = await deliverMail(server, {
 *   from: "someone@remote.example",
 *   to: ["hello@mail.test"],
 *   data: rfc822({ from: "someone@remote.example", to: "hello@mail.test", subject: "Hi", text: "Hello" }),
 * });
 * ```
 */
export const deliverMail = async (
  server: TestServer,
  delivery: { from: string; to: string[]; data: string; helo?: string }
): Promise<DeliveryResult> => {
  const socket = net.createConnection({ port: server.ports.smtp, host: "127.0.0.1" });
  const reader = new ReplyReader(socket);
  const replies: string[] = [];
  const expect = async (send?: string) => {
    if (send !== undefined) socket.write(send);
    const reply = await reader.next();
    replies.push(reply ?? "(connection closed)");
    return reply !== undefined && /^[23]/.test(reply);
  };

  try {
    const steps = [
      undefined,
      `EHLO ${delivery.helo ?? "client.remote.example"}\r\n`,
      `MAIL FROM:<${delivery.from}>\r\n`,
      ...delivery.to.map((recipient) => `RCPT TO:<${recipient}>\r\n`),
      "DATA\r\n",
    ];
    for (const step of steps) {
      if (!(await expect(step))) {
        return { accepted: false, replies, final: replies[replies.length - 1]! };
      }
    }
    const accepted = await expect(`${smtpData(delivery.data)}.\r\n`);
    const final = replies[replies.length - 1]!;
    socket.write("QUIT\r\n");
    await reader.next().catch(() => undefined);
    return { accepted, replies, final };
  } finally {
    socket.destroy();
  }
};

export interface MessageParts {
  from: string;
  to: string;
  subject?: string;
  text?: string;
  html?: string;
  cc?: string;
  date?: Date;
  messageId?: string;
  /** Extra header lines, such as `In-Reply-To: <id>`. */
  headers?: string[];
}

/** A small RFC 5322 message: text, HTML, or both as multipart/alternative. */
export const rfc822 = (parts: MessageParts): string => {
  const headers = [
    `From: ${parts.from}`,
    `To: ${parts.to}`,
    ...(parts.cc ? [`Cc: ${parts.cc}`] : []),
    `Subject: ${parts.subject ?? "Test message"}`,
    `Date: ${(parts.date ?? new Date()).toUTCString().replace("GMT", "+0000")}`,
    `Message-ID: ${parts.messageId ?? `<${crypto.randomUUID()}@remote.example>`}`,
    "MIME-Version: 1.0",
    ...(parts.headers ?? []),
  ];
  const text = parts.text ?? (parts.html === undefined ? "Hello" : undefined);
  if (text !== undefined && parts.html !== undefined) {
    const boundary = `alt-${crypto.randomBytes(8).toString("hex")}`;
    return [
      ...headers,
      `Content-Type: multipart/alternative; boundary="${boundary}"`,
      "",
      `--${boundary}`,
      "Content-Type: text/plain; charset=utf-8",
      "",
      text,
      `--${boundary}`,
      "Content-Type: text/html; charset=utf-8",
      "",
      parts.html,
      `--${boundary}--`,
      "",
    ].join("\r\n");
  }
  const [type, body] = parts.html !== undefined ? ["text/html", parts.html] : ["text/plain", text!];
  return [...headers, `Content-Type: ${type}; charset=utf-8`, "", body, ""].join("\r\n");
};
