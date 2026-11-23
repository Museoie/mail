import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import fs from "fs";
import path from "path";
import { createUser, type TestServer, type TestUser } from "../harness";
import { b64, ImapClient, lit, openSession, utf8, type CommandResult } from "../harness/imap";
import { BINARY, FIXTURE_NAMES, type FixtureName } from "./messages";
import {
  served,
  storedParts,
  wireBody,
  wireHeaderSection,
  wireMessage,
  wireTopHeader,
  type StoredParts,
} from "./served";
import { crlf, deliverFixture, mailDomain, startImapServer, type DeliveredMail } from "./support";

let server: TestServer;
/** Holds every fixture in INBOX, in `FIXTURE_NAMES` order. Only PEEK fetches touch it. */
let user: TestUser;
let domain: string;
let imap: ImapClient;
const rows = {} as Record<FixtureName, DeliveredMail>;
const seqOf = (name: FixtureName) => FIXTURE_NAMES.indexOf(name) + 1;

/** The parts the server writes fixture `name` from. */
const partsOf = (name: FixtureName): StoredParts =>
  storedParts(served(name, domain), rows[name].mail_id, new Date(rows[name].date).toUTCString());

/** The single literal of a one-item FETCH answer, with the text around it. */
const single = (result: CommandResult) => {
  expect(result.status).toBe("OK");
  expect(result.responses).toHaveLength(2);
  const response = result.responses[0]!;
  return { raw: response.raw, literal: response.literals[0], literals: response.literals };
};

/** `* <seq> FETCH (<key> {n}CRLF<octets>)` */
const literalLine = (seq: number, key: string, content: string): string =>
  `* ${seq} FETCH (${key} {${content.length}}\r\n${content})`;

beforeAll(async () => {
  server = await startImapServer();
  user = await createUser(server, "fetbody");
  domain = mailDomain(server, user.username);
  for (const name of FIXTURE_NAMES) rows[name] = await deliverFixture(server, name, user.username);
  imap = await openSession(server, user.username, user.password, "INBOX");
}, 120_000);

afterAll(async () => {
  imap?.close();
  await server?.stop();
});

describe("BODY[]: the whole message", () => {
  test("a plain text message, octet for octet", async () => {
    const id = rows.plain.mail_id;
    const body = "SGVsbG8sCgpUaGlzIGlzIGEgcGxhaW4gdGV4dCBtZXNzYWdlLgotLSAKQ2Fyb2wK";
    const message = crlf(
      "Message-ID: <plain@fixtures.example>",
      "Date: Mon, 05 Jan 2026 09:15:30 GMT",
      'From: "Carol Sender" <carol@remote.example>',
      'To: "Hello There" <hello@fetbody.mail.test>',
      'Cc: "Dave" <dave@remote.example>',
      'Reply-To: "Replies" <replies@remote.example>',
      "Subject: Plain text message",
      "MIME-Version: 1.0",
      `Content-Type: multipart/alternative; boundary="boundary_${id}"`,
      "",
      `--boundary_${id}`,
      "Content-Type: text/plain; charset=utf-8",
      "Content-Transfer-Encoding: base64",
      "",
      body,
      `--boundary_${id}`,
      "Content-Type: text/html; charset=utf-8",
      "Content-Transfer-Encoding: base64",
      "",
      body,
      `--boundary_${id}--`,
      ""
    );
    expect(message).toHaveLength(826);
    const result = await imap.command("FETCH 1 (BODY.PEEK[])");
    expect(result.lines).toEqual([literalLine(1, "BODY[]", message), `${result.tag} OK FETCH completed`]);
    // The same octets the layout helper produces, which the tests below rely on.
    expect(wireMessage(partsOf("plain"))).toBe(message);
  });

  test("a message with an attachment, octet for octet", async () => {
    const id = rows.attachmentBase64.mail_id;
    const message = crlf(
      "Message-ID: <attachmentBase64@fixtures.example>",
      "Date: Thu, 08 Jan 2026 12:00:00 GMT",
      'From: "Carol Sender" <carol@remote.example>',
      "To: hello@fetbody.mail.test",
      "Subject: Attachment in base64",
      "MIME-Version: 1.0",
      `Content-Type: multipart/mixed; boundary="boundary_${id}"`,
      "",
      `--boundary_${id}`,
      `Content-Type: multipart/alternative; boundary="alt_${id}"`,
      "",
      `--alt_${id}`,
      "Content-Type: text/plain; charset=utf-8",
      "Content-Transfer-Encoding: base64",
      "",
      "U2VlIHRoZSBhdHRhY2hlZCByZXBvcnQu",
      `--alt_${id}`,
      "Content-Type: text/html; charset=utf-8",
      "Content-Transfer-Encoding: base64",
      "",
      "U2VlIHRoZSBhdHRhY2hlZCByZXBvcnQu",
      `--alt_${id}--`,
      `--boundary_${id}`,
      "Content-Type: application/pdf",
      "Content-Transfer-Encoding: base64",
      'Content-Disposition: attachment; filename="report.pdf"',
      "",
      // The attachment on one unfolded base64 line.
      BINARY.toString("base64"),
      `--boundary_${id}--`,
      ""
    );
    expect(message).toHaveLength(1369);
    const seq = seqOf("attachmentBase64");
    const result = await imap.command(`FETCH ${seq} (BODY.PEEK[])`);
    expect(result.lines).toEqual([literalLine(seq, "BODY[]", message), `${result.tag} OK FETCH completed`]);
    expect(wireMessage(partsOf("attachmentBase64"))).toBe(message);
  });

  for (const name of FIXTURE_NAMES) {
    test(`${name}: BODY[] is the message rebuilt from its stored parts, RFC822.SIZE octets long`, async () => {
      const expected = wireMessage(partsOf(name));
      expect(expected).toHaveLength(served(name, domain).size);
      const seq = seqOf(name);
      const { raw, literal } = single(await imap.command(`FETCH ${seq} (BODY.PEEK[])`));
      // Compared as a boolean first: a mismatch in a 2 MB literal should not be printed.
      if (literal !== expected) expect(literal?.slice(0, 4000)).toBe(expected.slice(0, 4000));
      expect(literal === expected).toBe(true);
      expect(raw.startsWith(`* ${seq} FETCH (BODY[] {${expected.length}}\r\n`)).toBe(true);
      expect(raw.endsWith(")")).toBe(true);
    });
  }

  test("RFC822 returns the same octets under its own name", async () => {
    const expected = wireMessage(partsOf("alternative"));
    // On its own connection: RFC822 has no PEEK form and would mark the message read.
    const other = await openSession(server, user.username, user.password);
    await other.command("EXAMINE INBOX");
    const result = await other.command("UID FETCH 3 (RFC822)");
    expect(result.untagged).toEqual([`* 3 FETCH (UID 3 RFC822 {${expected.length}}\r\n${expected})`]);
    await imap.command("STORE 3 -FLAGS.SILENT (\\Seen)");
    other.close();
  });

  test("text bodies are served as UTF-8 in base64, whatever charset and encoding they arrived in", async () => {
    const cases: [FixtureName, string][] = [
      ["encodedHeaders", "Grüße, Jörg\n"],
      ["utf8Body", "Emoji 😀, CJK 日本語, accents éàü and a euro €.\n"],
      ["latin1Body", "Café München Straße\n"],
      ["charsets", "Price: 5 € – “quoted”"],
    ];
    for (const [name, text] of cases) {
      const { literals } = single(await imap.command(`FETCH ${seqOf(name)} (BODY.PEEK[1])`));
      expect({ name, part: literals[0] }).toEqual({ name, part: `${b64(text)}\r\n` });
    }
    const { literals } = single(await imap.command(`FETCH ${seqOf("charsets")} (BODY.PEEK[2])`));
    expect(literals[0]).toBe(`${b64("<p>Привет</p>")}\r\n`);
  });

  test("non-ASCII header values are served as raw UTF-8, not RFC 2047 encoded", async () => {
    const { literal } = single(await imap.command(`FETCH ${seqOf("encodedHeaders")} (BODY.PEEK[HEADER.FIELDS (FROM TO SUBJECT)])`));
    expect(literal).toBe(
      utf8(
        crlf(
          'From: "Jörg Müller" <joerg@remote.example>',
          'To: "Zoë 日本" <hello@fetbody.mail.test>',
          "Subject: Grüße aus Köln ✉ and more",
          "",
          ""
        )
      )
    );
  });

  test("headers the server does not model are not served: In-Reply-To, References, Sender, X-*", async () => {
    const reply = single(await imap.command(`FETCH ${seqOf("reply")} (BODY.PEEK[HEADER.FIELDS (IN-REPLY-TO REFERENCES X-CUSTOM-HEADER)])`));
    expect(reply.literal).toBe("\r\n");
    const addresses = single(await imap.command(`FETCH ${seqOf("addresses")} (BODY.PEEK[HEADER.FIELDS (SENDER)])`));
    expect(addresses.literal).toBe("\r\n");
  });
});

describe("BODY[HEADER], RFC822.HEADER and BODY[TEXT]", () => {
  test("BODY[HEADER] is the header lines and the blank line, declared text/plain even for a multipart message", async () => {
    // NOTE: looks like a bug. BODY[HEADER] is not the header block of BODY[]:
    // its Content-Type is always "text/plain; charset=utf-8" (base64), while
    // BODY[] of the same message declares multipart/alternative with a
    // boundary. A client that assembles BODY[HEADER] + BODY[TEXT] gets
    // multipart framing under a text/plain header.
    const header = crlf(
      "Message-ID: <alternative@fixtures.example>",
      "Date: Wed, 07 Jan 2026 16:30:00 GMT",
      'From: "Carol Sender" <carol@remote.example>',
      "To: hello@fetbody.mail.test",
      "Subject: Alternative message",
      "MIME-Version: 1.0",
      "Content-Type: text/plain; charset=utf-8",
      "Content-Transfer-Encoding: base64",
      "",
      ""
    );
    const result = await imap.command("FETCH 3 (BODY.PEEK[HEADER])");
    expect(result.lines).toEqual([literalLine(3, "BODY[HEADER]", header), `${result.tag} OK FETCH completed`]);
    expect(wireHeaderSection(partsOf("alternative"))).toBe(header);
    expect(wireTopHeader(partsOf("alternative"))).not.toBe(header);
  });

  test("RFC822.HEADER returns the same octets under its own name", async () => {
    const header = wireHeaderSection(partsOf("attachmentBase64"));
    const seq = seqOf("attachmentBase64");
    const result = await imap.command(`FETCH ${seq} (RFC822.HEADER)`);
    expect(result.untagged).toEqual([literalLine(seq, "RFC822.HEADER", header)]);
  });

  for (const name of ["plain", "addresses", "encodedHeaders", "missingHeaders", "emptyBody", "large"] as const) {
    test(`${name}: BODY[HEADER]`, async () => {
      const { literal } = single(await imap.command(`FETCH ${seqOf(name)} (BODY.PEEK[HEADER])`));
      expect(literal).toBe(wireHeaderSection(partsOf(name)));
    });
  }

  test("BODY[TEXT] is everything after the header block of BODY[], plus the closing CRLF", async () => {
    const id = rows.alternative.mail_id;
    const text = crlf(
      `--boundary_${id}`,
      "Content-Type: text/plain; charset=utf-8",
      "Content-Transfer-Encoding: base64",
      "",
      "VGhlIHBsYWluIGFsdGVybmF0aXZlLg==",
      `--boundary_${id}`,
      "Content-Type: text/html; charset=utf-8",
      "Content-Transfer-Encoding: base64",
      "",
      "PHA+VGhlIDxpPkhUTUw8L2k+IGFsdGVybmF0aXZlLjwvcD4=",
      `--boundary_${id}--`,
      ""
    );
    const result = await imap.command("FETCH 3 (BODY.PEEK[TEXT])");
    expect(result.untagged).toEqual([literalLine(3, "BODY[TEXT]", text)]);
    expect(`${wireBody(partsOf("alternative"))}\r\n`).toBe(text);
  });

  for (const name of ["plain", "attachmentQp", "nested", "utf8Body", "longLines"] as const) {
    test(`${name}: BODY[TEXT], and header block + BODY[TEXT] is BODY[]`, async () => {
      const parts = partsOf(name);
      const { literal } = single(await imap.command(`FETCH ${seqOf(name)} (BODY.PEEK[TEXT])`));
      expect(literal).toBe(`${wireBody(parts)}\r\n`);
      expect(wireTopHeader(parts) + literal).toBe(wireMessage(parts));
    });
  }

  test("a message with no body has BODY[TEXT] NIL, and its BODY[] is the header block plus one CRLF", async () => {
    const seq = seqOf("emptyBody");
    const parts = partsOf("emptyBody");
    const result = await imap.command(`FETCH ${seq} (BODY.PEEK[TEXT] BODY.PEEK[])`);
    const message = `${wireHeaderSection(parts)}\r\n`;
    expect(result.untagged).toEqual([`* ${seq} FETCH (BODY[TEXT] NIL BODY[] {${message.length}}\r\n${message})`]);
    expect(message).toHaveLength(273);
  });
});

describe("BODY[HEADER.FIELDS] and BODY[HEADER.FIELDS.NOT]", () => {
  const fields = async (seq: number, section: string) => (await imap.command(`FETCH ${seq} (BODY.PEEK[${section}])`)).untagged;

  test("returns the named fields in the message's own order, with the blank line", async () => {
    const content = crlf('From: "Carol Sender" <carol@remote.example>', "Subject: Alternative message", "", "");
    expect(await fields(3, "HEADER.FIELDS (Subject From)")).toEqual([
      literalLine(3, "BODY[HEADER.FIELDS (SUBJECT FROM)]", content),
    ]);
  });

  test("echoes the field names in upper case, as asked, and matches them case-insensitively", async () => {
    const content = crlf("Date: Wed, 07 Jan 2026 16:30:00 GMT", "", "");
    expect(await fields(3, "header.fields (date)")).toEqual([literalLine(3, "BODY[HEADER.FIELDS (DATE)]", content)]);
    expect(await fields(3, "HEADER.FIELDS (dAtE X-None)")).toEqual([
      literalLine(3, "BODY[HEADER.FIELDS (DATE X-NONE)]", content),
    ]);
  });

  test("a field list that matches nothing is a lone CRLF", async () => {
    expect(await fields(3, "HEADER.FIELDS (X-None)")).toEqual(["* 3 FETCH (BODY[HEADER.FIELDS (X-NONE)] {2}\r\n\r\n)"]);
    expect(await fields(3, "HEADER.FIELDS ()")).toEqual(["* 3 FETCH (BODY[HEADER.FIELDS ()] {2}\r\n\r\n)"]);
  });

  test("the fields a mail client asks for when listing a mailbox", async () => {
    const section = "HEADER.FIELDS (DATE FROM SUBJECT TO CC MESSAGE-ID REFERENCES CONTENT-TYPE)";
    const content = crlf(
      "Message-ID: <plain@fixtures.example>",
      "Date: Mon, 05 Jan 2026 09:15:30 GMT",
      'From: "Carol Sender" <carol@remote.example>',
      'To: "Hello There" <hello@fetbody.mail.test>',
      'Cc: "Dave" <dave@remote.example>',
      "Subject: Plain text message",
      "Content-Type: text/plain; charset=utf-8",
      "",
      ""
    );
    expect(await fields(1, section)).toEqual([literalLine(1, `BODY[${section}]`, content)]);
  });

  test("HEADER.FIELDS.NOT returns every other field", async () => {
    const content = crlf(
      "Message-ID: <alternative@fixtures.example>",
      "Date: Wed, 07 Jan 2026 16:30:00 GMT",
      "To: hello@fetbody.mail.test",
      "MIME-Version: 1.0",
      "Content-Transfer-Encoding: base64",
      "",
      ""
    );
    expect(await fields(3, "HEADER.FIELDS.NOT (Subject From Content-Type)")).toEqual([
      literalLine(3, "BODY[HEADER.FIELDS.NOT (SUBJECT FROM CONTENT-TYPE)]", content),
    ]);
  });

  test("HEADER.FIELDS.NOT with an empty list is the whole header section", async () => {
    const { literal } = single(await imap.command("FETCH 3 (BODY.PEEK[HEADER.FIELDS.NOT ()])"));
    expect(literal).toBe(wireHeaderSection(partsOf("alternative")));
  });
});

describe("BODY[<part>]: numbered parts", () => {
  test("a text + HTML message has parts 1 and 2: the base64 text, with a CRLF added", async () => {
    // NOTE: the literal of a part is 2 octets longer than the size
    // BODYSTRUCTURE reports for it (32 and 48 here): the CRLF that ends the
    // part's line in the message is included in BODY[n].
    const result = await imap.command("FETCH 3 (BODY.PEEK[1] BODY.PEEK[2])");
    expect(result.untagged).toEqual([
      "* 3 FETCH (BODY[1] {34}\r\nVGhlIHBsYWluIGFsdGVybmF0aXZlLg==\r\n " +
        "BODY[2] {50}\r\nPHA+VGhlIDxpPkhUTUw8L2k+IGFsdGVybmF0aXZlLjwvcD4=\r\n)",
    ]);
  });

  test("<part>.TEXT is the same as the part; <part>.MIME and <part>.HEADER are its two MIME header lines", async () => {
    const plainMime = crlf("Content-Type: text/plain; charset=utf-8", "Content-Transfer-Encoding: base64", "", "");
    const htmlMime = crlf("Content-Type: text/html; charset=utf-8", "Content-Transfer-Encoding: base64", "", "");
    const result = await imap.command(
      "FETCH 3 (BODY.PEEK[1.TEXT] BODY.PEEK[1.MIME] BODY.PEEK[1.HEADER] BODY.PEEK[2.MIME])"
    );
    expect(result.untagged).toEqual([
      "* 3 FETCH (BODY[1.TEXT] {34}\r\nVGhlIHBsYWluIGFsdGVybmF0aXZlLg==\r\n " +
        `BODY[1.MIME] {${plainMime.length}}\r\n${plainMime} ` +
        `BODY[1.HEADER] {${plainMime.length}}\r\n${plainMime} ` +
        `BODY[2.MIME] {${htmlMime.length}}\r\n${htmlMime})`,
    ]);
  });

  test("a part that does not exist is left out of the answer, not answered NIL", async () => {
    expect((await imap.command("FETCH 3 (BODY.PEEK[3])")).untagged).toEqual(["* 3 FETCH ()"]);
    expect((await imap.command("FETCH 3 (BODY.PEEK[0])")).untagged).toEqual(["* 3 FETCH ()"]);
    expect((await imap.command("FETCH 3 (BODY.PEEK[1.1])")).untagged).toEqual(["* 3 FETCH ()"]);
    expect((await imap.command("FETCH 3 (BODY.PEEK[3.MIME])")).untagged).toEqual(["* 3 FETCH ()"]);
    expect((await imap.command("FETCH 3 (UID BODY.PEEK[9] FLAGS)")).untagged).toEqual(["* 3 FETCH (UID 3 FLAGS ())"]);
  });

  test("with attachments the bodies are 1.1 and 1.2 and the attachments follow from 2", async () => {
    const seq = seqOf("attachmentQp");
    const expected = served("attachmentQp", domain);
    const result = await imap.command(
      `FETCH ${seq} (BODY.PEEK[1.1] BODY.PEEK[1.2] BODY.PEEK[2] BODY.PEEK[3] BODY.PEEK[4])`
    );
    const text = `${b64(expected.text)}\r\n`;
    const csv = `${expected.attachments[0]!.bytes.toString("base64")}\r\n`;
    const notes = `${expected.attachments[1]!.bytes.toString("base64")}\r\n`;
    expect(result.untagged).toEqual([
      `* ${seq} FETCH (BODY[1.1] {${text.length}}\r\n${text} BODY[1.2] {${text.length}}\r\n${text} ` +
        `BODY[2] {${csv.length}}\r\n${csv} BODY[3] {${notes.length}}\r\n${notes})`,
    ]);
    // The quoted-printable attachment arrived as text and is served as its decoded octets.
    expect(Buffer.from(csv, "base64").toString("utf8")).toBe(`name;city\r\nRenée;Zürich\r\nlong;${"x".repeat(90)}`);
    expect(Buffer.from(notes, "base64").toString("utf8")).toBe("Plain notes.\r\nSecond line.");
  });

  test("the multipart/alternative wrapper itself, part 1, is not served", async () => {
    const seq = seqOf("attachmentBase64");
    expect((await imap.command(`FETCH ${seq} (BODY.PEEK[1])`)).untagged).toEqual([`* ${seq} FETCH ()`]);
    expect((await imap.command(`FETCH ${seq} (BODY.PEEK[1.TEXT])`)).untagged).toEqual([`* ${seq} FETCH ()`]);
  });

  test("1.MIME of a message with attachments is the text part's header, not the wrapper's", async () => {
    // NOTE: part 1 is multipart/alternative in BODYSTRUCTURE, but 1.MIME
    // answers with the header of 1.1.
    const seq = seqOf("attachmentBase64");
    const plainMime = crlf("Content-Type: text/plain; charset=utf-8", "Content-Transfer-Encoding: base64", "", "");
    const htmlMime = crlf("Content-Type: text/html; charset=utf-8", "Content-Transfer-Encoding: base64", "", "");
    const result = await imap.command(`FETCH ${seq} (BODY.PEEK[1.MIME] BODY.PEEK[1.1.MIME] BODY.PEEK[1.2.MIME])`);
    expect(result.untagged).toEqual([
      `* ${seq} FETCH (BODY[1.MIME] {${plainMime.length}}\r\n${plainMime} ` +
        `BODY[1.1.MIME] {${plainMime.length}}\r\n${plainMime} ` +
        `BODY[1.2.MIME] {${htmlMime.length}}\r\n${htmlMime})`,
    ]);
  });

  test("an attachment part: its MIME header, and its octets in base64 on one line", async () => {
    const seq = seqOf("attachmentBase64");
    const mime = crlf(
      "Content-Type: application/pdf",
      "Content-Transfer-Encoding: base64",
      'Content-Disposition: attachment; filename="report.pdf"',
      "",
      ""
    );
    const data = `${BINARY.toString("base64")}\r\n`;
    const result = await imap.command(`FETCH ${seq} (BODY.PEEK[2.MIME] BODY.PEEK[2.HEADER] BODY.PEEK[2] BODY.PEEK[2.TEXT])`);
    expect(result.untagged).toEqual([
      `* ${seq} FETCH (BODY[2.MIME] {${mime.length}}\r\n${mime} BODY[2.HEADER] {${mime.length}}\r\n${mime} ` +
        `BODY[2] {${data.length}}\r\n${data} BODY[2.TEXT] {${data.length}}\r\n${data})`,
    ]);
    // Every octet value survives the round trip.
    expect(Buffer.from(data, "base64").equals(BINARY)).toBe(true);
  });

  test("nested multiparts are flattened: bodies at 1.1 and 1.2, every other part an attachment", async () => {
    const seq = seqOf("nested");
    const expected = served("nested", domain);
    const result = await imap.command(`FETCH ${seq} (BODY.PEEK[1.2] BODY.PEEK[2.MIME] BODY.PEEK[2] BODY.PEEK[3.MIME] BODY.PEEK[1.2.1])`);
    const html = `${b64(expected.html)}\r\n`;
    const logoMime = crlf(
      "Content-Type: image/png",
      "Content-Transfer-Encoding: base64",
      'Content-Disposition: attachment; filename="logo.png"',
      "",
      ""
    );
    const dataMime = crlf(
      "Content-Type: application/octet-stream",
      "Content-Transfer-Encoding: base64",
      'Content-Disposition: attachment; filename="data.bin"',
      "",
      ""
    );
    const logo = `${expected.attachments[0]!.bytes.toString("base64")}\r\n`;
    expect(result.untagged).toEqual([
      `* ${seq} FETCH (BODY[1.2] {${html.length}}\r\n${html} BODY[2.MIME] {${logoMime.length}}\r\n${logoMime} ` +
        `BODY[2] {${logo.length}}\r\n${logo} BODY[3.MIME] {${dataMime.length}}\r\n${dataMime})`,
    ]);
    // The inline image is in the HTML as a data: URI.
    expect(expected.html).toContain('<img src="data:image/png;base64,');
  });

  test("a message with no body has no part 1", async () => {
    const seq = seqOf("emptyBody");
    expect((await imap.command(`FETCH ${seq} (BODY.PEEK[1] BODY.PEEK[1.MIME])`)).untagged).toEqual([`* ${seq} FETCH ()`]);
  });
});

describe("BODY[...]<start.count>: partial fetches", () => {
  const whole = () => wireMessage(partsOf("alternative"));
  const item = async (seq: number, spec: string) => (await imap.command(`FETCH ${seq} (BODY.PEEK${spec})`)).untagged;

  test("returns count octets from start, answered with the origin only", async () => {
    expect(await item(3, "[]<0.20>")).toEqual(["* 3 FETCH (BODY[]<0> {20}\r\nMessage-ID: <alterna)"]);
    expect(await item(3, "[]<10.20>")).toEqual(["* 3 FETCH (BODY[]<10> {20}\r\n: <alternative@fixtu)"]);
    expect(await item(3, "[]<100.50>")).toEqual([`* 3 FETCH (BODY[]<100> {50}\r\n${whole().slice(100, 150)})`]);
  });

  test("a count running past the end is cut at the end of the message, before the literal's closing CRLF", async () => {
    const message = whole();
    expect(message).toHaveLength(689);
    // 687 addressable octets: the CRLF a whole BODY[] ends with is not part of the range.
    expect(await item(3, "[]<680.100>")).toEqual([`* 3 FETCH (BODY[]<680> {7}\r\n${message.slice(680, 687)})`]);
    expect(await item(3, "[]<0.100000>")).toEqual([`* 3 FETCH (BODY[]<0> {687}\r\n${message.slice(0, 687)})`]);
  });

  test("a start at or past the end is NIL, with no origin", async () => {
    expect(await item(3, "[]<687.100>")).toEqual(["* 3 FETCH (BODY[] NIL)"]);
    expect(await item(3, "[]<689.1>")).toEqual(["* 3 FETCH (BODY[] NIL)"]);
    expect(await item(3, "[]<999999.1>")).toEqual(["* 3 FETCH (BODY[] NIL)"]);
  });

  test("a count of zero is an empty literal", async () => {
    expect(await item(3, "[]<0.0>")).toEqual(["* 3 FETCH (BODY[]<0> {0}\r\n)"]);
  });

  test("a malformed range is ignored and the whole section is returned", async () => {
    const message = whole();
    for (const spec of ["[]<5>", "[]<5.>", "[]<.5>", "[]<a.b>"]) {
      expect({ spec, answer: await item(3, spec) }).toEqual({
        spec,
        answer: [`* 3 FETCH (BODY[] {689}\r\n${message})`],
      });
    }
  });

  test("applies to HEADER, HEADER.FIELDS, TEXT and numbered parts, each within its own octets", async () => {
    const header = wireHeaderSection(partsOf("alternative"));
    const text = wireBody(partsOf("alternative"));
    expect(await item(3, "[HEADER]<5.10>")).toEqual(["* 3 FETCH (BODY[HEADER]<5> {10}\r\nge-ID: <al)"]);
    expect(await item(3, "[HEADER]<0.100000>")).toEqual([`* 3 FETCH (BODY[HEADER]<0> {${header.length}}\r\n${header})`]);
    expect(await item(3, "[HEADER]<5000.10>")).toEqual(["* 3 FETCH (BODY[HEADER] NIL)"]);
    expect(await item(3, "[HEADER.FIELDS (Subject)]<3.4>")).toEqual([
      "* 3 FETCH (BODY[HEADER.FIELDS (SUBJECT)]<3> {4}\r\nject)",
    ]);
    expect(await item(3, "[TEXT]<3.12>")).toEqual([`* 3 FETCH (BODY[TEXT]<3> {12}\r\n${text.slice(3, 15)})`]);
    expect(await item(3, "[TEXT]<0.100000>")).toEqual([`* 3 FETCH (BODY[TEXT]<0> {${text.length}}\r\n${text})`]);
    expect(await item(3, "[TEXT]<100000.12>")).toEqual(["* 3 FETCH (BODY[TEXT] NIL)"]);
    expect(await item(3, "[1]<2.5>")).toEqual(["* 3 FETCH (BODY[1]<2> {5}\r\nhlIHB)"]);
    // A partial part ends where its base64 ends: 32 octets, not the 34 of the whole BODY[1].
    expect(await item(3, "[1]<2.500>")).toEqual(["* 3 FETCH (BODY[1]<2> {30}\r\nhlIHBsYWluIGFsdGVybmF0aXZlLg==)"]);
    expect(await item(3, "[1]<32.5>")).toEqual(["* 3 FETCH (BODY[1] NIL)"]);
    expect(await item(3, "[1.MIME]<14.10>")).toEqual(["* 3 FETCH (BODY[1.MIME]<14> {10}\r\ntext/plain)"]);
  });

  test("a range inside an attachment starts at any octet of its base64", async () => {
    const seq = seqOf("attachmentBase64");
    const data = BINARY.toString("base64");
    expect(data).toHaveLength(344);
    for (const [start, count] of [[0, 10], [1, 7], [2, 9], [3, 5], [101, 50], [340, 100]] as const) {
      expect({ start, answer: await item(seq, `[2]<${start}.${count}>`) }).toEqual({
        start,
        answer: [
          `* ${seq} FETCH (BODY[2]<${start}> {${data.slice(start, start + count).length}}\r\n${data.slice(start, start + count)})`,
        ],
      });
    }
  });

  test("a client can read a whole message in slices that add up to BODY[] without its closing CRLF", async () => {
    const seq = seqOf("nested");
    const message = wireMessage(partsOf("nested"));
    let assembled = "";
    for (let start = 0; ; start += 500) {
      const result = await imap.command(`FETCH ${seq} (BODY.PEEK[]<${start}.500>)`);
      const literal = result.responses[0]!.literals[0];
      if (literal === undefined) {
        expect(result.untagged).toEqual([`* ${seq} FETCH (BODY[] NIL)`]);
        break;
      }
      expect(result.untagged[0]).toStartWith(`* ${seq} FETCH (BODY[]<${start}> {${literal.length}}\r\n`);
      assembled += literal;
    }
    expect(assembled).toBe(message.slice(0, -2));
  });
});

describe("BODY[]: a large message", () => {
  test("slices deep inside a 2 MB attachment are the right octets", async () => {
    const seq = seqOf("large");
    const message = wireMessage(partsOf("large"));
    expect(message).toHaveLength(2_293_992);
    for (const [start, count] of [
      [0, 393_216],
      [293_000, 4096],
      [393_216, 393_216],
      [1_000_001, 65_537],
      [2_293_000, 5000],
    ] as const) {
      const result = await imap.command(`FETCH ${seq} (BODY.PEEK[]<${start}.${count}>)`);
      const literal = result.responses[0]!.literals[0]!;
      const expected = message.slice(start, Math.min(start + count, message.length - 2));
      expect({ start, same: literal === expected, length: literal.length }).toEqual({
        start,
        same: true,
        length: expected.length,
      });
    }
  });

  test("the text part and the attachment part come back whole", async () => {
    const seq = seqOf("large");
    const expected = served("large", domain);
    const text = single(await imap.command(`FETCH ${seq} (BODY.PEEK[1.1])`));
    expect(text.literal === `${b64(expected.text)}\r\n`).toBe(true);
    const attachment = single(await imap.command(`FETCH ${seq} (BODY.PEEK[2])`));
    expect(attachment.literal?.length).toBe(2_000_002);
    expect(Buffer.from(attachment.literal!, "base64").equals(expected.attachments[0]!.bytes)).toBe(true);
  });

  test("pipelined slice requests are answered in order, each literal intact", async () => {
    const seq = seqOf("large");
    const message = wireMessage(partsOf("large"));
    const starts = [0, 500_000, 1_000_000, 1_500_000, 2_000_000];
    imap.write(starts.map((start, i) => `P${i} FETCH ${seq} (BODY.PEEK[]<${start}.100000>)\r\n`).join(""));
    for (const [i, start] of starts.entries()) {
      const result = await imap.readResult(`P${i}`);
      expect(result.lines).toHaveLength(2);
      expect(result.responses[0]!.literals[0] === message.slice(start, start + 100_000)).toBe(true);
    }
  });
});

describe("BODY[]: stored shapes a delivery does not produce", () => {
  let other: TestUser;
  let own: ImapClient;
  const shaped = {} as Record<"alternative" | "html" | "attachmentBase64" | "attachmentQp" | "inlineImage", DeliveredMail>;
  const names = ["alternative", "html", "attachmentBase64", "attachmentQp", "inlineImage"] as const;

  beforeAll(async () => {
    // The same fixtures for another account, then reshaped in the table.
    other = await createUser(server, "shapes1");
    for (const name of names) shaped[name] = await deliverFixture(server, name, other.username);
    const set = (name: (typeof names)[number], columns: string) =>
      server.db.query(`UPDATE mails SET ${columns} WHERE mail_id = $1`, [shaped[name].mail_id]);
    await set("alternative", "html = ''"); // text only
    await set("html", "text = ''"); // HTML only
    await set("attachmentBase64", "html = ''"); // text + attachment
    await set("attachmentQp", "text = '', html = ''"); // attachments only
    await set("inlineImage", "text = ''"); // HTML + attachment
    own = await openSession(server, other.username, other.password, "INBOX");
  });

  afterAll(() => own?.close());

  const d = () => mailDomain(server, other.username);
  const parts = (name: (typeof names)[number], overrides: Partial<StoredParts>): StoredParts => ({
    ...storedParts(served(name, d()), shaped[name].mail_id),
    ...overrides,
  });
  const fetchOne = async (seq: number, items: string) => (await own.command(`FETCH ${seq} (${items})`)).untagged;

  test("text only: a single text/plain part, the header block declaring it", async () => {
    const message = wireMessage(parts("alternative", { html: "" }));
    expect(await fetchOne(1, "BODYSTRUCTURE BODY BODY.PEEK[]")).toEqual([
      '* 1 FETCH (BODYSTRUCTURE ("TEXT" "PLAIN" ("CHARSET" "UTF-8") NIL NIL "BASE64" 32 1 NIL NIL NIL NIL) ' +
        'BODY ("TEXT" "PLAIN" ("CHARSET" "UTF-8") NIL NIL "BASE64" 32 1) ' +
        `BODY[] {${message.length}}\r\n${message})`,
    ]);
    expect(message.endsWith("Content-Transfer-Encoding: base64\r\n\r\nVGhlIHBsYWluIGFsdGVybmF0aXZlLg==\r\n\r\n")).toBe(true);
    expect(await fetchOne(1, "BODY.PEEK[TEXT] BODY.PEEK[1] BODY.PEEK[2] BODY.PEEK[1.MIME]")).toEqual([
      "* 1 FETCH (BODY[TEXT] {36}\r\nVGhlIHBsYWluIGFsdGVybmF0aXZlLg==\r\n\r\n " +
        "BODY[1] {34}\r\nVGhlIHBsYWluIGFsdGVybmF0aXZlLg==\r\n " +
        "BODY[1.MIME] {78}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n)",
    ]);
  });

  test("HTML only: a single text/html part", async () => {
    const message = wireMessage(parts("html", { text: "" }));
    const html = b64(served("html", d()).html);
    expect(await fetchOne(2, "BODYSTRUCTURE BODY.PEEK[]")).toEqual([
      '* 2 FETCH (BODYSTRUCTURE ("TEXT" "HTML" ("CHARSET" "UTF-8") NIL NIL "BASE64" 104 1 NIL NIL NIL NIL) ' +
        `BODY[] {${message.length}}\r\n${message})`,
    ]);
    expect(message).toContain("\r\nContent-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n");
    expect(await fetchOne(2, "BODY.PEEK[1] BODY.PEEK[1.MIME]")).toEqual([
      `* 2 FETCH (BODY[1] {${html.length + 2}}\r\n${html}\r\n ` +
        "BODY[1.MIME] {77}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n)",
    ]);
  });

  test("text and an attachment: multipart/mixed with the text as part 1", async () => {
    const message = wireMessage(parts("attachmentBase64", { html: "" }));
    expect(await fetchOne(3, "BODYSTRUCTURE BODY.PEEK[]")).toEqual([
      '* 3 FETCH (BODYSTRUCTURE (("TEXT" "PLAIN" ("CHARSET" "UTF-8") NIL NIL "BASE64" 32 1 NIL NIL NIL NIL)' +
        '("application" "pdf" ("NAME" "report.pdf") NIL NIL "BASE64" 344 NIL ("ATTACHMENT" ("FILENAME" "report.pdf")) NIL NIL)' +
        ' "mixed" NIL NIL NIL NIL) ' +
        `BODY[] {${message.length}}\r\n${message})`,
    ]);
    expect(await fetchOne(3, "BODY.PEEK[1] BODY.PEEK[1.1] BODY.PEEK[2]")).toEqual([
      "* 3 FETCH (BODY[1] {34}\r\nU2VlIHRoZSBhdHRhY2hlZCByZXBvcnQu\r\n " +
        `BODY[2] {346}\r\n${BINARY.toString("base64")}\r\n)`,
    ]);
  });

  test("attachments only: multipart/mixed whose first attachment is part 1", async () => {
    const message = wireMessage(parts("attachmentQp", { text: "", html: "" }));
    const [csv, notes] = served("attachmentQp", d()).attachments.map((a) => a.bytes.toString("base64"));
    expect(await fetchOne(4, "BODYSTRUCTURE BODY.PEEK[]")).toEqual([
      "* 4 FETCH (BODYSTRUCTURE (" +
        '("text" "csv" ("NAME" "people.csv") NIL NIL "BASE64" 164 1 NIL ("ATTACHMENT" ("FILENAME" "people.csv")) NIL NIL)' +
        '("text" "plain" ("NAME" "notes.txt") NIL NIL "BASE64" 36 1 NIL ("ATTACHMENT" ("FILENAME" "notes.txt")) NIL NIL)' +
        ' "mixed" NIL NIL NIL NIL) ' +
        `BODY[] {${message.length}}\r\n${message})`,
    ]);
    expect(await fetchOne(4, "BODY.PEEK[1] BODY.PEEK[2] BODY.PEEK[3]")).toEqual([
      `* 4 FETCH (BODY[1] {${csv!.length + 2}}\r\n${csv}\r\n BODY[2] {${notes!.length + 2}}\r\n${notes}\r\n)`,
    ]);
  });

  test("HTML and an attachment: multipart/mixed with the HTML as part 1", async () => {
    const message = wireMessage(parts("inlineImage", { text: "" }));
    const [answer] = await fetchOne(5, "BODYSTRUCTURE BODY.PEEK[]");
    expect(answer).toBe(
      '* 5 FETCH (BODYSTRUCTURE (("TEXT" "HTML" ("CHARSET" "UTF-8") NIL NIL "BASE64" 196 1 NIL NIL NIL NIL)' +
        '("image" "png" ("NAME" "attachment") NIL NIL "BASE64" 96 NIL ("ATTACHMENT" ("FILENAME" "attachment")) NIL NIL)' +
        ' "mixed" NIL NIL NIL NIL) ' +
        `BODY[] {${message.length}}\r\n${message})`
    );
  });

  test("RFC822.SIZE keeps the value stored before the row changed", async () => {
    // NOTE: the size is cached on the row the first time it is served and is
    // not recomputed when the stored parts change, so it can disagree with
    // the BODY[] literal. (Nothing in the product edits a stored mail today.)
    const before = await fetchOne(1, "RFC822.SIZE");
    expect(before).toEqual(["* 1 FETCH (RFC822.SIZE 689)"]);
    const message = wireMessage(parts("alternative", { html: "" }));
    expect(message.length).not.toBe(689);
  });

  test("an attachment whose file is gone is served as a notice of the same advertised length", async () => {
    // The stored attachment of message 3 disappears from the attachment directory.
    const [row] = await server.db.query("SELECT attachments FROM mails WHERE mail_id = $1", [
      shaped.attachmentBase64.mail_id,
    ]);
    const fileId = row!.attachments[0].content.data as string;
    const file = path.join(server.cwd, "attachments", fileId);
    expect(fs.existsSync(file)).toBe(true);
    fs.rmSync(file);

    // BODYSTRUCTURE still describes the stored size; the part itself is the notice.
    const notice = b64("Attachment data not found");
    expect(await fetchOne(3, "BODY.PEEK[2]")).toEqual([`* 3 FETCH (BODY[2] {${notice.length + 2}}\r\n${notice}\r\n)`]);
    const [structure] = await fetchOne(3, "BODYSTRUCTURE");
    expect(structure).toContain('("application" "pdf" ("NAME" "report.pdf") NIL NIL "BASE64" 344 NIL');
    const whole = await own.command("FETCH 3 (BODY.PEEK[])");
    const literal = whole.responses[0]!.literals[0]!;
    expect(literal).toContain(`filename="report.pdf"\r\n\r\n${notice}\r\n--boundary_`);
    expect(whole.status).toBe("OK");
  });
});

describe("BODY[]: messages stored by APPEND", () => {
  test("an appended plain text message is stored and served like a delivered one, its Message-ID without brackets", async () => {
    const appender = await createUser(server, "appendr");
    const own = await openSession(server, appender.username, appender.password);
    const message = crlf(
      "From: Me <me@appendr.mail.test>",
      "To: You <you@remote.example>",
      "Subject: Appended",
      "Date: Thu, 22 Jan 2026 10:00:00 +0000",
      "Message-ID: <appended-plain@fixtures.example>",
      "X-Mailer: not kept",
      "",
      "Appended body.",
      ""
    );
    expect((await own.command(["APPEND INBOX ", lit(message)])).status).toBe("OK");
    await own.command("SELECT INBOX");
    const [row] = await server.db.query("SELECT mail_id FROM mails WHERE user_id = $1", [appender.id]);
    // Not the octets that were appended: the message is parsed, stored as its
    // parts (the text doubling as the HTML body) and rebuilt on FETCH.
    const rebuilt = wireMessage({
      mailId: row!.mail_id,
      headers: [
        "Message-ID: appended-plain@fixtures.example",
        "Date: Thu, 22 Jan 2026 10:00:00 GMT",
        'From: "Me" <me@appendr.mail.test>',
        'To: "You" <you@remote.example>',
        "Subject: Appended",
      ],
      text: "Appended body.\n",
      html: "Appended body.\n",
    });
    const result = await own.command("FETCH 1 (BODYSTRUCTURE ENVELOPE RFC822.SIZE BODY.PEEK[])");
    expect(result.untagged).toEqual([
      '* 1 FETCH (BODYSTRUCTURE (("TEXT" "PLAIN" ("CHARSET" "UTF-8") NIL NIL "BASE64" 20 1 NIL NIL NIL NIL)' +
        '("TEXT" "HTML" ("CHARSET" "UTF-8") NIL NIL "BASE64" 20 1 NIL NIL NIL NIL) "alternative" NIL NIL NIL NIL) ' +
        'ENVELOPE ("Thu, 22 Jan 2026 10:00:00 GMT" "Appended" (("Me" NIL "me" "appendr.mail.test")) ' +
        '(("Me" NIL "me" "appendr.mail.test")) NIL (("You" NIL "you" "remote.example")) NIL NIL NIL ' +
        '"appended-plain@fixtures.example") ' +
        `RFC822.SIZE ${rebuilt.length} ` +
        `BODY[] {${rebuilt.length}}\r\n${rebuilt})`,
    ]);
    expect(rebuilt).not.toBe(message);
    own.close();
  });
});
