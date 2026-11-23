/**
 * TLS clients for the handshake tests of the mail listeners.
 *
 * `sClient` drives `openssl s_client`, which the harness needs anyway, with
 * whatever versions, suites, groups and signature algorithms a test offers.
 *
 * `handshakeWithoutSignatureAlgorithms` is a TLS 1.2 client small enough to
 * send what OpenSSL's cannot be made to: a ClientHello without the
 * `signature_algorithms` extension, as older stacks send. The server then
 * signs its key exchange with SHA-1 (RFC 5246 §7.4.1.4.1). It completes the
 * handshake, ECDHE over X25519 with AES-128-GCM, and reads the server's first
 * line inside it.
 */
import crypto from "crypto";
import net from "net";

export type StartTls = "smtp" | "imap";

export interface SClientOptions {
  /** Speak STARTTLS first, as `s_client -starttls` does. */
  starttls?: StartTls;
  /** The protocol spoken inside TLS, for the command that ends the session. */
  protocol: "smtp" | "imap";
}

export interface SClientResult {
  /** `TLSv1.2` or `TLSv1.3`; undefined when no handshake completed. */
  protocol?: string;
  /** OpenSSL's name of the suite. */
  cipher?: string;
  /** The server's signature: `RSA-PSS/SHA256`, `RSA/SHA1`; undefined when it signed nothing. */
  signature?: string;
  /** The key exchange: `X25519, 253 bits`. */
  serverTempKey?: string;
  /** Whether the session ended by its own command over TLS: QUIT answered 221, LOGOUT answered BYE. */
  roundTrip: boolean;
  /** Everything `s_client` printed. */
  output: string;
}

// OpenSSL 3.2 and later name the signature scheme (`rsa_pkcs1_sha1`) where 3.0 names the key type (`RSA`).
const SCHEME_KEY_TYPES: [RegExp, string][] = [
  [/^rsa_pkcs1_/, "RSA"],
  [/^rsa_pss_/, "RSA-PSS"],
  [/^ecdsa_/, "ECDSA"],
  [/^ed25519$/, "Ed25519"],
  [/^ed448$/, "Ed448"],
];

const keyTypeOf = (signatureType: string | undefined): string | undefined =>
  SCHEME_KEY_TYPES.find(([scheme]) => signatureType && scheme.test(signatureType))?.[1] ?? signatureType;

/**
 * Connects to `port` with `openssl s_client`, offering what `args` set, and
 * ends the session with QUIT or LOGOUT once the handshake is through.
 */
export const sClient = async (port: number, args: string[], options: SClientOptions): Promise<SClientResult> => {
  const command = ["openssl", "s_client", "-connect", `127.0.0.1:${port}`, "-servername", "localhost", "-brief", "-ign_eof", ...args];
  if (options.starttls) command.push("-starttls", options.starttls);
  const child = Bun.spawn(command, { stdin: "pipe", stdout: "pipe", stderr: "pipe" });
  let out = "";
  let err = "";
  let sent = false;
  const quit = options.protocol === "imap" ? "a LOGOUT\r\n" : "QUIT\r\n";
  // An implicit-TLS client waits for the greeting: the SMTP server refuses
  // one that talks first.
  const ready = () => (options.starttls ? err.includes("CONNECTION ESTABLISHED") : /^(220 |\* OK)/m.test(out));
  const pump = async (stream: ReadableStream<Uint8Array>, append: (text: string) => void) => {
    const decoder = new TextDecoder();
    const reader = stream.getReader();
    for (let chunk = await reader.read(); !chunk.done; chunk = await reader.read()) {
      append(decoder.decode(chunk.value, { stream: true }));
      if (!sent && ready()) {
        sent = true;
        child.stdin.write(quit);
        child.stdin.end();
      }
    }
  };
  const timer = setTimeout(() => child.kill(), 20_000);
  await Promise.all([pump(child.stdout, (text) => (out += text)), pump(child.stderr, (text) => (err += text)), child.exited]);
  clearTimeout(timer);
  if (!sent) child.stdin.end();

  const field = (name: string) => err.match(new RegExp(`^${name}: (.+)$`, "m"))?.[1]?.trim();
  const signatureType = keyTypeOf(field("Signature type"));
  const hash = field("Hash used");
  return {
    protocol: field("Protocol version"),
    cipher: field("Ciphersuite"),
    signature: signatureType && hash ? `${signatureType}/${hash}` : undefined,
    serverTempKey: field("Server Temp Key") ?? field("Peer Temp Key"),
    roundTrip: /^(221 |\* BYE )/m.test(out),
    output: `${err}${out}`,
  };
};

// ---------------------------------------------------------------------------
// A TLS 1.2 client without signature_algorithms
// ---------------------------------------------------------------------------

/** What the server sent on a connection, read as it is needed. */
class Wire {
  private buffer = Buffer.alloc(0);
  private wake?: () => void;
  private closed = false;

  constructor(readonly socket: net.Socket) {
    socket.on("data", (chunk: Buffer) => {
      this.buffer = Buffer.concat([this.buffer, chunk]);
      this.wake?.();
    });
    socket.on("close", () => {
      this.closed = true;
      this.wake?.();
    });
    socket.on("error", () => undefined);
  }

  private async more(): Promise<void> {
    if (this.closed) throw new Error("the server closed the connection");
    await new Promise<void>((resolve) => (this.wake = resolve));
  }

  async take(length: number): Promise<Buffer> {
    while (this.buffer.length < length) await this.more();
    const taken = this.buffer.subarray(0, length);
    this.buffer = this.buffer.subarray(length);
    return taken;
  }

  async line(): Promise<string> {
    while (!this.buffer.includes(10)) await this.more();
    const end = this.buffer.indexOf(10) + 1;
    return (await this.take(end)).toString("latin1").trimEnd();
  }
}

const u16 = (value: number) => Buffer.from([value >> 8, value & 0xff]);
const u24 = (value: number) => Buffer.from([value >> 16, (value >> 8) & 0xff, value & 0xff]);
const vector16 = (body: Buffer) => Buffer.concat([u16(body.length), body]);
const extension = (type: number, body: Buffer) => Buffer.concat([u16(type), vector16(body)]);
const handshakeMessage = (type: number, body: Buffer) => Buffer.concat([Buffer.from([type]), u24(body.length), body]);
const record = (type: number, body: Buffer) => Buffer.concat([Buffer.from([type, 3, 3]), u16(body.length), body]);

const hmac = (secret: Buffer, data: Buffer) => crypto.createHmac("sha256", secret).update(data).digest();

/** The TLS 1.2 PRF with SHA-256 (RFC 5246 §5). */
const prf = (secret: Buffer, label: string, seed: Buffer, length: number): Buffer => {
  const labelled = Buffer.concat([Buffer.from(label, "latin1"), seed]);
  const out: Buffer[] = [];
  let a = labelled;
  for (let produced = 0; produced < length; ) {
    a = hmac(secret, a);
    const chunk = hmac(secret, Buffer.concat([a, labelled]));
    out.push(chunk);
    produced += chunk.length;
  }
  return Buffer.concat(out).subarray(0, length);
};

const X25519_SPKI = Buffer.from("302a300506032b656e032100", "hex");
const ECDHE_RSA_AES128_GCM = 0xc02f;
const ECDHE_ECDSA_AES128_GCM = 0xc02b;
const HASHES: Record<number, string> = { 2: "sha1", 4: "sha256", 5: "sha384", 6: "sha512" };
const ALERTS: Record<number, string> = { 40: "handshake_failure", 47: "illegal_parameter", 70: "protocol_version", 80: "internal_error" };

export interface LegacyHandshake {
  /** The suite the server chose. */
  cipher: "ECDHE-RSA-AES128-GCM-SHA256" | "ECDHE-ECDSA-AES128-GCM-SHA256";
  /** The scheme the server signed its key exchange with: `0x0201` is RSA with SHA-1. */
  signatureScheme: number;
  /** The first line the server sent inside TLS: its greeting, or the answer to EHLO / CAPABILITY. */
  firstLine: string;
}

/**
 * A full TLS 1.2 handshake with a ClientHello that has no
 * `signature_algorithms` extension, after STARTTLS when `starttls` is given.
 * Rejects with the server's alert when it refuses the handshake.
 */
export const handshakeWithoutSignatureAlgorithms = async (port: number, starttls?: StartTls): Promise<LegacyHandshake> => {
  const socket = net.createConnection({ host: "127.0.0.1", port });
  const wire = new Wire(socket);
  const timer = setTimeout(() => socket.destroy(), 15_000);
  try {
    await new Promise<void>((resolve, reject) => {
      socket.once("connect", resolve);
      socket.once("error", reject);
    });
    if (starttls === "smtp") {
      await wire.line();
      socket.write("EHLO legacy.example\r\n");
      while (!/^250 /.test(await wire.line()));
      socket.write("STARTTLS\r\n");
      if (!(await wire.line()).startsWith("220 ")) throw new Error("STARTTLS refused");
    } else if (starttls === "imap") {
      await wire.line();
      socket.write("a STARTTLS\r\n");
      if (!(await wire.line()).startsWith("a OK")) throw new Error("STARTTLS refused");
    }

    const transcript: Buffer[] = [];
    const sendHandshake = (type: number, body: Buffer) => {
      const message = handshakeMessage(type, body);
      transcript.push(message);
      socket.write(record(22, message));
    };

    const clientRandom = crypto.randomBytes(32);
    const extensions = Buffer.concat([
      extension(0, vector16(Buffer.concat([Buffer.from([0]), vector16(Buffer.from("localhost"))]))),
      // supported_groups: X25519 for the key exchange; P-256 too, the curve an
      // ECDSA certificate is on.
      extension(10, vector16(Buffer.concat([u16(0x001d), u16(0x0017)]))),
      extension(11, Buffer.from([1, 0])), // ec_point_formats: uncompressed
    ]);
    sendHandshake(
      1,
      Buffer.concat([
        u16(0x0303),
        clientRandom,
        Buffer.from([0]),
        vector16(Buffer.concat([u16(ECDHE_RSA_AES128_GCM), u16(ECDHE_ECDSA_AES128_GCM)])),
        Buffer.from([1, 0]),
        vector16(extensions),
      ])
    );

    // The server's flight, up to ServerHelloDone.
    let pending = Buffer.alloc(0);
    const nextMessage = async (): Promise<{ type: number; body: Buffer }> => {
      while (pending.length < 4 || pending.length < 4 + pending.readUIntBE(1, 3)) {
        const header = await wire.take(5);
        const body = await wire.take(header.readUInt16BE(3));
        if (header[0] === 21) throw new Error(`alert ${ALERTS[body[1]!] ?? body[1]}`);
        if (header[0] !== 22) throw new Error(`unexpected record type ${header[0]}`);
        pending = Buffer.concat([pending, body]);
      }
      const length = pending.readUIntBE(1, 3);
      const message = pending.subarray(0, 4 + length);
      pending = pending.subarray(4 + length);
      transcript.push(message);
      return { type: message[0]!, body: message.subarray(4) };
    };

    let serverRandom: Buffer = Buffer.alloc(0);
    let suite = 0;
    let publicKey: crypto.KeyObject | undefined;
    let serverPoint: Buffer = Buffer.alloc(0);
    let signatureScheme = 0;
    for (;;) {
      const { type, body } = await nextMessage();
      if (type === 2) {
        serverRandom = body.subarray(2, 34);
        suite = body.readUInt16BE(35 + body[34]!);
      } else if (type === 11) {
        const length = body.readUIntBE(3, 3);
        publicKey = new crypto.X509Certificate(body.subarray(6, 6 + length)).publicKey;
      } else if (type === 12) {
        // curve type, named curve, point; then the signature scheme and the signature.
        const params = body.subarray(0, 4 + body[3]!);
        serverPoint = params.subarray(4);
        signatureScheme = body.readUInt16BE(params.length);
        const signature = body.subarray(params.length + 4, params.length + 4 + body.readUInt16BE(params.length + 2));
        const signed = Buffer.concat([clientRandom, serverRandom, params]);
        const hash = HASHES[signatureScheme >> 8];
        if (!hash || !publicKey || !crypto.verify(hash, signed, publicKey, signature)) {
          throw new Error("the ServerKeyExchange signature does not verify");
        }
      } else if (type === 14) {
        break;
      }
    }

    const keys = crypto.generateKeyPairSync("x25519");
    const shared = crypto.diffieHellman({
      privateKey: keys.privateKey,
      publicKey: crypto.createPublicKey({ key: Buffer.concat([X25519_SPKI, serverPoint]), format: "der", type: "spki" }),
    });
    const ownPoint = keys.publicKey.export({ format: "der", type: "spki" }).subarray(-32);
    sendHandshake(16, Buffer.concat([Buffer.from([32]), ownPoint]));
    socket.write(record(20, Buffer.from([1])));

    const master = prf(shared, "master secret", Buffer.concat([clientRandom, serverRandom]), 48);
    const block = prf(master, "key expansion", Buffer.concat([serverRandom, clientRandom]), 40);
    const [clientKey, serverKey, clientIv, serverIv] = [block.subarray(0, 16), block.subarray(16, 32), block.subarray(32, 36), block.subarray(36, 40)];
    const additional = (sequence: bigint, type: number, length: number) => {
      const data = Buffer.alloc(13);
      data.writeBigUInt64BE(sequence);
      data[8] = type;
      data.writeUInt16BE(0x0303, 9);
      data.writeUInt16BE(length, 11);
      return data;
    };
    let sent = 0n;
    const seal = (type: number, plaintext: Buffer) => {
      const explicit = Buffer.alloc(8);
      explicit.writeBigUInt64BE(sent);
      const cipher = crypto.createCipheriv("aes-128-gcm", clientKey, Buffer.concat([clientIv, explicit]));
      cipher.setAAD(additional(sent++, type, plaintext.length));
      const sealed = Buffer.concat([cipher.update(plaintext), cipher.final(), cipher.getAuthTag()]);
      socket.write(record(type, Buffer.concat([explicit, sealed])));
    };
    let received = 0n;
    const open = async (): Promise<{ type: number; plaintext: Buffer }> => {
      const header = await wire.take(5);
      const body = await wire.take(header.readUInt16BE(3));
      if (header[0] === 21 && body.length === 2) throw new Error(`alert ${ALERTS[body[1]!] ?? body[1]}`);
      const sealed = body.subarray(8, body.length - 16);
      const decipher = crypto.createDecipheriv("aes-128-gcm", serverKey, Buffer.concat([serverIv, body.subarray(0, 8)]));
      decipher.setAAD(additional(received++, header[0]!, sealed.length));
      decipher.setAuthTag(body.subarray(body.length - 16));
      return { type: header[0]!, plaintext: Buffer.concat([decipher.update(sealed), decipher.final()]) };
    };
    const digest = () => crypto.createHash("sha256").update(Buffer.concat(transcript)).digest();

    const finished = handshakeMessage(20, prf(master, "client finished", digest(), 12));
    seal(22, finished);
    transcript.push(finished);

    const changeCipherSpec = await wire.take(5);
    if (changeCipherSpec[0] !== 20) throw new Error(`expected ChangeCipherSpec, got record type ${changeCipherSpec[0]}`);
    await wire.take(changeCipherSpec.readUInt16BE(3));
    const serverFinished = await open();
    const expected = handshakeMessage(20, prf(master, "server finished", digest(), 12));
    if (!serverFinished.plaintext.equals(expected)) throw new Error("the server's Finished does not verify");

    // Inside TLS: the greeting comes first on an implicit-TLS port; after
    // STARTTLS the client speaks first.
    if (starttls === "smtp") seal(23, Buffer.from("EHLO legacy.example\r\n"));
    if (starttls === "imap") seal(23, Buffer.from("a CAPABILITY\r\n"));
    let text = "";
    while (!text.includes("\n")) {
      const { type, plaintext } = await open();
      if (type === 23) text += plaintext.toString("latin1");
    }
    return {
      cipher: suite === ECDHE_RSA_AES128_GCM ? "ECDHE-RSA-AES128-GCM-SHA256" : "ECDHE-ECDSA-AES128-GCM-SHA256",
      signatureScheme,
      firstLine: text.slice(0, text.indexOf("\n")).trimEnd(),
    };
  } finally {
    clearTimeout(timer);
    socket.destroy();
  }
};
