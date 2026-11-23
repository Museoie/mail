import { afterAll, afterEach, beforeAll, describe, expect, test } from "bun:test";
import type { TestServer } from "../harness";
import { HOST, boot, closeClients, ehloReply, mailByMessageId, messageId, open, plain, type Listener } from "./support";

// With PROXY_PROTOCOL=true every mail connection must open with a PROXY
// protocol v1 line, and the client it names is the one the session is
// attributed to. The tests connect from 127.0.0.1, which the server trusts as
// a proxy.
let server: TestServer;

beforeAll(async () => {
  server = await boot({ tls: true, env: { PROXY_PROTOCOL: "true" } });
}, 180_000);

afterEach(closeClients);

afterAll(async () => {
  await server?.stop();
});

/** Opens a connection that starts with `header`, and reports what the server then says to EHLO. */
const greetedAs = async (header: string, listener: Listener = "smtp") => {
  const smtp = await open(server, listener, { proxy: header });
  expect(smtp.greeting.raw).toBe(`220 ${HOST} ESMTP`);
  const reply = await smtp.ehlo();
  return { smtp, address: /\[(.*)\]$/.exec(reply.lines[0]!)?.[1], reply };
};

/** Opens a connection that starts with `bytes` and waits for the server to drop it without a word. */
const dropped = async (bytes: string | Uint8Array, listener: Listener = "smtp") => {
  const smtp = await open(server, listener, { tls: false, greeting: false });
  smtp.write(bytes);
  const reply = await smtp.readOrClose(4000).catch(() => "still open");
  return { reply, closed: smtp.closed, received: smtp.unparsed };
};
const SILENTLY = { reply: undefined, closed: true, received: "" };

describe("the client a header names", () => {
  test("TCP4: the source address is the session's client", async () => {
    const { address, reply } = await greetedAs("PROXY TCP4 10.1.2.3 10.0.0.2 51000 25");
    expect(address).toBe("10.1.2.3");
    expect(reply.raw).toBe(ehloReply({ address: "10.1.2.3", starttls: true }));
    expect((await greetedAs("PROXY TCP4 203.0.113.7 198.51.100.1 1 65535")).address).toBe("203.0.113.7");
    expect((await greetedAs("PROXY TCP4 127.0.0.1 127.0.0.1 0 0")).address).toBe("127.0.0.1");
  });

  test("TCP6: an IPv6 source, written back as the header wrote it", async () => {
    expect((await greetedAs("PROXY TCP6 2001:db8::7 fd00::2 51000 25")).address).toBe("2001:db8::7");
    expect((await greetedAs("PROXY TCP6 2001:DB8:0:0:0:0:0:7 ::1 51000 25")).address).toBe("2001:DB8:0:0:0:0:0:7");
    expect((await greetedAs("PROXY TCP6 ::1 ::1 51000 25")).address).toBe("::1");
  });

  test("TCP6 with an IPv4-mapped source is attributed to the IPv4 address", async () => {
    expect((await greetedAs("PROXY TCP6 ::ffff:10.1.2.3 ::ffff:10.0.0.2 51000 25")).address).toBe("10.1.2.3");
    expect((await greetedAs("PROXY TCP6 ::FFFF:10.1.2.4 ::1 51000 25")).address).toBe("10.1.2.4");
  });

  test("UNKNOWN keeps the connection's own address, whatever follows the word", async () => {
    expect((await greetedAs("PROXY UNKNOWN")).address).toBe("127.0.0.1");
    expect((await greetedAs("PROXY UNKNOWN ffff::1 ffff::2 1 2")).address).toBe("127.0.0.1");
    expect((await greetedAs("PROXY UNKNOWN anything at all")).address).toBe("127.0.0.1");
  });

  test("the address holds for the whole session and what it delivers", async () => {
    const id = messageId("proxy");
    const { smtp } = await greetedAs("PROXY TCP4 10.20.30.40 10.0.0.2 51000 25");
    expect((await smtp.helo()).raw).toBe(`250 ${HOST} Nice to meet you, [10.20.30.40]`);
    const result = await smtp.transaction({ from: "bounce@remote.example", to: ["hello@mail.test"], data: plain({ messageId: id }) });
    expect(result.final.raw).toBe("250 OK: message queued");
    expect((await mailByMessageId(server, id)).subject).toBe("Test message");
    expect((await smtp.ehlo()).lines[0]).toBe(`${HOST} Nice to meet you, [10.20.30.40]`);
  });
});

describe("when the header arrives", () => {
  test("nothing is sent before it, not even the greeting", async () => {
    const smtp = await open(server, "smtp", { greeting: false });
    expect(await smtp.staysQuiet(500)).toBe(true);
    smtp.write("PROXY TCP4 10.1.2.5 10.0.0.2 51000 25\r\n");
    expect((await smtp.read()).raw).toBe(`220 ${HOST} ESMTP`);
    expect((await smtp.ehlo()).lines[0]).toBe(`${HOST} Nice to meet you, [10.1.2.5]`);
  });

  test("a header split across writes is put together", async () => {
    const smtp = await open(server, "smtp", { greeting: false });
    await smtp.writeFlushed("PROXY TCP4 10.1.");
    await smtp.writeFlushed("2.6 10.0.0.2 51000 25\r");
    expect(await smtp.staysQuiet(300)).toBe(true);
    smtp.write("\n");
    expect((await smtp.read()).raw).toBe(`220 ${HOST} ESMTP`);
    expect((await smtp.ehlo()).lines[0]).toBe(`${HOST} Nice to meet you, [10.1.2.6]`);
  });

  test("a command sent in the same packet as the header still counts as talking too soon", async () => {
    const smtp = await open(server, "smtp", { greeting: false });
    smtp.write("PROXY TCP4 10.1.2.7 10.0.0.2 51000 25\r\nEHLO eager.example\r\n");
    expect((await smtp.read()).raw).toBe(`421 ${HOST} You talk too soon`);
    expect(await smtp.waitForClose(5000)).toBe(true);
  });

  test("a connection that sends no header is dropped after five seconds", async () => {
    const smtp = await open(server, "smtp", { greeting: false });
    const started = Date.now();
    expect(await smtp.waitForClose(4000)).toBe(false);
    expect(await smtp.waitForClose(15_000)).toBe(true);
    expect(Date.now() - started).toBeGreaterThanOrEqual(4500);
    expect(smtp.unparsed).toBe("");
  }, 30_000);
});

describe("connections without a valid header are dropped without a reply", () => {
  test("a client that starts with SMTP", async () => {
    expect(await dropped("EHLO client.remote.example\r\n")).toEqual(SILENTLY);
    expect(await dropped("QUIT\r\n")).toEqual(SILENTLY);
  });

  test("malformed headers", async () => {
    const malformed = [
      "PROXY",
      "PROXY TCP4",
      "proxy TCP4 10.1.2.3 10.0.0.2 51000 25",
      "PROXY tcp4 10.1.2.3 10.0.0.2 51000 25",
      "PROXY unknown",
      "PROXY TCP5 10.1.2.3 10.0.0.2 51000 25",
      "PROXY TCP4 10.1.2.3 10.0.0.2 51000",
      "PROXY TCP4 10.1.2.3 10.0.0.2 51000 25 extra",
      "PROXY  TCP4 10.1.2.3 10.0.0.2 51000 25",
      "PROXY TCP4 10.1.2.3  10.0.0.2 51000 25",
      " PROXY TCP4 10.1.2.3 10.0.0.2 51000 25",
      "PROXY TCP4 10.1.2.3 10.0.0.2 51000 25 ",
      "PROXY TCP4 10.1.2 10.0.0.2 51000 25",
      "PROXY TCP4 10.1.2.256 10.0.0.2 51000 25",
      "PROXY TCP4 host.example 10.0.0.2 51000 25",
      "PROXY TCP4 10.1.2.3 nowhere 51000 25",
      "PROXY TCP4 2001:db8::7 fd00::2 51000 25",
      "PROXY TCP6 10.1.2.3 10.0.0.2 51000 25",
      "PROXY TCP6 2001:db8::7 10.0.0.2 51000 25",
      "PROXY TCP4 10.1.2.3 10.0.0.2 65536 25",
      "PROXY TCP4 10.1.2.3 10.0.0.2 51000 65536",
      "PROXY TCP4 10.1.2.3 10.0.0.2 051000 25",
      "PROXY TCP4 10.1.2.3 10.0.0.2 -1 25",
      "PROXY TCP4 10.1.2.3 10.0.0.2 5e3 25",
      "PROXY TCP4 10.1.2.3 10.0.0.2 port 25",
    ];
    for (const header of malformed) {
      expect({ header, ...(await dropped(`${header}\r\n`)) }).toEqual({ header, ...SILENTLY });
    }
  });

  test("a header longer than 107 octets", async () => {
    expect(await dropped(`PROXY UNKNOWN ${"x".repeat(100)}\r\n`)).toEqual(SILENTLY);
    // Cut off as soon as 107 octets have come without a line end.
    expect(await dropped(`PROXY TCP4 ${"1".repeat(96)}`)).toEqual(SILENTLY);
  });

  test("the binary version 2 header", async () => {
    const signature = Buffer.from([0x0d, 0x0a, 0x0d, 0x0a, 0x00, 0x0d, 0x0a, 0x51, 0x55, 0x49, 0x54, 0x0a]);
    const v2 = Buffer.concat([signature, Buffer.from([0x21, 0x11, 0x00, 0x0c, 10, 1, 2, 3, 10, 0, 0, 2, 0xc7, 0x38, 0x00, 0x19])]);
    expect(await dropped(v2)).toEqual(SILENTLY);
  });

  test("the server keeps serving after each of them", async () => {
    expect((await greetedAs("PROXY TCP4 10.1.2.8 10.0.0.2 51000 25")).address).toBe("10.1.2.8");
  });
});

describe("a header that ends in a bare LF", () => {
  test("is not taken as complete: the connection waits, then is dropped", async () => {
    const smtp = await open(server, "smtp", { greeting: false });
    smtp.write("PROXY TCP4 10.1.2.9 10.0.0.2 51000 25\n");
    expect(await smtp.staysQuiet(500)).toBe(true);
    expect(smtp.closed).toBe(false);
    // Completing the line with CRLF now makes the first line end where the CRLF is: one malformed header.
    smtp.write("\r\n");
    expect(await smtp.readOrClose(4000)).toBeUndefined();
    expect(smtp.closed).toBe(true);
  });
});

describe("the TLS listeners", () => {
  test("implicit TLS: the header comes before the handshake", async () => {
    const { smtp, address, reply } = await greetedAs("PROXY TCP4 10.9.8.7 10.0.0.2 51000 465", "smtps");
    expect(smtp.secure).toBe(true);
    expect(address).toBe("10.9.8.7");
    expect(reply.raw).toBe(ehloReply({ address: "10.9.8.7", starttls: false }));
    expect((await greetedAs("PROXY TCP6 2001:db8::99 fd00::2 51000 465", "smtps")).address).toBe("2001:db8::99");
  });

  test("implicit TLS: a handshake with no header before it is dropped", async () => {
    const attempt = open(server, "smtps");
    await expect(attempt).rejects.toThrow();
  });

  test("submission: the header, then STARTTLS, and the address survives the upgrade", async () => {
    const { smtp, reply } = await greetedAs("PROXY TCP4 10.9.8.6 10.0.0.2 51000 587", "submission");
    expect(reply.raw).toBe(ehloReply({ address: "10.9.8.6", starttls: true }));
    expect((await smtp.startTls()).raw).toBe("220 Ready to start TLS");
    expect((await smtp.ehlo()).raw).toBe(ehloReply({ address: "10.9.8.6", starttls: false }));
  });

  test("no second header is expected after STARTTLS", async () => {
    const { smtp } = await greetedAs("PROXY TCP4 10.9.8.5 10.0.0.2 51000 25");
    await smtp.startTls();
    expect((await smtp.send("PROXY TCP4 10.6.6.6 10.0.0.2 51000 25")).raw).toBe("500 Error: command not recognized");
    expect((await smtp.ehlo()).lines[0]).toBe(`${HOST} Nice to meet you, [10.9.8.5]`);
  });
});
