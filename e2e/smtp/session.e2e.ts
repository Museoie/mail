import { afterAll, afterEach, beforeAll, describe, expect, test } from "bun:test";
import { waitFor, type TestServer } from "../harness";
import { SmtpClient } from "../harness/smtp";
import { HOST, MAX_MESSAGE_BYTES, boot, closeClients, crlf, ehloReply, mailByMessageId, messageId, open, plain, raws, session } from "./support";

// The port-25 listener of a server with no certificate: the plainest posture.
let server: TestServer;

beforeAll(async () => {
  server = await boot();
}, 180_000);

afterEach(closeClients);

afterAll(async () => {
  await server?.stop();
});

const UNRECOGNIZED = "500 Error: command not recognized";
const NEED_HELO = "503 Error: send HELO/EHLO first";

// First in the file: it counts connections, so nothing else may be open.
describe("connection limit", () => {
  test("the 101st simultaneous connection is answered 421 and closed", async () => {
    const held = await Promise.all(Array.from({ length: 99 }, () => open(server)));
    expect(new Set(held.map((client) => client.greeting.raw))).toEqual(new Set([`220 ${HOST} ESMTP`]));

    const hundredth = await open(server);
    expect(hundredth.greeting.raw).toBe(`220 ${HOST} ESMTP`);

    const refused = await open(server);
    expect(refused.greeting.raw).toBe(`421 ${HOST} Too many connected clients, try again in a moment`);
    expect(await refused.waitForClose(5000)).toBe(true);

    // The held connections are unaffected, and a slot frees as soon as one leaves.
    expect((await held[0]!.send("NOOP")).raw).toBe("250 OK");
    expect((await hundredth.send("QUIT")).raw).toBe("221 Bye");
    expect(await hundredth.waitForClose(5000)).toBe(true);
    const next = await waitFor(async () => {
      const client = await open(server);
      return client.greeting.code === 220 ? client : undefined;
    }, { what: "a freed connection slot" });
    expect(next.greeting.raw).toBe(`220 ${HOST} ESMTP`);

    closeClients();
    // Leave only once the server has let go of them all.
    await waitFor(async () => {
      const probe = await SmtpClient.connect({ port: server.ports.smtp });
      probe.close();
      return probe.greeting.code === 220;
    }, { what: "the connections to drain" });
  });
});

describe("greeting", () => {
  test("is 220 with the host name and ESMTP, and nothing else", async () => {
    const smtp = await open(server);
    expect(smtp.greeting.raw).toBe(`220 ${HOST} ESMTP`);
    expect(await smtp.staysQuiet(200)).toBe(true);
  });

  test("a client that talks before the greeting is answered 421 and closed", async () => {
    const smtp = await open(server, "smtp", { greeting: false });
    smtp.write("EHLO early.example\r\n");
    const reply = await smtp.read();
    expect(reply.raw).toBe(`421 ${HOST} You talk too soon`);
    expect(await smtp.waitForClose(5000)).toBe(true);
    // The greeting never follows.
    expect(raws(smtp.transcript)).toEqual([`421 ${HOST} You talk too soon`]);
  });

  test("an HTTP request line is answered 421 and closed, in any letter case", async () => {
    for (const line of ["GET / HTTP/1.1", "post /submit HTTP/1.0", "OPTIONS /x HTTP/2.0", "CONNECT /a b HTTP/1.1"]) {
      const smtp = await open(server);
      expect((await smtp.send(line)).raw).toBe("421 HTTP requests not allowed");
      expect(await smtp.waitForClose(5000)).toBe(true);
    }
  });

  test("a line that only resembles an HTTP request is an unrecognized command", async () => {
    const smtp = await open(server);
    expect((await smtp.send("GET /")).raw).toBe(UNRECOGNIZED);
    expect((await smtp.send("PATCH / HTTP/1.1")).raw).toBe(UNRECOGNIZED);
    expect((await smtp.send("GET index.html HTTP/1.1")).raw).toBe(UNRECOGNIZED);
  });
});

describe("EHLO and HELO", () => {
  test("EHLO lists the extensions, without STARTTLS when there is no certificate", async () => {
    const smtp = await open(server);
    const reply = await smtp.send("EHLO client.remote.example");
    expect(reply.raw).toBe(ehloReply());
    expect(reply.lines).toEqual([
      `${HOST} Nice to meet you, [127.0.0.1]`,
      "PIPELINING",
      "8BITMIME",
      "SMTPUTF8",
      "AUTH LOGIN PLAIN",
      `SIZE ${MAX_MESSAGE_BYTES}`,
    ]);
  });

  test("HELO answers one line with no extensions", async () => {
    const smtp = await open(server);
    expect((await smtp.send("HELO client.remote.example")).raw).toBe(`250 ${HOST} Nice to meet you, [127.0.0.1]`);
  });

  test("EHLO needs exactly one argument", async () => {
    const smtp = await open(server);
    expect((await smtp.send("EHLO")).raw).toBe("501 Error: syntax: EHLO hostname");
    expect((await smtp.send("EHLO ")).raw).toBe("501 Error: syntax: EHLO hostname");
    expect((await smtp.send("EHLO a.example b.example")).raw).toBe("501 Error: syntax: EHLO hostname");
    // A refused EHLO does not open the session.
    expect((await smtp.send("MAIL FROM:<a@remote.example>")).raw).toBe(NEED_HELO);
  });

  test("HELO needs exactly one argument, and says so with a capital S", async () => {
    const smtp = await open(server);
    expect((await smtp.send("HELO")).raw).toBe("501 Error: Syntax: HELO hostname");
    expect((await smtp.send("HELO a.example b.example")).raw).toBe("501 Error: Syntax: HELO hostname");
    expect((await smtp.send("MAIL FROM:<a@remote.example>")).raw).toBe(NEED_HELO);
  });

  test("the argument is not checked: an address literal, a bare word and extra spaces all pass", async () => {
    const smtp = await open(server);
    for (const line of ["EHLO [192.0.2.1]", "EHLO x", "EHLO    spaced.example   ", "EHLO !!!", "HELO  spaced.example "]) {
      expect((await smtp.send(line)).code).toBe(250);
    }
  });

  test("the reply names the client's address, not what the client called itself", async () => {
    const smtp = await open(server);
    const reply = await smtp.send("EHLO [203.0.113.99]");
    expect(reply.lines[0]).toBe(`${HOST} Nice to meet you, [127.0.0.1]`);
  });

  test("EHLO can be repeated, and each one starts a clean envelope", async () => {
    const smtp = await session(server);
    expect((await smtp.send("MAIL FROM:<a@remote.example>")).raw).toBe("250 Accepted");
    expect((await smtp.send("EHLO again.example")).raw).toBe(ehloReply());
    expect((await smtp.send("RCPT TO:<hello@mail.test>")).raw).toBe("503 Error: need MAIL command");
    expect((await smtp.send("MAIL FROM:<a@remote.example>")).raw).toBe("250 Accepted");
  });

  test("HELO also clears the envelope", async () => {
    const smtp = await session(server);
    expect((await smtp.send("MAIL FROM:<a@remote.example>")).raw).toBe("250 Accepted");
    expect((await smtp.send("RCPT TO:<hello@mail.test>")).raw).toBe("250 Accepted");
    expect((await smtp.send("HELO again.example")).code).toBe(250);
    expect((await smtp.send("DATA")).raw).toBe("503 Error: need RCPT command");
    expect((await smtp.send("MAIL FROM:<b@remote.example>")).raw).toBe("250 Accepted");
  });

  test("a session opened with HELO delivers mail", async () => {
    const id = messageId("helo");
    const smtp = await open(server);
    await smtp.helo();
    const result = await smtp.transaction({ from: "a@remote.example", to: ["hello@mail.test"], data: plain({ messageId: id }) });
    expect(raws(result.replies)).toEqual(["250 Accepted", "250 Accepted", "250 OK: message queued"]);
    expect((await mailByMessageId(server, id)).subject).toBe("Test message");
  });
});

describe("commands that need no session", () => {
  test("NOOP, RSET, HELP, VRFY and QUIT answer before EHLO", async () => {
    const smtp = await open(server);
    expect((await smtp.send("NOOP")).raw).toBe("250 OK");
    expect((await smtp.send("RSET")).raw).toBe("250 Flushed");
    expect((await smtp.send("HELP")).raw).toBe("214 See https://tools.ietf.org/html/rfc5321 for details");
    expect((await smtp.send("VRFY admin")).raw).toBe("252 Try to send something. No promises though");
    expect((await smtp.send("QUIT")).raw).toBe("221 Bye");
    expect(await smtp.waitForClose(5000)).toBe(true);
  });

  test("their arguments are ignored", async () => {
    const smtp = await open(server);
    expect((await smtp.send("NOOP anything at all")).raw).toBe("250 OK");
    expect((await smtp.send("RSET now")).raw).toBe("250 Flushed");
    expect((await smtp.send("HELP MAIL")).raw).toBe("214 See https://tools.ietf.org/html/rfc5321 for details");
    expect((await smtp.send("VRFY")).raw).toBe("252 Try to send something. No promises though");
    expect((await smtp.send("VRFY <nobody@nowhere.mail.test>")).raw).toBe("252 Try to send something. No promises though");
    expect((await smtp.send("QUIT and goodbye")).raw).toBe("221 Bye");
    expect(await smtp.waitForClose(5000)).toBe(true);
  });

  test("EXPN, TURN, ETRN, BDAT, SEND and friends are not implemented", async () => {
    const smtp = await session(server);
    for (const line of ["EXPN staff", "TURN", "ETRN mail.test", "BDAT 5 LAST", "SEND FROM:<a@remote.example>", "SAML", "SOML", "X-CUSTOM", "LHLO x"]) {
      expect((await smtp.send(line)).raw).toBe(UNRECOGNIZED);
    }
  });

  test("MAIL, RCPT, DATA and AUTH are refused before EHLO", async () => {
    const smtp = await open(server);
    for (const line of ["MAIL FROM:<a@remote.example>", "RCPT TO:<hello@mail.test>", "DATA", "AUTH PLAIN AGEAYg==", "mail from:<a@remote.example>", "AUTH"]) {
      expect((await smtp.send(line)).raw).toBe(NEED_HELO);
    }
    // Refused even when the line itself is malformed: the session check comes first.
    expect((await smtp.send("MAIL")).raw).toBe(NEED_HELO);
    expect((await smtp.send("RCPT nonsense")).raw).toBe(NEED_HELO);
  });

  test("STARTTLS without a certificate is 454, before or after EHLO", async () => {
    const smtp = await open(server);
    expect((await smtp.send("STARTTLS")).raw).toBe("454 Error: TLS not available");
    await smtp.ehlo();
    expect((await smtp.send("STARTTLS")).raw).toBe("454 Error: TLS not available");
    expect((await smtp.send("NOOP")).raw).toBe("250 OK");
  });
});

describe("command sequencing", () => {
  test("RCPT needs MAIL, and DATA needs RCPT", async () => {
    const smtp = await session(server);
    expect((await smtp.send("RCPT TO:<hello@mail.test>")).raw).toBe("503 Error: need MAIL command");
    expect((await smtp.send("DATA")).raw).toBe("503 Error: need RCPT command");
    expect((await smtp.send("MAIL FROM:<a@remote.example>")).raw).toBe("250 Accepted");
    expect((await smtp.send("DATA")).raw).toBe("503 Error: need RCPT command");
    expect((await smtp.send("RCPT TO:<hello@mail.test>")).raw).toBe("250 Accepted");
    expect((await smtp.send("DATA")).raw).toBe("354 End data with <CR><LF>.<CR><LF>");
    smtp.write(".\r\n");
    expect((await smtp.read()).raw).toBe("250 OK: message queued");
  });

  test("a second MAIL in one transaction is refused and leaves the first in place", async () => {
    const id = messageId("nested");
    const smtp = await session(server);
    expect((await smtp.send("MAIL FROM:<first@remote.example>")).raw).toBe("250 Accepted");
    expect((await smtp.send("MAIL FROM:<second@remote.example>")).raw).toBe("503 Error: nested MAIL command");
    // Syntax is still checked first.
    expect((await smtp.send("MAIL FROM:second@remote.example")).raw).toBe("501 Error: Bad sender address syntax");
    expect((await smtp.send("RCPT TO:<hello@mail.test>")).raw).toBe("250 Accepted");
    expect((await smtp.data(plain({ messageId: id }))).raw).toBe("250 OK: message queued");
    expect((await mailByMessageId(server, id)).envelope_from).toEqual([{ address: "first@remote.example" }]);
  });

  test("a null sender opens a transaction like any other", async () => {
    const smtp = await session(server);
    expect((await smtp.send("MAIL FROM:<>")).raw).toBe("250 Accepted");
    expect((await smtp.send("MAIL FROM:<>")).raw).toBe("503 Error: nested MAIL command");
    expect((await smtp.send("RCPT TO:<hello@mail.test>")).raw).toBe("250 Accepted");
  });

  test("RSET forgets the sender and the recipients", async () => {
    const smtp = await session(server);
    expect((await smtp.send("MAIL FROM:<a@remote.example>")).raw).toBe("250 Accepted");
    expect((await smtp.send("RCPT TO:<hello@mail.test>")).raw).toBe("250 Accepted");
    expect((await smtp.send("RSET")).raw).toBe("250 Flushed");
    expect((await smtp.send("DATA")).raw).toBe("503 Error: need RCPT command");
    expect((await smtp.send("RCPT TO:<hello@mail.test>")).raw).toBe("503 Error: need MAIL command");
    expect((await smtp.send("MAIL FROM:<b@remote.example>")).raw).toBe("250 Accepted");
  });

  test("a finished transaction leaves a clean envelope and the session open", async () => {
    const first = messageId("seq");
    const second = messageId("seq");
    const smtp = await session(server);
    expect((await smtp.transaction({ from: "a@remote.example", to: ["hello@mail.test"], data: plain({ messageId: first }) })).final.raw).toBe(
      "250 OK: message queued"
    );
    expect((await smtp.send("RCPT TO:<hello@mail.test>")).raw).toBe("503 Error: need MAIL command");
    expect((await smtp.send("DATA")).raw).toBe("503 Error: need RCPT command");
    expect((await smtp.transaction({ from: "b@remote.example", to: ["hello@mail.test"], data: plain({ messageId: second }) })).final.raw).toBe(
      "250 OK: message queued"
    );
    expect((await mailByMessageId(server, first)).envelope_from).toEqual([{ address: "a@remote.example" }]);
    expect((await mailByMessageId(server, second)).envelope_from).toEqual([{ address: "b@remote.example" }]);
  });

  test("QUIT mid-transaction closes without storing anything", async () => {
    const smtp = await session(server);
    await smtp.send("MAIL FROM:<a@remote.example>");
    await smtp.send("RCPT TO:<hello@mail.test>");
    expect((await smtp.send("QUIT")).raw).toBe("221 Bye");
    expect(await smtp.waitForClose(5000)).toBe(true);
  });
});

describe("command syntax", () => {
  test("verbs are matched in any letter case", async () => {
    const id = messageId("case");
    const smtp = await open(server);
    expect((await smtp.send("ehlo client.remote.example")).raw).toBe(ehloReply());
    expect((await smtp.send("noop")).raw).toBe("250 OK");
    expect((await smtp.send("Help")).code).toBe(214);
    expect((await smtp.send("vRfY x")).code).toBe(252);
    expect((await smtp.send("mail from:<a@remote.example>")).raw).toBe("250 Accepted");
    expect((await smtp.send("rSeT")).raw).toBe("250 Flushed");
    expect((await smtp.send("MaIl FrOm:<a@remote.example>")).raw).toBe("250 Accepted");
    expect((await smtp.send("rcpt TO:<hello@mail.test>")).raw).toBe("250 Accepted");
    expect((await smtp.send("data")).raw).toBe("354 End data with <CR><LF>.<CR><LF>");
    smtp.write(`${plain({ messageId: id })}.\r\n`);
    expect((await smtp.read()).raw).toBe("250 OK: message queued");
    expect((await smtp.send("quit")).raw).toBe("221 Bye");
    expect((await mailByMessageId(server, id)).subject).toBe("Test message");
  });

  test("a bare LF ends a command line as well as CRLF", async () => {
    const smtp = await open(server);
    smtp.write("NOOP\n");
    expect((await smtp.read()).raw).toBe("250 OK");
    smtp.write("EHLO client.remote.example\nNOOP\r\n");
    expect((await smtp.read()).raw).toBe(ehloReply());
    expect((await smtp.read()).raw).toBe("250 OK");
  });

  test("the verb ends at the first space: a leading space or a tab makes the line unrecognized", async () => {
    const smtp = await open(server);
    expect((await smtp.send(" NOOP")).raw).toBe(UNRECOGNIZED);
    expect((await smtp.send("NOOP\tx")).raw).toBe(UNRECOGNIZED);
    expect((await smtp.send("EHLO\tclient.remote.example")).raw).toBe(UNRECOGNIZED);
    expect((await smtp.send("NOOP ")).raw).toBe("250 OK");
  });

  test("an empty line is an unrecognized command", async () => {
    const smtp = await open(server);
    expect((await smtp.send("")).raw).toBe(UNRECOGNIZED);
    smtp.write("\n");
    expect((await smtp.read()).raw).toBe(UNRECOGNIZED);
    expect((await smtp.send("   ")).raw).toBe(UNRECOGNIZED);
  });

  test("a word that starts with a verb is not that verb", async () => {
    const smtp = await session(server);
    for (const line of ["NOOPX", "QUITE", "EHLOO x", "MAILFROM:<a@remote.example>", "DATA.", "RSET;"]) {
      expect((await smtp.send(line)).raw).toBe(UNRECOGNIZED);
    }
  });
});

describe("unrecognized-command limit", () => {
  test("the tenth unrecognized command is answered 421 and the connection closed", async () => {
    const smtp = await open(server);
    for (let i = 1; i <= 9; i++) expect((await smtp.send(`BOGUS${i}`)).raw).toBe(UNRECOGNIZED);
    expect((await smtp.send("BOGUS10")).raw).toBe("421 Error: too many unrecognized commands");
    expect(await smtp.waitForClose(5000)).toBe(true);
  });

  test("recognized commands in between do not reset the count, nor does RSET or EHLO", async () => {
    const smtp = await open(server);
    for (let i = 1; i <= 5; i++) expect((await smtp.send("BOGUS")).raw).toBe(UNRECOGNIZED);
    expect((await smtp.send("NOOP")).raw).toBe("250 OK");
    expect((await smtp.send("EHLO client.remote.example")).code).toBe(250);
    expect((await smtp.send("RSET")).raw).toBe("250 Flushed");
    // Refusals of recognized commands are not counted either.
    expect((await smtp.send("DATA")).raw).toBe("503 Error: need RCPT command");
    expect((await smtp.send("MAIL FROM:nobrackets")).raw).toBe("501 Error: Bad sender address syntax");
    for (let i = 6; i <= 9; i++) expect((await smtp.send("")).raw).toBe(UNRECOGNIZED);
    expect((await smtp.send("BOGUS")).raw).toBe("421 Error: too many unrecognized commands");
    expect(await smtp.waitForClose(5000)).toBe(true);
  });

  test("a completed DATA transaction resets the count", async () => {
    const smtp = await session(server);
    for (let i = 1; i <= 9; i++) expect((await smtp.send("BOGUS")).raw).toBe(UNRECOGNIZED);
    const result = await smtp.transaction({ from: "a@remote.example", to: ["hello@mail.test"], data: plain() });
    expect(result.final.raw).toBe("250 OK: message queued");
    for (let i = 1; i <= 9; i++) expect((await smtp.send("BOGUS")).raw).toBe(UNRECOGNIZED);
    expect((await smtp.send("BOGUS")).raw).toBe("421 Error: too many unrecognized commands");
    expect(await smtp.waitForClose(5000)).toBe(true);
  });
});

describe("line length", () => {
  const LIMIT = 16 * 1024;

  test("a command line of exactly 16384 octets is accepted", async () => {
    const smtp = await open(server);
    const line = `NOOP ${"x".repeat(LIMIT - 5)}`;
    expect(line.length).toBe(LIMIT);
    expect((await smtp.send(line)).raw).toBe("250 OK");
    expect((await smtp.send("NOOP")).raw).toBe("250 OK");
  });

  test("one octet more is answered 421 and the connection closed", async () => {
    const smtp = await open(server);
    const line = `NOOP ${"x".repeat(LIMIT - 4)}`;
    expect(line.length).toBe(LIMIT + 1);
    expect((await smtp.send(line)).raw).toBe("421 Error: line too long");
    expect(await smtp.waitForClose(5000)).toBe(true);
  });

  test("an unterminated line is cut off as soon as it passes the limit", async () => {
    const smtp = await open(server);
    await smtp.writeFlushed("x".repeat(LIMIT));
    expect(await smtp.staysQuiet(300)).toBe(true);
    smtp.write("x");
    expect((await smtp.read()).raw).toBe("421 Error: line too long");
    expect(await smtp.waitForClose(5000)).toBe(true);
  });

  test("the limit covers the MAIL command too", async () => {
    const smtp = await session(server);
    const line = `MAIL FROM:<a@remote.example> X=${"y".repeat(LIMIT)}`;
    expect((await smtp.send(line)).raw).toBe("421 Error: line too long");
    expect(await smtp.waitForClose(5000)).toBe(true);
  });
});

describe("pipelining", () => {
  test("a whole session written at once is answered in order", async () => {
    const id = messageId("pipe");
    const smtp = await open(server);
    smtp.write(
      "EHLO client.remote.example\r\n" +
        "MAIL FROM:<a@remote.example>\r\n" +
        "RCPT TO:<hello@mail.test>\r\n" +
        "RCPT TO:<other@mail.test>\r\n" +
        "DATA\r\n" +
        plain({ messageId: id, subject: "Pipelined" }) +
        ".\r\n" +
        "NOOP\r\n" +
        "MAIL FROM:<b@remote.example>\r\n" +
        "RSET\r\n" +
        "QUIT\r\n"
    );
    const replies: string[] = [];
    for (;;) {
      const reply = await smtp.readOrClose();
      if (!reply) break;
      replies.push(reply.raw);
    }
    expect(replies).toEqual([
      ehloReply(),
      "250 Accepted",
      "250 Accepted",
      "250 Accepted",
      "354 End data with <CR><LF>.<CR><LF>",
      "250 OK: message queued",
      "250 OK",
      "250 Accepted",
      "250 Flushed",
      "221 Bye",
    ]);
    const mail = await mailByMessageId(server, id);
    expect(mail.subject).toBe("Pipelined");
    expect(mail.envelope_to).toEqual([{ address: "hello@mail.test" }, { address: "other@mail.test" }]);
  });

  test("a refused command in a pipelined group does not stop the ones behind it", async () => {
    const smtp = await session(server);
    smtp.write("RCPT TO:<hello@mail.test>\r\nBOGUS\r\nMAIL FROM:<a@remote.example>\r\nDATA\r\nNOOP\r\n");
    const replies = [await smtp.read(), await smtp.read(), await smtp.read(), await smtp.read(), await smtp.read()];
    expect(raws(replies)).toEqual([
      "503 Error: need MAIL command",
      UNRECOGNIZED,
      "250 Accepted",
      "503 Error: need RCPT command",
      "250 OK",
    ]);
  });

  test("two transactions pipelined back to back are both stored", async () => {
    const first = messageId("pipe2");
    const second = messageId("pipe2");
    const smtp = await session(server);
    const one = (id: string, from: string) =>
      `MAIL FROM:<${from}>\r\nRCPT TO:<hello@mail.test>\r\nDATA\r\n${plain({ messageId: id })}.\r\n`;
    smtp.write(one(first, "a@remote.example") + one(second, "b@remote.example"));
    const replies: string[] = [];
    for (let i = 0; i < 8; i++) replies.push((await smtp.read()).raw);
    const transaction = ["250 Accepted", "250 Accepted", "354 End data with <CR><LF>.<CR><LF>", "250 OK: message queued"];
    expect(replies).toEqual([...transaction, ...transaction]);
    expect((await mailByMessageId(server, first)).envelope_from).toEqual([{ address: "a@remote.example" }]);
    expect((await mailByMessageId(server, second)).envelope_from).toEqual([{ address: "b@remote.example" }]);
  });

  test("a flood of pipelined commands is answered in full and in order, however slowly the client reads", async () => {
    const smtp = await session(server);
    const count = 100_000;
    // The client stops reading, writes 600 kB of commands, and only later
    // reads the 800 kB of replies: the server has to hold back, not drop.
    smtp.pause();
    smtp.write(`${"NOOP\r\n".repeat(count)}RSET\r\n`);
    expect(await smtp.staysQuiet(500)).toBe(true);
    smtp.resume();
    const seen = new Map<string, number>();
    for (let i = 0; i < count; i++) {
      const reply = (await smtp.read()).raw;
      seen.set(reply, (seen.get(reply) ?? 0) + 1);
    }
    expect([...seen]).toEqual([["250 OK", count]]);
    expect((await smtp.read()).raw).toBe("250 Flushed");
  });

  test("a command split across writes is read once its line ends", async () => {
    const smtp = await open(server);
    await smtp.writeFlushed("NO");
    expect(await smtp.staysQuiet(200)).toBe(true);
    await smtp.writeFlushed("OP\r");
    expect(await smtp.staysQuiet(200)).toBe(true);
    smtp.write("\n");
    expect((await smtp.read()).raw).toBe("250 OK");
  });
});

describe("message text of a transaction that only has headers", () => {
  test("DATA ended at once stores an empty message", async () => {
    const smtp = await session(server);
    await smtp.send("MAIL FROM:<a@remote.example>");
    await smtp.send("RCPT TO:<x-empty@mail.test>");
    expect((await smtp.send("DATA")).raw).toBe("354 End data with <CR><LF>.<CR><LF>");
    smtp.write(crlf("."));
    expect((await smtp.read()).raw).toBe("250 OK: message queued");
    const [row] = await server.db.query("SELECT subject, text, html, from_address FROM mails WHERE envelope_to @> $1", [
      [{ address: "x-empty@mail.test" }],
    ]);
    expect(row).toEqual({ subject: "", text: "", html: "", from_address: null });
  });
});
