import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import net from "net";
import tls from "tls";
import { ADMIN_PASSWORD, createUser, freePort, sleep, type TestServer, type TestUser } from "../harness";
import { handshakeWithoutSignatureAlgorithms, sClient, type StartTls } from "../harness/handshake";
import { ImapClient, saslPlain } from "../harness/imap";
import { CAPABILITIES, CAPABILITIES_STARTTLS, raws, startImapServer } from "./support";

// `secure` has a certificate: STARTTLS on the plain port, implicit TLS on the
// other. `proxied` is the same behind the PROXY protocol. The two `sso`
// servers have single sign-on configured (against a provider nothing answers
// for), which is what turns plain-text sign-in off.
let secure: TestServer;
let proxied: TestServer;
let sso: TestServer;
let ssoLocal: TestServer;
let ssoNoCertificate: TestServer;
let alice: TestUser;

const LOGINDISABLED = CAPABILITIES.replace("AUTH=PLAIN", "LOGINDISABLED");

beforeAll(async () => {
  const unreachable = `http://127.0.0.1:${await freePort()}`;
  const ssoEnv = {
    AUTH_ISSUER: "https://auth.example.test",
    AUTH_CLIENT_ID: "mail",
    AUTH_CLIENT_SECRET: "test_key_not_real_do_not_use",
    AUTH_BACKCHANNEL_URL: unreachable,
  };
  [secure, proxied, sso, ssoLocal, ssoNoCertificate] = await Promise.all([
    startImapServer({ tls: true }),
    startImapServer({ tls: true, env: { PROXY_PROTOCOL: "true" } }),
    startImapServer({ tls: true, env: ssoEnv }),
    startImapServer({ tls: true, env: { ...ssoEnv, AUTH_LOCAL_LOGIN: "true" } }),
    startImapServer({ env: ssoEnv }),
  ]);
  alice = await createUser(secure, "alice");
  await createUser(sso, "alice");
  await createUser(ssoLocal, "alice");
}, 120_000);

afterAll(async () => {
  await Promise.all([secure, proxied, sso, ssoLocal, ssoNoCertificate].map((server) => server?.stop()));
});

describe("the implicit TLS port", () => {
  test("greets inside TLS, without STARTTLS in its capabilities", async () => {
    const imap = await ImapClient.connect(secure, { tls: true });
    expect(imap.encrypted).toBe(true);
    expect(imap.greeting).toBe(`* OK [CAPABILITY ${CAPABILITIES}] IMAP4rev1 Service Ready`);
    expect((await imap.command("CAPABILITY")).lines).toEqual([`* CAPABILITY ${CAPABILITIES}`, "A1 OK CAPABILITY completed"]);
    imap.close();
  });

  test("serves a whole session", async () => {
    const imap = await ImapClient.connect(secure, { tls: true });
    expect((await imap.command(`LOGIN alice ${alice.password}`)).lines).toEqual([
      `A1 OK [CAPABILITY ${CAPABILITIES}] LOGIN completed`,
    ]);
    expect((await imap.command("SELECT INBOX")).tagged).toBe("A2 OK [READ-WRITE] SELECT completed");
    const { tag } = await imap.idle();
    expect((await imap.done(tag)).lines).toEqual([`${tag} OK IDLE terminated`]);
    expect((await imap.command("LOGOUT")).lines).toEqual(["* BYE IMAP4rev1 Server logging out", "A4 OK LOGOUT completed"]);
    await imap.waitForClose();
  });

  test("refuses STARTTLS, before and after login", async () => {
    const imap = await ImapClient.connect(secure, { tls: true });
    expect((await imap.command("STARTTLS")).lines).toEqual(["A1 BAD STARTTLS not permitted on a TLS connection"]);
    await imap.login("alice", alice.password);
    expect((await imap.command("STARTTLS")).lines).toEqual(["A3 BAD STARTTLS not permitted on a TLS connection"]);
    imap.close();
  });

  test("drops a client that speaks plain text to it", async () => {
    const socket = net.createConnection({ host: "127.0.0.1", port: secure.ports.imaps });
    const received: Buffer[] = [];
    socket.on("data", (chunk: Buffer) => received.push(chunk));
    socket.on("error", () => undefined);
    const closed = new Promise<void>((resolve) => socket.on("close", () => resolve()));
    socket.write("A1 NOOP\r\n");
    await closed;
    // Whatever came back is a TLS alert, not an IMAP line.
    expect(Buffer.concat(received).toString("latin1")).not.toContain("OK");
  });

  test("is not opened without a certificate", async () => {
    const plain = await startImapServer();
    try {
      const error = await new Promise<NodeJS.ErrnoException | undefined>((resolve) => {
        const socket = net.createConnection({ host: "127.0.0.1", port: plain.ports.imaps });
        socket.on("connect", () => {
          socket.destroy();
          resolve(undefined);
        });
        socket.on("error", (e) => resolve(e));
      });
      expect(error?.code).toBe("ECONNREFUSED");
    } finally {
      await plain.stop();
    }
  });
});

describe("STARTTLS on the plain port", () => {
  test("the greeting and CAPABILITY advertise STARTTLS when a certificate is configured", async () => {
    const imap = await ImapClient.connect(secure);
    expect(imap.encrypted).toBe(false);
    expect(imap.greeting).toBe(`* OK [CAPABILITY ${CAPABILITIES_STARTTLS}] IMAP4rev1 Service Ready`);
    expect((await imap.command("CAPABILITY")).untagged).toEqual([`* CAPABILITY ${CAPABILITIES_STARTTLS}`]);
    imap.close();
  });

  test("plain-text sign-in is allowed too, and repeats STARTTLS in its capability code", async () => {
    const imap = await ImapClient.connect(secure);
    expect((await imap.command(`LOGIN alice ${alice.password}`)).lines).toEqual([
      `A1 OK [CAPABILITY ${CAPABILITIES_STARTTLS}] LOGIN completed`,
    ]);
    imap.close();
  });

  test("answers OK in the clear, then speaks TLS: no second greeting, and STARTTLS is gone from the capabilities", async () => {
    const imap = await ImapClient.connect(secure);
    const result = await imap.command("STARTTLS");
    expect(result.lines).toEqual(["A1 OK Begin TLS negotiation now"]);
    expect(imap.transcript.endsWith("A1 OK Begin TLS negotiation now\r\n")).toBe(true);
    await imap.upgradeToTls();
    expect(imap.encrypted).toBe(true);
    // The first thing inside TLS is the answer to the client's first command.
    expect((await imap.command("CAPABILITY")).lines).toEqual([`* CAPABILITY ${CAPABILITIES}`, "A2 OK CAPABILITY completed"]);
    expect((await imap.command(`LOGIN alice ${alice.password}`)).lines).toEqual([
      `A3 OK [CAPABILITY ${CAPABILITIES}] LOGIN completed`,
    ]);
    expect((await imap.command("SELECT INBOX")).status).toBe("OK");
    imap.close();
  });

  test("ignores arguments, and is matched in any letter case", async () => {
    const imap = await ImapClient.connect(secure);
    expect((await imap.command("starttls extra args")).lines).toEqual(["A1 OK Begin TLS negotiation now"]);
    await imap.upgradeToTls();
    expect((await imap.command("NOOP")).status).toBe("OK");
    imap.close();
  });

  test("a second STARTTLS inside TLS is BAD", async () => {
    const imap = await ImapClient.connect(secure);
    expect((await imap.startTls()).status).toBe("OK");
    expect((await imap.command("STARTTLS")).lines).toEqual(["A2 BAD STARTTLS not permitted on a TLS connection"]);
    imap.close();
  });

  test("is BAD once signed in", async () => {
    const imap = await ImapClient.connect(secure);
    await imap.login("alice", alice.password);
    expect((await imap.command("STARTTLS")).lines).toEqual(["A2 BAD STARTTLS not permitted after authentication"]);
    // The connection carries on in the clear.
    expect((await imap.command("NOOP")).status).toBe("OK");
    expect(imap.encrypted).toBe(false);
    imap.close();
  });

  test("nothing learned before the upgrade carries over: what was enabled is enabled anew", async () => {
    const imap = await ImapClient.connect(secure);
    expect((await imap.command("ENABLE CONDSTORE")).untagged).toEqual(["* ENABLED CONDSTORE"]);
    await imap.startTls();
    expect((await imap.command("ENABLE CONDSTORE")).untagged).toEqual(["* ENABLED CONDSTORE"]);
    imap.close();
  });

  test("an unfinished AUTHENTICATE exchange does not survive the upgrade", async () => {
    const imap = await ImapClient.connect(secure);
    const first = await imap.command("AUTHENTICATE PLAIN", { stopAtContinuation: true });
    expect(first.lines).toEqual(["+ "]);
    // The line after the challenge is the SASL response, so cancel, then upgrade.
    expect((await imap.respond(first.tag, "*")).lines).toEqual([`${first.tag} BAD Authentication cancelled`]);
    await imap.startTls();
    expect((await imap.command("NOOP")).lines).toEqual(["A3 OK NOOP completed"]);
    imap.close();
  });

  test("commands pipelined behind STARTTLS in the clear are discarded, never answered", async () => {
    const imap = await ImapClient.connect(secure);
    imap.write(`S1 STARTTLS\r\nS2 NOOP\r\nS3 LOGIN alice ${alice.password}\r\n`);
    expect((await imap.readResult("S1")).lines).toEqual(["S1 OK Begin TLS negotiation now"]);
    await imap.upgradeToTls();
    // The injected LOGIN did not run: the session inside TLS is not signed in.
    const list = await imap.command('LIST "" "*"');
    expect(list.lines).toEqual(["A1 NO Not authenticated."]);
    imap.close();
  });

  test("a client that sends plain text instead of a handshake is disconnected", async () => {
    const imap = await ImapClient.connect(secure);
    expect((await imap.command("STARTTLS")).status).toBe("OK");
    imap.write("A2 NOOP\r\n");
    expect(raws(await imap.waitForClose())).toEqual([]);
  });

  test("a TLS handshake sent without STARTTLS is read as commands", async () => {
    const error = await new Promise<Error | undefined>((resolve) => {
      const socket = tls.connect({ host: "127.0.0.1", port: secure.ports.imap, rejectUnauthorized: false }, () => {
        socket.destroy();
        resolve(undefined);
      });
      socket.on("error", (e) => resolve(e));
    });
    // The server answered the ClientHello with its IMAP greeting, which is not TLS.
    expect(error).toBeInstanceOf(Error);
  });

  test("without a certificate STARTTLS is not advertised and is refused with NO", async () => {
    const plain = await startImapServer();
    try {
      const imap = await ImapClient.connect(plain);
      expect(imap.greeting).toBe(`* OK [CAPABILITY ${CAPABILITIES}] IMAP4rev1 Service Ready`);
      expect((await imap.command("STARTTLS")).lines).toEqual(["A1 NO STARTTLS is not available"]);
      // The connection is still usable in the clear.
      expect((await imap.command(`LOGIN admin ${ADMIN_PASSWORD}`)).status).toBe("OK");
      expect((await imap.command("STARTTLS")).lines).toEqual(["A3 BAD STARTTLS not permitted after authentication"]);
      imap.close();
    } finally {
      await plain.stop();
    }
  });
});

// What an old mail client's TLS 1.2 stack offers, and what is refused, on
// both ports. Outlook on Windows 7 and 8 offers CBC suites only.
describe("the TLS a mail client may offer", () => {
  type Port = "imaps" | "imap";
  const PORTS: [Port, StartTls | undefined][] = [
    ["imaps", undefined],
    ["imap", "imap"],
  ];
  const offer = (port: Port, starttls: StartTls | undefined, args: string[]) =>
    sClient(secure.ports[port], args, { starttls, protocol: "imap" });

  const CBC_ONLY = ["-tls1_2", "-cipher", "ECDHE-RSA-AES128-SHA:ECDHE-RSA-AES256-SHA:AES128-SHA:AES256-SHA"];
  const RSA_KEY_EXCHANGE_ONLY = ["-tls1_2", "-cipher", "AES128-SHA:AES256-SHA"];
  const SHA1_SIGNATURES_ONLY = ["-tls1_2", "-sigalgs", "RSA+SHA1", "-cipher", "ECDHE-RSA-AES128-GCM-SHA256:@SECLEVEL=0"];

  for (const [port, starttls] of PORTS) {
    test(`${port}: TLS 1.2 with CBC suites only`, async () => {
      const result = await offer(port, starttls, CBC_ONLY);
      expect([result.protocol, result.cipher, result.roundTrip]).toEqual(["TLSv1.2", "ECDHE-RSA-AES128-SHA", true]);
    });

    test(`${port}: TLS 1.2 with RSA key exchange only`, async () => {
      const result = await offer(port, starttls, RSA_KEY_EXCHANGE_ONLY);
      expect([result.protocol, result.cipher, result.signature, result.roundTrip]).toEqual(["TLSv1.2", "AES128-SHA", undefined, true]);
    });

    test(`${port}: TLS 1.2 with SHA-1 signatures only`, async () => {
      const result = await offer(port, starttls, SHA1_SIGNATURES_ONLY);
      expect([result.protocol, result.signature, result.roundTrip]).toEqual(["TLSv1.2", "RSA/SHA1", true]);
    });

    test(`${port}: TLS 1.2 without signature_algorithms is signed for with SHA-1`, async () => {
      const result = await handshakeWithoutSignatureAlgorithms(secure.ports[port], starttls);
      expect(result.cipher).toBe("ECDHE-RSA-AES128-GCM-SHA256");
      expect(result.signatureScheme).toBe(0x0201);
      expect(result.firstLine).toBe(
        starttls ? `* CAPABILITY ${CAPABILITIES}` : `* OK [CAPABILITY ${CAPABILITIES}] IMAP4rev1 Service Ready`
      );
    });

    test(`${port}: TLS 1.0 and 1.1 are refused`, async () => {
      for (const version of ["-tls1", "-tls1_1"]) {
        const result = await offer(port, starttls, [version, "-cipher", "ALL:@SECLEVEL=0"]);
        expect(result.protocol).toBeUndefined();
        expect(result.output).toContain("alert protocol version");
      }
    });
  }

  test("STARTTLS negotiates what the implicit-TLS port negotiates", async () => {
    const offers = [
      [],
      CBC_ONLY,
      RSA_KEY_EXCHANGE_ONLY,
      SHA1_SIGNATURES_ONLY,
      ["-tls1_2", "-cipher", "ECDHE-RSA-CHACHA20-POLY1305"],
      ["-tls1_2", "-groups", "P-256", "-cipher", "ECDHE-RSA-AES256-GCM-SHA384"],
      ["-tls1_2", "-cipher", "ECDHE-RSA-AES128-SHA256:DHE-RSA-AES128-GCM-SHA256:@SECLEVEL=0"],
      ["-tls1_3", "-ciphersuites", "TLS_CHACHA20_POLY1305_SHA256:TLS_AES_128_GCM_SHA256"],
      ["-tls1_3", "-groups", "P-256"],
    ];
    const negotiated = async (port: Port, starttls: StartTls | undefined) => {
      const results = [];
      for (const args of offers) {
        const { protocol, cipher, signature, serverTempKey, roundTrip } = await offer(port, starttls, args);
        results.push({ args: args.join(" "), protocol, cipher, signature, serverTempKey, roundTrip });
      }
      return results;
    };
    const implicit = await negotiated("imaps", undefined);
    expect(implicit.filter((result) => !result.protocol).map((result) => result.args)).toEqual([
      "-tls1_2 -cipher ECDHE-RSA-AES128-SHA256:DHE-RSA-AES128-GCM-SHA256:@SECLEVEL=0",
    ]);
    expect(await negotiated("imap", "imap")).toEqual(implicit);
  });

  test("at TLS 1.2 the implicit-TLS port picks the suite and group by its own order, STARTTLS by the client's", async () => {
    // The implicit-TLS port is a TLS listener of its own; STARTTLS wraps a
    // socket that is already open, which takes the client's order, as every
    // SMTP listener does.
    const suites = ["-tls1_2", "-cipher", "AES256-SHA:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-RSA-AES128-GCM-SHA256"];
    expect((await offer("imaps", undefined, suites)).cipher).toBe("ECDHE-RSA-AES128-GCM-SHA256");
    expect((await offer("imap", "imap", suites)).cipher).toBe("AES256-SHA");
    const groups = ["-tls1_2", "-groups", "P-384:X25519"];
    expect((await offer("imaps", undefined, groups)).serverTempKey).toBe("X25519, 253 bits");
    expect((await offer("imap", "imap", groups)).serverTempKey).toBe("ECDH, secp384r1, 384 bits");
  });
});

describe("LOGINDISABLED: plain-text sign-in turned off (single sign-on configured)", () => {
  test("the plain port advertises LOGINDISABLED in place of AUTH=PLAIN, with STARTTLS", async () => {
    const imap = await ImapClient.connect(sso);
    expect(imap.greeting).toBe(`* OK [CAPABILITY ${LOGINDISABLED} STARTTLS] IMAP4rev1 Service Ready`);
    expect((await imap.command("CAPABILITY")).untagged).toEqual([`* CAPABILITY ${LOGINDISABLED} STARTTLS`]);
    imap.close();
  });

  test("LOGIN and every AUTHENTICATE form are refused with PRIVACYREQUIRED before TLS", async () => {
    const imap = await ImapClient.connect(sso);
    const refusal = "NO [PRIVACYREQUIRED] Use STARTTLS or the TLS port to sign in";
    for (const command of [
      "LOGIN alice password-of-alice",
      "LOGIN nobody wrong",
      `AUTHENTICATE PLAIN ${saslPlain("alice", "password-of-alice")}`,
      "AUTHENTICATE PLAIN",
      "AUTHENTICATE LOGIN",
      "AUTHENTICATE PLAIN !!!",
    ]) {
      const result = await imap.command(command);
      expect({ command, lines: result.lines }).toEqual({ command, lines: [`${result.tag} ${refusal}`] });
    }
    // Refusals here are not failed sign-ins: a hundred of them would not lock the address out.
    expect((await imap.command("NOOP")).status).toBe("OK");
    imap.close();
  });

  test("after STARTTLS the capabilities offer AUTH=PLAIN and no LOGINDISABLED", async () => {
    const imap = await ImapClient.connect(sso);
    expect((await imap.startTls()).lines).toEqual(["A1 OK Begin TLS negotiation now"]);
    expect((await imap.command("CAPABILITY")).untagged).toEqual([`* CAPABILITY ${CAPABILITIES}`]);
    imap.close();
  });

  test("the TLS port never advertises LOGINDISABLED", async () => {
    const imap = await ImapClient.connect(sso, { tls: true });
    expect(imap.greeting).toBe(`* OK [CAPABILITY ${CAPABILITIES}] IMAP4rev1 Service Ready`);
    imap.close();
  });

  test("inside TLS a sign-in is checked with the provider; while it cannot be reached the answer is UNAVAILABLE", async () => {
    const imap = await ImapClient.connect(sso, { tls: true });
    const refusal = "NO [UNAVAILABLE] Authentication is temporarily unavailable";
    // Local passwords are not accepted here, right or wrong.
    for (const command of [
      "LOGIN alice password-of-alice",
      "LOGIN alice wrong",
      `LOGIN admin ${ADMIN_PASSWORD}`,
      `AUTHENTICATE PLAIN ${saslPlain("alice", "password-of-alice")}`,
    ]) {
      const result = await imap.command(command);
      expect({ command, lines: result.lines }).toEqual({ command, lines: [`${result.tag} ${refusal}`] });
    }
    // The connection stays open, and these do not count as failed attempts.
    for (let i = 0; i < 12; i++) {
      expect((await imap.command("LOGIN alice wrong")).text).toBe("[UNAVAILABLE] Authentication is temporarily unavailable");
    }
    expect(imap.closed).toBe(false);
    imap.close();
  });

  test("with local sign-in kept on, a correct local password signs in inside TLS", async () => {
    const imap = await ImapClient.connect(ssoLocal);
    expect(imap.greeting).toBe(`* OK [CAPABILITY ${LOGINDISABLED} STARTTLS] IMAP4rev1 Service Ready`);
    expect((await imap.command("LOGIN alice password-of-alice")).lines).toEqual([
      "A1 NO [PRIVACYREQUIRED] Use STARTTLS or the TLS port to sign in",
    ]);
    await imap.startTls();
    expect((await imap.command("LOGIN alice password-of-alice")).lines).toEqual([
      `A3 OK [CAPABILITY ${CAPABILITIES}] LOGIN completed`,
    ]);
    imap.close();

    const direct = await ImapClient.connect(ssoLocal, { tls: true });
    expect((await direct.command(`AUTHENTICATE PLAIN ${saslPlain("admin", ADMIN_PASSWORD)}`)).lines).toEqual([
      `A1 OK [CAPABILITY ${CAPABILITIES}] AUTHENTICATE completed`,
    ]);
    // A wrong local password falls through to the provider.
    expect((await direct.command("LOGIN alice wrong")).lines).toEqual([
      "A2 NO [UNAVAILABLE] Authentication is temporarily unavailable",
    ]);
    direct.close();
  });

  test("without a certificate there is LOGINDISABLED and no STARTTLS: no way to sign in over IMAP", async () => {
    const imap = await ImapClient.connect(ssoNoCertificate);
    expect(imap.greeting).toBe(`* OK [CAPABILITY ${LOGINDISABLED}] IMAP4rev1 Service Ready`);
    expect((await imap.command(`LOGIN admin ${ADMIN_PASSWORD}`)).lines).toEqual([
      "A1 NO [PRIVACYREQUIRED] Use STARTTLS or the TLS port to sign in",
    ]);
    expect((await imap.command("STARTTLS")).lines).toEqual(["A2 NO STARTTLS is not available"]);
    imap.close();
  });
});

describe("PROXY protocol", () => {
  const header = (source = "203.0.113.7") => `PROXY TCP4 ${source} 10.0.0.1 40000 143\r\n`;

  test("after a PROXY v1 header the plain port greets as usual", async () => {
    const imap = await ImapClient.connect(proxied, { preface: header() });
    expect(imap.greeting).toBe(`* OK [CAPABILITY ${CAPABILITIES_STARTTLS}] IMAP4rev1 Service Ready`);
    expect((await imap.command(`LOGIN admin ${ADMIN_PASSWORD}`)).status).toBe("OK");
    imap.close();
  });

  test("nothing is sent before the header arrives, however it is split", async () => {
    const imap = await ImapClient.connect(proxied, { greeting: false });
    await sleep(300);
    expect(imap.transcript).toBe("");
    imap.write("PROXY TCP4 203.0.113.8 ");
    await sleep(100);
    expect(imap.transcript).toBe("");
    imap.write("10.0.0.1 1234 143\r\n");
    expect((await imap.next()).raw).toBe(`* OK [CAPABILITY ${CAPABILITIES_STARTTLS}] IMAP4rev1 Service Ready`);
    imap.close();
  });

  test("commands sent in the same segment as the header are answered after the greeting", async () => {
    const imap = await ImapClient.connect(proxied, { preface: `${header()}A1 NOOP\r\n` });
    expect((await imap.next()).raw).toBe("A1 OK NOOP completed");
    imap.close();
  });

  test("TCP6 and UNKNOWN headers are accepted", async () => {
    for (const preface of ["PROXY TCP6 2001:db8::1 2001:db8::2 40000 143\r\n", "PROXY UNKNOWN\r\n", "PROXY UNKNOWN ignored text\r\n"]) {
      const imap = await ImapClient.connect(proxied, { preface });
      expect({ preface, greeting: imap.greeting }).toEqual({
        preface,
        greeting: `* OK [CAPABILITY ${CAPABILITIES_STARTTLS}] IMAP4rev1 Service Ready`,
      });
      imap.close();
    }
  });

  test("a connection that starts with anything else is dropped without a word", async () => {
    const malformed = [
      "A1 NOOP\r\n",
      "HELLO\r\n",
      "proxy TCP4 203.0.113.7 10.0.0.1 40000 143\r\n",
      "PROXY TCP4 203.0.113.7\r\n",
      "PROXY TCP4 not-an-address 10.0.0.1 40000 143\r\n",
      "PROXY TCP6 203.0.113.7 10.0.0.1 40000 143\r\n",
      "PROXY TCP4 203.0.113.7 10.0.0.1 70000 143\r\n",
      "PROXY TCP4 203.0.113.7 10.0.0.1 40000 143 extra\r\n",
      `PROXY ${"x".repeat(200)}\r\n`,
    ];
    for (const preface of malformed) {
      const imap = await ImapClient.connect(proxied, { preface, greeting: false });
      expect({ preface, sent: raws(await imap.waitForClose()), transcript: imap.transcript }).toEqual({
        preface,
        sent: [],
        transcript: "",
      });
    }
  });

  test("a connection that sends no header is dropped after five seconds", async () => {
    const imap = await ImapClient.connect(proxied, { greeting: false });
    const started = Date.now();
    expect(raws(await imap.waitForClose(10_000))).toEqual([]);
    const waited = Date.now() - started;
    expect(waited).toBeGreaterThanOrEqual(4500);
    expect(waited).toBeLessThan(9000);
    expect(imap.transcript).toBe("");
  }, 20_000);

  test("on the TLS port the header comes first, in the clear, then the handshake", async () => {
    const imap = await ImapClient.connect(proxied, { tls: true, preface: header() });
    expect(imap.encrypted).toBe(true);
    expect(imap.greeting).toBe(`* OK [CAPABILITY ${CAPABILITIES}] IMAP4rev1 Service Ready`);
    expect((await imap.command(`LOGIN admin ${ADMIN_PASSWORD}`)).status).toBe("OK");
    imap.close();
  });

  test("STARTTLS works behind the proxy header", async () => {
    const imap = await ImapClient.connect(proxied, { preface: header() });
    expect((await imap.startTls()).lines).toEqual(["A1 OK Begin TLS negotiation now"]);
    expect((await imap.command("CAPABILITY")).untagged).toEqual([`* CAPABILITY ${CAPABILITIES}`]);
    imap.close();
  });

  test("the failed sign-in limit follows the address the header names, also across STARTTLS", async () => {
    await createUser(proxied, "alice");
    const source = "203.0.113.99";
    const imap = await ImapClient.connect(proxied, { preface: header(source) });
    await imap.startTls();
    const answers: string[] = [];
    for (let i = 0; i < 10; i++) answers.push((await imap.command(`LOGIN alice wrong-${i}`)).text!);
    expect(answers.slice(0, 9)).toEqual(Array(9).fill("[AUTHENTICATIONFAILED] Invalid credentials."));
    expect(answers[9]).toBe("[AUTHENTICATIONFAILED] Too many failed attempts");
    await imap.waitForClose();

    // The same address is locked out on a fresh plain connection; another address is not.
    const again = await ImapClient.connect(proxied, { preface: header(source) });
    expect((await again.command("LOGIN alice password-of-alice")).text).toBe(
      "[AUTHENTICATIONFAILED] Too many failed attempts"
    );
    const other = await ImapClient.connect(proxied, { preface: header("203.0.113.100") });
    expect((await other.command("LOGIN alice password-of-alice")).status).toBe("OK");
    other.close();
  }, 30_000);
});
