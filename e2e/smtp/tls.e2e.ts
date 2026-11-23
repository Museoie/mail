import { afterAll, afterEach, beforeAll, describe, expect, test } from "bun:test";
import crypto from "crypto";
import fs from "fs";
import net from "net";
import { waitFor, type TestServer } from "../harness";
import { handshakeWithoutSignatureAlgorithms, sClient, type StartTls } from "../harness/handshake";
import { SmtpClient } from "../harness/smtp";
import { HOST, bootAll, closeClients, ehloReply, mailByMessageId, messageId, open, plain, raws, session, type Listener } from "./support";

// `server` has a certificate, so all three listeners are up. `unreadable` is
// configured with certificate paths that do not exist.
let server: TestServer;
let unreadable: TestServer;

const MISSING_CERT = "/nonexistent/e2e-cert.pem";
const MISSING_KEY = "/nonexistent/e2e-key.pem";

beforeAll(async () => {
  [server, unreadable] = (await bootAll({ tls: true }, { env: { SSL_CERTIFICATE: MISSING_CERT, SSL_CERTIFICATE_KEY: MISSING_KEY } })) as [
    TestServer,
    TestServer,
  ];
}, 180_000);

afterEach(closeClients);

afterAll(async () => {
  await Promise.all([server?.stop(), unreadable?.stop()]);
});

const READY = "220 Ready to start TLS";
const NEED_HELO = "503 Error: send HELO/EHLO first";

/** How a bare TCP connection to `port` ends: "connected" or the error code. */
const tcp = (port: number): Promise<string> =>
  new Promise((resolve) => {
    const socket = net.createConnection({ port, host: "127.0.0.1" });
    socket.on("connect", () => {
      socket.destroy();
      resolve("connected");
    });
    socket.on("error", (error: NodeJS.ErrnoException) => resolve(error.code ?? "error"));
  });

describe("what each listener offers", () => {
  test("port 25 and the submission port offer STARTTLS; the implicit-TLS port does not", async () => {
    expect((await (await open(server, "smtp")).ehlo()).raw).toBe(ehloReply({ starttls: true }));
    expect((await (await open(server, "submission")).ehlo()).raw).toBe(ehloReply({ starttls: true }));
    expect((await (await open(server, "smtps")).ehlo()).raw).toBe(ehloReply({ starttls: false }));
  });

  test("every listener greets the same way", async () => {
    for (const listener of ["smtp", "submission", "smtps"] as const) {
      expect((await open(server, listener)).greeting.raw).toBe(`220 ${HOST} ESMTP`);
    }
  });

  test("HELO lists nothing on any of them", async () => {
    for (const listener of ["smtp", "submission", "smtps"] as const) {
      expect((await (await open(server, listener)).helo()).raw).toBe(`250 ${HOST} Nice to meet you, [127.0.0.1]`);
    }
  });
});

describe("implicit TLS", () => {
  test("the handshake comes first, then the greeting", async () => {
    const smtp = await open(server, "smtps");
    expect(smtp.secure).toBe(true);
    expect(smtp.greeting.raw).toBe(`220 ${HOST} ESMTP`);
    expect(smtp.tlsProtocol).toBe("TLSv1.3");
  });

  test("the server presents the configured certificate", async () => {
    const smtp = await open(server, "smtps");
    const configured = new crypto.X509Certificate(fs.readFileSync(server.certificate!.certPath));
    expect(smtp.peerCertificate!.fingerprint256).toBe(configured.fingerprint256);
    expect(smtp.peerCertificate!.subject.CN).toBe("localhost");
  });

  test("TLS 1.2 is accepted, older versions are not", async () => {
    const tls12 = await open(server, "smtps", { tls: { maxVersion: "TLSv1.2" } });
    expect(tls12.tlsProtocol).toBe("TLSv1.2");
    expect(tls12.greeting.raw).toBe(`220 ${HOST} ESMTP`);
    for (const version of ["TLSv1.1", "TLSv1"] as const) {
      const attempt = SmtpClient.connect({ port: server.ports.smtps, tls: { minVersion: "TLSv1", maxVersion: version } });
      await expect(attempt).rejects.toThrow();
    }
  });

  test("a client that speaks cleartext gets no SMTP reply and is disconnected", async () => {
    const smtp = await open(server, "smtps", { tls: false, greeting: false });
    smtp.write("EHLO cleartext.example\r\n");
    expect(await smtp.readOrClose(5000)).toBeUndefined();
    expect(await smtp.waitForClose(5000)).toBe(true);
    // No greeting, no 5xx: at most a TLS alert record.
    expect(smtp.unparsed).not.toMatch(/^\d{3}/);
  });

  test("no greeting is sent before the handshake", async () => {
    const smtp = await open(server, "smtps", { tls: false, greeting: false });
    expect(await smtp.staysQuiet(400)).toBe(true);
    expect(smtp.closed).toBe(false);
  });

  test("STARTTLS inside an encrypted session is refused", async () => {
    const smtp = await session(server, "smtps");
    expect((await smtp.send("STARTTLS")).raw).toBe("503 Error: TLS already active");
    expect((await smtp.send("NOOP")).raw).toBe("250 OK");
  });

  test("a delivery over the implicit-TLS port is stored like any other", async () => {
    const id = messageId("smtps");
    const smtp = await session(server, "smtps");
    const result = await smtp.transaction({ from: "bounce@remote.example", to: ["hello@mail.test"], data: plain({ messageId: id }) });
    expect(raws(result.replies)).toEqual(["250 Accepted", "250 Accepted", "250 OK: message queued"]);
    expect((await mailByMessageId(server, id)).subject).toBe("Test message");
    expect((await smtp.send("QUIT")).raw).toBe("221 Bye");
    expect(await smtp.waitForClose(5000)).toBe(true);
  });
});

describe("STARTTLS", () => {
  for (const listener of ["smtp", "submission"] as const) {
    test(`${listener}: the upgrade works and the session starts over`, async () => {
      const smtp = await session(server, listener);
      expect((await smtp.send("MAIL FROM:<before@remote.example>")).raw).toBe("250 Accepted");
      expect((await smtp.startTls()).raw).toBe(READY);
      expect(smtp.secure).toBe(true);
      expect(smtp.tlsProtocol).toBe("TLSv1.3");
      // Nothing is sent after the handshake until the client speaks.
      expect(await smtp.staysQuiet(200)).toBe(true);

      // The EHLO from before does not count, and neither does the MAIL FROM.
      expect((await smtp.send("MAIL FROM:<after@remote.example>")).raw).toBe(NEED_HELO);
      expect((await smtp.send("RCPT TO:<hello@mail.test>")).raw).toBe(NEED_HELO);
      expect((await smtp.ehlo()).raw).toBe(ehloReply({ starttls: false }));
      expect((await smtp.send("RCPT TO:<hello@mail.test>")).raw).toBe("503 Error: need MAIL command");

      const id = messageId("starttls");
      const result = await smtp.transaction({ from: "after@remote.example", to: ["hello@mail.test"], data: plain({ messageId: id }) });
      expect(result.final.raw).toBe("250 OK: message queued");
      expect((await mailByMessageId(server, id)).envelope_from).toEqual([{ address: "after@remote.example" }]);
    });
  }

  test("a second STARTTLS is refused", async () => {
    const smtp = await session(server);
    await smtp.startTls();
    expect((await smtp.send("STARTTLS")).raw).toBe("503 Error: TLS already active");
    await smtp.ehlo();
    expect((await smtp.send("STARTTLS")).raw).toBe("503 Error: TLS already active");
    expect((await smtp.send("NOOP")).raw).toBe("250 OK");
  });

  test("STARTTLS is taken before EHLO, in any case, and with a parameter", async () => {
    // NOTE: RFC 3207 §4 gives 501 to STARTTLS with a parameter; here it is ignored.
    for (const line of ["STARTTLS", "starttls", "STARTTLS now"]) {
      const smtp = await open(server);
      expect((await smtp.send(line)).raw).toBe(READY);
      await smtp.upgrade();
      expect((await smtp.ehlo()).raw).toBe(ehloReply({ starttls: false }));
    }
  });

  test("the certificate is the same one the implicit-TLS port presents, and TLS 1.2 is accepted", async () => {
    const smtp = await session(server);
    expect((await smtp.startTls({ maxVersion: "TLSv1.2" })).raw).toBe(READY);
    expect(smtp.tlsProtocol).toBe("TLSv1.2");
    const configured = new crypto.X509Certificate(fs.readFileSync(server.certificate!.certPath));
    expect(smtp.peerCertificate!.fingerprint256).toBe(configured.fingerprint256);
  });

  test("commands pipelined behind STARTTLS in cleartext are thrown away", async () => {
    const smtp = await session(server);
    smtp.write("STARTTLS\r\nMAIL FROM:<injected@remote.example>\r\nRCPT TO:<hello@mail.test>\r\n");
    expect((await smtp.read()).raw).toBe(READY);
    await smtp.upgrade();
    // No reply to the injected commands arrives over the encrypted channel.
    expect(await smtp.staysQuiet(300)).toBe(true);
    expect((await smtp.ehlo()).raw).toBe(ehloReply({ starttls: false }));
    // And they left nothing behind: there is no sender.
    expect((await smtp.send("RCPT TO:<hello@mail.test>")).raw).toBe("503 Error: need MAIL command");
  });

  test("commands pipelined ahead of STARTTLS are answered first", async () => {
    const smtp = await open(server);
    smtp.write("EHLO client.remote.example\r\nNOOP\r\nSTARTTLS\r\n");
    expect(raws([await smtp.read(), await smtp.read(), await smtp.read()])).toEqual([ehloReply({ starttls: true }), "250 OK", READY]);
    await smtp.upgrade();
    expect((await smtp.send("NOOP")).raw).toBe("250 OK");
  });

  test("anything but a TLS handshake after the 220 ends the connection", async () => {
    const smtp = await session(server);
    expect((await smtp.send("STARTTLS")).raw).toBe(READY);
    smtp.write("EHLO still.cleartext.example\r\n");
    expect(await smtp.readOrClose(5000)).toBeUndefined();
    expect(await smtp.waitForClose(5000)).toBe(true);
    // The server is still serving others.
    expect((await open(server)).greeting.code).toBe(220);
  });

  test("a client that hangs up instead of shaking hands is simply gone", async () => {
    const smtp = await session(server);
    expect((await smtp.send("STARTTLS")).raw).toBe(READY);
    smtp.close();
    const next = await session(server);
    expect((await next.send("NOOP")).raw).toBe("250 OK");
  });

  test("a larger message survives the encrypted channel intact", async () => {
    const id = messageId("tlsbig");
    const text = `${"The quick brown fox jumps over the lazy dog. ".repeat(20)}\r\n`.repeat(1000);
    const smtp = await session(server, "submission");
    await smtp.startTls();
    await smtp.ehlo();
    const result = await smtp.transaction({ from: "bounce@remote.example", to: ["hello@mail.test"], data: plain({ messageId: id, body: text.slice(0, -2) }) });
    expect(result.final.raw).toBe("250 OK: message queued");
    const mail = await mailByMessageId(server, id);
    expect(mail.text).toBe(text.replace(/\r\n/g, "\n"));
  });
});

// What an old sender's TLS 1.2 stack offers, and what is refused, on every
// listener: implicit TLS (465) and STARTTLS (25, 587). A sender refused here
// does not fall back to cleartext: Postfix defers the mail until it bounces.
describe("the TLS a sending server may offer", () => {
  const LISTENERS: [Listener, StartTls | undefined][] = [
    ["smtps", undefined],
    ["smtp", "smtp"],
    ["submission", "smtp"],
  ];
  const offer = (listener: Listener, starttls: StartTls | undefined, args: string[]) =>
    sClient(server.ports[listener], args, { starttls, protocol: "smtp" });

  // As a Postfix pinned to an old cipher list, or Windows 7's SChannel, offers.
  const CBC_ONLY = ["-tls1_2", "-cipher", "ECDHE-RSA-AES128-SHA:ECDHE-RSA-AES256-SHA:AES128-SHA:AES256-SHA"];
  const RSA_KEY_EXCHANGE_ONLY = ["-tls1_2", "-cipher", "AES256-SHA:AES128-GCM-SHA256"];
  const SHA1_SIGNATURES_ONLY = ["-tls1_2", "-sigalgs", "RSA+SHA1", "-cipher", "ECDHE-RSA-AES128-GCM-SHA256:@SECLEVEL=0"];

  for (const [listener, starttls] of LISTENERS) {
    test(`${listener}: TLS 1.2 with CBC suites only`, async () => {
      const result = await offer(listener, starttls, CBC_ONLY);
      expect([result.protocol, result.cipher, result.roundTrip]).toEqual(["TLSv1.2", "ECDHE-RSA-AES128-SHA", true]);
    });

    test(`${listener}: TLS 1.2 with RSA key exchange only`, async () => {
      const result = await offer(listener, starttls, RSA_KEY_EXCHANGE_ONLY);
      expect([result.protocol, result.cipher, result.signature, result.roundTrip]).toEqual(["TLSv1.2", "AES256-SHA", undefined, true]);
    });

    test(`${listener}: TLS 1.2 with SHA-1 signatures only`, async () => {
      const result = await offer(listener, starttls, SHA1_SIGNATURES_ONLY);
      expect([result.protocol, result.signature, result.roundTrip]).toEqual(["TLSv1.2", "RSA/SHA1", true]);
    });

    test(`${listener}: TLS 1.2 without signature_algorithms is signed for with SHA-1`, async () => {
      const result = await handshakeWithoutSignatureAlgorithms(server.ports[listener], starttls);
      expect(result.cipher).toBe("ECDHE-RSA-AES128-GCM-SHA256");
      expect(result.signatureScheme).toBe(0x0201);
      expect(result.firstLine).toBe(starttls ? `250-${HOST} Nice to meet you, [127.0.0.1]` : `220 ${HOST} ESMTP`);
    });

    test(`${listener}: TLS 1.0 and 1.1 are refused`, async () => {
      for (const version of ["-tls1", "-tls1_1"]) {
        const result = await offer(listener, starttls, [version, "-cipher", "ALL:@SECLEVEL=0"]);
        expect(result.protocol).toBeUndefined();
        expect(result.output).toContain("alert protocol version");
      }
    });
  }

  test("STARTTLS negotiates what the implicit-TLS port negotiates", async () => {
    const offers = [
      [],
      ["-tls1_2"],
      CBC_ONLY,
      RSA_KEY_EXCHANGE_ONLY,
      SHA1_SIGNATURES_ONLY,
      ["-tls1_2", "-cipher", "ECDHE-RSA-CHACHA20-POLY1305:ECDHE-RSA-AES256-GCM-SHA384"],
      ["-tls1_2", "-groups", "P-384:X25519"],
      ["-tls1_2", "-cipher", "ECDHE-RSA-AES128-SHA256:DHE-RSA-AES128-GCM-SHA256:@SECLEVEL=0"],
      ["-tls1_3", "-ciphersuites", "TLS_CHACHA20_POLY1305_SHA256:TLS_AES_128_GCM_SHA256"],
      ["-tls1_3", "-groups", "P-256"],
    ];
    const negotiated = async (listener: Listener, starttls: StartTls | undefined) => {
      const results = [];
      for (const args of offers) {
        const { protocol, cipher, signature, serverTempKey, roundTrip } = await offer(listener, starttls, args);
        results.push({ args: args.join(" "), protocol, cipher, signature, serverTempKey, roundTrip });
      }
      return results;
    };
    const implicit = await negotiated("smtps", undefined);
    // Every offer but the one of suites outside the set is taken.
    expect(implicit.filter((result) => !result.protocol).map((result) => result.args)).toEqual([
      "-tls1_2 -cipher ECDHE-RSA-AES128-SHA256:DHE-RSA-AES128-GCM-SHA256:@SECLEVEL=0",
    ]);
    expect(await negotiated("smtp", "smtp")).toEqual(implicit);
    expect(await negotiated("submission", "smtp")).toEqual(implicit);
  });
});

describe("a certificate that cannot be read", () => {
  test("only port 25 listens, in cleartext, without STARTTLS", async () => {
    const smtp = await session(unreadable);
    expect(smtp.transcript[1]!.raw).toBe(ehloReply({ starttls: false }));
    expect((await smtp.send("STARTTLS")).raw).toBe("454 Error: TLS not available");
    expect(await tcp(unreadable.ports.smtps)).toBe("ECONNREFUSED");
    expect(await tcp(unreadable.ports.submission)).toBe("ECONNREFUSED");
    expect(await tcp(unreadable.ports.smtp)).toBe("connected");
  });

  test("mail is still accepted", async () => {
    const id = messageId("nocert");
    const smtp = await session(unreadable);
    const result = await smtp.transaction({ from: "bounce@remote.example", to: ["hello@mail.test"], data: plain({ messageId: id }) });
    expect(result.final.raw).toBe("250 OK: message queued");
  });

  test("an alarm names the files", async () => {
    const alarm = await waitFor(
      () => unreadable.alarms.alarms.find((entry) => entry.description.startsWith("SMTP is configured for TLS")),
      { what: "the certificate alarm" }
    );
    expect(alarm.title).toBe("🚨 Mail Server Error: TLS certificate not readable");
    expect(alarm.description).toBe(
      "SMTP is configured for TLS but cannot read its certificate, and is serving cleartext only.\n" +
        `**cert:** ${MISSING_CERT}\n**key:** ${MISSING_KEY}`
    );
  });
});
