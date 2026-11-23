/**
 * Records what the Bun SMTP server (`src/server/lib/smtp/server.ts`) says on
 * the wire, for the Rust server to be held to.
 *
 *   bun src/server-rs/tests/golden/smtp/record.ts
 *
 * Each conversation in `conversations.ts` runs against a fresh `SmtpServer`
 * with the recording hooks below. What the client saw at every step and what
 * the hooks were called with are written to `conversations.json`, next to
 * two tables of pure functions the protocol leans on: `base64.json`
 * (`Buffer.from(text, "base64")`) and `address-command.json`
 * (`parseAddressCommand`). `tests/smtp_golden.rs` replays all three.
 *
 * Re-run it whenever the Bun server's SMTP behaviour changes, and commit the
 * JSON it writes. The TLS conversations use a certificate made on the spot
 * with the `openssl` CLI and thrown away.
 */
import { spawnSync } from "child_process";
import { createHash } from "crypto";
import fs from "fs";
import net from "net";
import os from "os";
import path from "path";
import type { Readable } from "stream";
import tls from "tls";
import { parseAddressCommand } from "../../../../server/lib/smtp/address-command";
import { createSmtpServer, SmtpServer } from "../../../../server/lib/smtp/server";
import type { SmtpAddress, SmtpServerOptions, SmtpSession } from "../../../../server/lib/smtp/types";
import {
  evictSmtpConnections,
  registerSmtpServer,
  unregisterSmtpServer
} from "../../../../server/lib/smtp-registry";
import { addressCommandLines, base64Inputs, conversations, Conversation, ServerSpec, Step } from "./conversations";

const HERE = import.meta.dir;
const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

/** How long a "slow" hook takes: long enough for a script to act while it runs. */
const SLOW_MS = 300;

// The listeners call `listen(port)` with no host, which Bun binds to `::`. On
// a machine without IPv6 that fails, so there a host-less listen gets the
// IPv4 wildcard. Nothing else about the server changes.
const hasIpv6 = await new Promise<boolean>((resolve) => {
  const probe = net.createServer();
  probe.once("error", () => resolve(false));
  probe.listen(0, "::", () => probe.close(() => resolve(true)));
});
if (!hasIpv6) {
  type Listen = (this: net.Server, ...args: unknown[]) => net.Server;
  const original = net.Server.prototype.listen as unknown as Listen;
  const patched: Listen = function (...args) {
    const [port, second] = args;
    if (typeof port === "number" && (second === undefined || typeof second === "function")) {
      return original.call(this, port, "0.0.0.0", ...args.slice(1));
    }
    return original.apply(this, args);
  };
  net.Server.prototype.listen = patched as unknown as typeof net.Server.prototype.listen;
}

const certificate = (() => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "smtp-golden-"));
  try {
    const result = spawnSync(
      "openssl",
      ["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "key.pem", "-out", "cert.pem",
        "-days", "1", "-subj", "/CN=localhost"],
      { cwd: dir, stdio: "ignore" }
    );
    if (result.status !== 0) throw new Error("openssl could not create a certificate");
    return { key: fs.readFileSync(path.join(dir, "key.pem")), cert: fs.readFileSync(path.join(dir, "cert.pem")) };
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
})();

type Json = string | number | boolean | null | Json[] | { [key: string]: Json };

const binary = (data: Buffer) => data.toString("latin1");

const addressJson = (address: SmtpAddress | false): Json =>
  address === false ? false : { address: address.address, args: address.args === false ? false : { ...address.args } };

/** The bytes of a message, or their size and digest when they would bloat the file. */
const dataJson = (data: Buffer): Json =>
  data.length > 2000
    ? { length: data.length, sha256: createHash("sha256").update(data).digest("hex") }
    : binary(data);

/**
 * The recipients of an envelope, or their number and digest when there are
 * many: each `address`, then `;KEY` or `;KEY=value` for its parameters in
 * key order, one recipient per line.
 */
const recipientsJson = (recipients: SmtpAddress[]): Json => {
  if (recipients.length <= 8) return recipients.map(addressJson);
  const lines = recipients.map((recipient) => {
    const args = recipient.args === false ? {} : recipient.args;
    const parameters = Object.keys(args)
      .sort()
      .map((key) => (args[key] === true ? `;${key}` : `;${key}=${args[key]}`));
    return recipient.address + parameters.join("");
  });
  return { length: recipients.length, sha256: createHash("sha256").update(lines.join("\n")).digest("hex") };
};

const refusal = (message: string, responseCode?: number) =>
  Object.assign(new Error(message), responseCode === undefined ? {} : { responseCode });

const localPart = (address: SmtpAddress | false) => (address ? address.address.split("@")[0] : "");

interface Recording {
  calls: { [key: string]: Json }[];
  errors: { [key: string]: Json }[];
  /** Hooks still at work: a conversation is over once this is back to zero. */
  pending: number;
  clientPorts: Set<number>;
  port: number;
}

type StampedSession = SmtpSession & { passwordVersion?: number };

const sessionJson = (session: StampedSession, recording: Recording): Json => ({
  id: /^[0-9a-f]{20}$/.test(session.id) ? "<id>" : session.id,
  remoteAddress: session.remoteAddress,
  remotePort: recording.clientPorts.has(session.remotePort) ? "<port>" : session.remotePort,
  localAddress: session.localAddress,
  localPort: session.localPort === recording.port ? "<port>" : session.localPort,
  secure: session.secure,
  user: session.user ?? null,
  hostNameAppearsAs: session.hostNameAppearsAs ?? null,
  envelope: {
    mailFrom: addressJson(session.envelope.mailFrom),
    rcptTo: recipientsJson(session.envelope.rcptTo),
    bodyType: session.envelope.bodyType,
    smtpUtf8: session.envelope.smtpUtf8,
    requireTLS: session.envelope.requireTLS
  },
  transaction: session.transaction,
  passwordVersion: session.passwordVersion ?? null
});

/**
 * The hooks every conversation runs against. What they answer depends only on
 * what they are called with, so the Rust test can answer the same way: see
 * `RecordingHandler` in `tests/smtp_golden.rs`, which mirrors this function.
 */
const recordingHooks = (
  recording: Recording
): Pick<SmtpServerOptions, "onAuth" | "onMailFrom" | "onRcptTo" | "onData"> => {
  const later = (fn: () => void) => {
    recording.pending++;
    setTimeout(() => {
      recording.pending--;
      fn();
    }, SLOW_MS);
  };
  return {
    onAuth: (auth, session: StampedSession, cb) => {
      recording.calls.push({
        hook: "onAuth",
        method: auth.method,
        username: auth.username,
        password: auth.password,
        session: sessionJson(session, recording)
      });
      const accept = (user: string, version: number) => {
        session.passwordVersion = version;
        cb(null, { user });
      };
      if (auth.username.startsWith("evict") && auth.password === "pass") return accept(auth.username, 1);
      switch (auth.username) {
        case "user":
          return auth.password === "pass" ? accept("user", 3) : cb(null, { user: undefined });
        case "alias":
          return accept("canonical", 7);
        case "slow":
          return later(() => accept("slow", 5));
        case "limited":
          return cb(new Error("Too many failed authentication attempts"));
        case "unavailable":
          return cb(refusal("4.7.0 Temporary authentication failure", 454));
        case "lookup":
          return cb(refusal("4.3.0 Error: temporary lookup failure", 451));
        case "gone":
          return cb(refusal("4.4.2 Credential changed, please re-authenticate", 421));
        case "custom":
          return cb(null, { message: "5.7.8 Custom refusal", responseCode: 534 });
        case "blank":
          return cb(new Error(""));
        default:
          return cb(null, {});
      }
    },

    onMailFrom: (address, session, cb) => {
      recording.calls.push({
        hook: "onMailFrom",
        address: addressJson(address),
        session: sessionJson(session, recording)
      });
      switch (localPart(address)) {
        case "refused":
          return cb(refusal("5.7.1 Sender refused", 550));
        case "nocode":
          return cb(new Error("Sender refused without a code"));
        case "lookup":
          return cb(refusal("4.3.0 Error: temporary lookup failure", 451));
        case "gone":
          return cb(refusal("4.4.2 Credential changed, please re-authenticate", 421));
        case "slow":
          return later(() => cb());
        default:
          return cb();
      }
    },

    onRcptTo: (address, session, cb) => {
      recording.calls.push({
        hook: "onRcptTo",
        address: addressJson(address),
        session: sessionJson(session, recording)
      });
      switch (localPart(address)) {
        case "nobody":
          return cb(refusal("5.1.1 Error: no such mailbox here", 550));
        case "nocode":
          return cb(new Error("Recipient refused without a code"));
        case "probes":
          return cb(refusal("4.7.0 Error: too many unknown recipients, try again later", 451));
        case "gone":
          return cb(refusal("4.4.2 Credential changed, please re-authenticate", 421));
        case "slow":
          return later(() => cb());
        default:
          return cb();
      }
    },

    onData: (stream: Readable, session, cb) => {
      const call: { [key: string]: Json } = {
        hook: "onData",
        session: sessionJson(session, recording),
        data: null,
        ended: null
      };
      recording.calls.push(call);
      const mode = localPart(session.envelope.mailFrom);

      // Refusals that do not wait for the message: it is read out and dropped.
      if (mode === "relay") {
        stream.resume();
        return cb(refusal("Error: relay access denied", 550));
      }
      if (mode === "unconfigured") {
        stream.resume();
        return cb(new Error("Email service not configured"));
      }
      // Never reads and never answers.
      if (mode === "ignore") return;

      recording.pending++;
      const chunks: Buffer[] = [];
      let bytes = 0;
      let refused = false;
      const settle = (ended: boolean) => {
        if (call.ended !== null) return;
        call.ended = ended;
        if (!refused) call.data = dataJson(Buffer.concat(chunks));
        recording.pending--;
      };
      stream.on("data", (chunk: Buffer) => {
        if (refused) return;
        bytes += chunk.length;
        // A ceiling enforced by the hook: refused as soon as it is crossed,
        // the rest read out and dropped.
        if (mode === "toobig" && bytes > 100) {
          refused = true;
          cb(refusal("Error: message exceeds fixed maximum message size 100", 552));
          return;
        }
        chunks.push(chunk);
      });
      stream.on("end", () => {
        settle(true);
        if (refused) return;
        switch (mode) {
          case "notstored":
            return cb(refusal("4.2.1 Error: mailbox unavailable, try again later", 451));
          case "busy":
            return cb(refusal("4.3.2 Error: too many concurrent deliveries, try again later", 421));
          case "parsefail":
            return cb(new Error("Parser failed"));
          case "slowdata":
            return later(() => cb());
          default:
            return cb();
        }
      });
      stream.on("close", () => settle(false));
    }
  };
};

/** One scripted client connection. */
class Client {
  socket: net.Socket;
  private buffer = "";
  private ended = false;
  private waiters: (() => void)[] = [];

  constructor(port: number, recording: Recording) {
    this.socket = net.connect({ port, host: "127.0.0.1", allowHalfOpen: true });
    this.socket.once("connect", () => recording.clientPorts.add(this.socket.localPort!));
    this.attach(this.socket);
  }

  private attach(socket: net.Socket) {
    const wake = () => this.waiters.splice(0).forEach((waiter) => waiter());
    socket.on("data", (chunk: Buffer) => {
      this.buffer += binary(chunk);
      wake();
    });
    const end = () => {
      this.ended = true;
      wake();
    };
    socket.on("end", end);
    socket.on("close", end);
    socket.on("error", () => {});
  }

  connected() {
    return new Promise<void>((resolve) => {
      if (!this.socket.connecting) return resolve();
      this.socket.once("connect", () => resolve());
      this.socket.once("error", () => resolve());
    });
  }

  private wait(deadline: number) {
    return new Promise<void>((resolve) => {
      this.waiters.push(resolve);
      setTimeout(resolve, Math.max(0, Math.min(25, deadline - Date.now())));
    });
  }

  /** A complete reply: its lines up to the one whose code is not followed by a dash. */
  private takeReply(): string | undefined {
    const lines = this.buffer.split("\r\n");
    for (let i = 0; i < lines.length - 1; i++) {
      if (/^\d{3}(?: |$)/.test(lines[i])) {
        this.buffer = lines.slice(i + 1).join("\r\n");
        return lines.slice(0, i + 1).join("\r\n");
      }
    }
    return undefined;
  }

  private takeRest() {
    const rest = this.buffer;
    this.buffer = "";
    return rest;
  }

  async reply(within: number): Promise<string> {
    const deadline = Date.now() + within;
    for (;;) {
      const reply = this.takeReply();
      if (reply !== undefined) return reply;
      if (this.ended) return this.buffer ? `<closed after ${JSON.stringify(this.takeRest())}>` : "<closed>";
      if (Date.now() >= deadline) return "<timeout>";
      await this.wait(deadline);
    }
  }

  /** `count` replies, as their number and a digest: for floods. */
  async replies(count: number, within: number): Promise<string> {
    const deadline = Date.now() + within;
    const hash = createHash("sha256");
    for (let i = 0; i < count; i++) {
      const reply = await this.reply(Math.max(0, deadline - Date.now()));
      if (reply.startsWith("<")) return `${reply} after ${i} replies`;
      hash.update(reply + "\r\n");
    }
    return `${count} replies sha256:${hash.digest("hex")}`;
  }

  async closed(within: number): Promise<string> {
    const deadline = Date.now() + within;
    while (!this.ended && Date.now() < deadline) await this.wait(deadline);
    const rest = this.takeRest();
    const state = this.ended ? "closed" : "open";
    return rest ? `${state} after ${JSON.stringify(rest)}` : state;
  }

  async silence(ms: number): Promise<string> {
    await sleep(ms);
    const rest = this.takeRest();
    if (rest) return `data ${JSON.stringify(rest)}`;
    return this.ended ? "closed" : "silence";
  }

  startTls(): Promise<string> {
    return new Promise((resolve) => {
      const secure = tls.connect({ socket: this.socket, rejectUnauthorized: false });
      secure.once("secureConnect", () => {
        this.socket = secure;
        this.attach(secure);
        resolve("secure");
      });
      secure.once("error", () => resolve("failed"));
      secure.once("close", () => resolve("failed"));
    });
  }
}

const serverDefaults = {
  secure: false,
  tls: false,
  proxyProtocol: false,
  allowInsecureAuth: false,
  name: "mx.test" as string | null,
  size: 1024 * 1024,
  maxClients: 10,
  closeTimeout: 400,
  socketTimeout: null as number | null
};

const runConversation = async (conversation: Conversation) => {
  const spec: Required<ServerSpec> = { ...serverDefaults, ...conversation.server };
  const recording: Recording = { calls: [], errors: [], pending: 0, clientPorts: new Set(), port: 0 };
  const server: SmtpServer = createSmtpServer({
    secure: spec.secure,
    proxyProtocol: spec.proxyProtocol,
    allowInsecureAuth: spec.allowInsecureAuth,
    ...(spec.name === null ? {} : { name: spec.name }),
    ...(spec.tls ? { ...certificate, minVersion: "TLSv1.2" as const } : {}),
    size: spec.size,
    maxClients: spec.maxClients,
    closeTimeout: spec.closeTimeout,
    ...(spec.socketTimeout === null ? {} : { socketTimeout: spec.socketTimeout }),
    ...recordingHooks(recording)
  });
  server.on(
    "error",
    (err: Error & { meta?: { stage?: string; remoteAddress?: string }; remoteAddress?: string }) =>
      recording.errors.push({
        stage: err.meta?.stage ?? null,
        remoteAddress: err.meta?.remoteAddress ?? err.remoteAddress ?? null
      })
  );
  await new Promise<void>((resolve) => server.listen(0, resolve));
  recording.port = (server.address() as net.AddressInfo).port;

  const clients = new Map<string, Client>();
  const client = (step: { on?: string }) => {
    const found = clients.get(step.on ?? "c");
    if (!found) throw new Error(`${conversation.name}: no connection named ${step.on ?? "c"}`);
    return found;
  };
  let registered = false;
  let serverClosed: Promise<void> | undefined;
  let closeResolved = false;

  const observed: Json[] = [];
  for (const step of conversation.steps as Step[]) {
    let seen: Json = null;
    if ("open" in step) {
      const opened = new Client(recording.port, recording);
      clients.set(step.open, opened);
      await opened.connected();
    } else if ("send" in step) {
      client(step).socket.write(Buffer.from(step.send.repeat(step.times ?? 1), "latin1"));
    } else if ("reply" in step) {
      seen = await client(step).reply(step.within ?? 3000);
    } else if ("replies" in step) {
      seen = await client(step).replies(step.replies, step.within ?? 20000);
    } else if ("closed" in step) {
      seen = await client(step).closed(step.within ?? 3000);
    } else if ("silence" in step) {
      seen = await client(step).silence(step.silence);
    } else if ("tls" in step) {
      seen = await client(step).startTls();
    } else if ("end" in step) {
      client(step).socket.end();
    } else if ("destroy" in step) {
      client(step).socket.destroy();
    } else if ("wait" in step) {
      await sleep(step.wait);
    } else if ("evict" in step) {
      if (!registered) registerSmtpServer(server);
      registered = true;
      seen = evictSmtpConnections(step.evict);
    } else if ("server" in step && step.server === "close") {
      serverClosed = new Promise<void>((resolve) => server.close(() => resolve())).then(() => {
        closeResolved = true;
      });
    } else if ("server" in step && step.server === "closed") {
      const deadline = Date.now() + (step.within ?? 3000);
      while (!closeResolved && Date.now() < deadline) await sleep(10);
      seen = closeResolved ? "closed" : "open";
    } else {
      throw new Error(`${conversation.name}: unknown step ${JSON.stringify(step)}`);
    }
    observed.push(seen);
  }

  // The conversation is over: every client goes, and the server is given the
  // time to notice and its hooks the time to finish.
  for (const open of clients.values()) open.socket.destroy();
  const deadline = Date.now() + 3000;
  while ((server.connections.size > 0 || recording.pending > 0) && Date.now() < deadline) await sleep(10);
  await sleep(30);
  if (registered) unregisterSmtpServer(server);
  if (!serverClosed) serverClosed = new Promise<void>((resolve) => server.close(() => resolve()));
  await serverClosed;

  const hostname = os.hostname();
  const withoutHostname = (value: Json): Json =>
    spec.name === null && typeof value === "string" ? value.split(hostname).join("<hostname>") : value;

  return {
    name: conversation.name,
    server: spec,
    steps: conversation.steps,
    observed: observed.map(withoutHostname),
    calls: recording.calls,
    ...(conversation.errors ? { errors: recording.errors } : {})
  };
};

const names = new Set<string>();
for (const conversation of conversations) {
  if (names.has(conversation.name)) throw new Error(`Two conversations are named ${conversation.name}`);
  names.add(conversation.name);
}

const only = process.argv[2];
const selected = only ? conversations.filter((conversation) => conversation.name.includes(only)) : conversations;

// One at a time: client and server share this process's event loop, and a
// flood in one conversation would stretch the timing of another.
const recorded: Awaited<ReturnType<typeof runConversation>>[] = [];
for (const conversation of selected) recorded.push(await runConversation(conversation));

if (only) {
  console.log(JSON.stringify(recorded, null, 2));
  process.exit(0);
}

/** One entry per line, so a change to one reply is a one-line diff. */
const lines = (entries: unknown[], indent: string) =>
  entries.length ? `[\n${entries.map((entry) => `${indent} ${JSON.stringify(entry)}`).join(",\n")}\n${indent}]` : "[]";

const write = (file: string, text: string) => fs.writeFileSync(path.join(HERE, file), text + "\n");

write(
  "conversations.json",
  `[\n${recorded
    .map((entry) => {
      const fields = [
        `"name": ${JSON.stringify(entry.name)}`,
        `"server": ${JSON.stringify(entry.server)}`,
        `"steps": ${lines(entry.steps, "  ")}`,
        `"observed": ${lines(entry.observed, "  ")}`,
        `"calls": ${lines(entry.calls, "  ")}`,
        ...("errors" in entry ? [`"errors": ${lines(entry.errors as unknown[], "  ")}`] : [])
      ];
      return ` {\n  ${fields.join(",\n  ")}\n }`;
    })
    .join(",\n")}\n]`
);
write(
  "base64.json",
  lines(
    base64Inputs.map((input) => ({ input, hex: Buffer.from(input, "base64").toString("hex") })),
    ""
  )
);
write(
  "address-command.json",
  lines(
    addressCommandLines.map(([verb, line]) => {
      const parsed = parseAddressCommand(verb, line);
      return { verb, line, parsed: parsed === false ? false : addressJson(parsed) };
    }),
    ""
  )
);

console.log(
  `Recorded ${recorded.length} conversations, ${base64Inputs.length} base64 inputs, ${addressCommandLines.length} address lines`
);
process.exit(0);
