/**
 * Fixed messages for the FETCH and SEARCH files: one per MIME shape, each with
 * a fixed Date and Message-ID so what the server stores, and so what IMAP
 * serves, is the same on every run. `message(name, domain)` returns the bytes
 * to deliver; non-ASCII octets are written as latin1 characters, so a fixture
 * is exactly the octets that travel.
 *
 * Not a test file.
 */

export interface Fixture {
  /** The Message-ID header value, brackets included. */
  messageId: string;
  /** The message as delivered, CRLF line endings, one char per octet. */
  data: string;
}

const lines = (...parts: string[]): string => parts.join("\r\n");

/** UTF-8 octets of `text` as a latin1 string. */
const u = (text: string): string => Buffer.from(text, "utf8").toString("latin1");

const b64Lines = (data: Buffer, width = 76): string =>
  (data.toString("base64").match(new RegExp(`.{1,${width}}`, "g")) ?? []).join("\r\n");

/** Bytes of a tiny valid PNG (1x1, transparent). */
export const PNG = Buffer.from(
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==",
  "base64"
);

/** Bytes standing in for a PDF: every octet value once, so no decoder may mangle any. */
export const BINARY = Buffer.from(Array.from({ length: 256 }, (_, i) => i));

/** The body of the `large` fixture's attachment: 1.5 MB of a repeating pattern. */
export const LARGE_ATTACHMENT = Buffer.alloc(1_500_000, "0123456789abcdef");

/** The text body of the `large` fixture: 3000 numbered lines. */
export const LARGE_TEXT = Array.from({ length: 3000 }, (_, i) => `Line ${i + 1} of the large message body.`).join(
  "\r\n"
);

export const FIXTURE_NAMES = [
  "plain",
  "html",
  "alternative",
  "attachmentBase64",
  "attachmentQp",
  "nested",
  "inlineImage",
  "encodedHeaders",
  "utf8Body",
  "latin1Body",
  "charsets",
  "longLines",
  "emptyBody",
  "missingHeaders",
  "addresses",
  "quotedSubject",
  "reply",
  "large",
] as const;

export type FixtureName = (typeof FIXTURE_NAMES)[number];

/**
 * The fixture called `name`, addressed to `hello@<domain>` (`domain` is the
 * recipient's mail domain, such as `alice.mail.test`).
 */
export const message = (name: FixtureName, domain: string): Fixture => {
  const to = `hello@${domain}`;
  const messageId = `<${name}@fixtures.example>`;
  const head = (subject: string, date: string, extra: string[] = []) => [
    "From: Carol Sender <carol@remote.example>",
    `To: ${to}`,
    `Subject: ${subject}`,
    `Date: ${date}`,
    `Message-ID: ${messageId}`,
    "MIME-Version: 1.0",
    ...extra,
  ];

  switch (name) {
    case "plain":
      return {
        messageId,
        data: lines(
          "From: Carol Sender <carol@remote.example>",
          `To: Hello There <${to}>`,
          "Cc: Dave <dave@remote.example>",
          "Reply-To: Replies <replies@remote.example>",
          "Subject: Plain text message",
          "Date: Mon, 05 Jan 2026 09:15:30 +0000",
          `Message-ID: ${messageId}`,
          "MIME-Version: 1.0",
          "Content-Type: text/plain; charset=us-ascii",
          "",
          "Hello,",
          "",
          "This is a plain text message.",
          "-- ",
          "Carol",
          ""
        ),
      };

    case "html":
      return {
        messageId,
        data: lines(
          ...head("HTML only message", "Tue, 06 Jan 2026 10:00:00 +0100", ["Content-Type: text/html; charset=utf-8"]),
          "",
          "<html><body><h1>Title</h1><p>An <b>HTML</b> only message.</p></body></html>",
          ""
        ),
      };

    case "alternative":
      return {
        messageId,
        data: lines(
          ...head("Alternative message", "Wed, 07 Jan 2026 11:30:00 -0500", [
            'Content-Type: multipart/alternative; boundary="alt-fixture"',
          ]),
          "",
          "--alt-fixture",
          "Content-Type: text/plain; charset=utf-8",
          "",
          "The plain alternative.",
          "--alt-fixture",
          "Content-Type: text/html; charset=utf-8",
          "",
          "<p>The <i>HTML</i> alternative.</p>",
          "--alt-fixture--",
          ""
        ),
      };

    case "attachmentBase64":
      return {
        messageId,
        data: lines(
          ...head("Attachment in base64", "Thu, 08 Jan 2026 12:00:00 +0000", [
            'Content-Type: multipart/mixed; boundary="mixed-fixture"',
          ]),
          "",
          "--mixed-fixture",
          "Content-Type: text/plain; charset=utf-8",
          "",
          "See the attached report.",
          "--mixed-fixture",
          'Content-Type: application/pdf; name="report.pdf"',
          "Content-Transfer-Encoding: base64",
          'Content-Disposition: attachment; filename="report.pdf"',
          "",
          b64Lines(BINARY),
          "--mixed-fixture--",
          ""
        ),
      };

    case "attachmentQp":
      return {
        messageId,
        data: lines(
          ...head("Attachments in quoted-printable and 7bit", "Fri, 09 Jan 2026 13:00:00 +0000", [
            'Content-Type: multipart/mixed; boundary="qp-fixture"',
          ]),
          "",
          "--qp-fixture",
          "Content-Type: text/plain; charset=utf-8",
          "Content-Transfer-Encoding: quoted-printable",
          "",
          "Two attachments, caf=C3=A9 and a soft line =",
          "break.",
          "--qp-fixture",
          'Content-Type: text/csv; charset=utf-8; name="people.csv"',
          "Content-Transfer-Encoding: quoted-printable",
          'Content-Disposition: attachment; filename="people.csv"',
          "",
          "name;city",
          "Ren=C3=A9e;Z=C3=BCrich",
          `long;${"x".repeat(60)}=`,
          "x".repeat(30),
          "--qp-fixture",
          'Content-Type: text/plain; name="notes.txt"',
          "Content-Transfer-Encoding: 7bit",
          'Content-Disposition: attachment; filename="notes.txt"',
          "",
          "Plain notes.",
          "Second line.",
          "--qp-fixture--",
          ""
        ),
      };

    case "nested":
      return {
        messageId,
        data: lines(
          ...head("Nested multiparts", "Sat, 10 Jan 2026 14:00:00 +0000", [
            'Content-Type: multipart/mixed; boundary="outer-fixture"',
          ]),
          "",
          "--outer-fixture",
          'Content-Type: multipart/alternative; boundary="inner-alt"',
          "",
          "--inner-alt",
          "Content-Type: text/plain; charset=utf-8",
          "",
          "Nested plain part.",
          "--inner-alt",
          'Content-Type: multipart/related; boundary="inner-rel"',
          "",
          "--inner-rel",
          "Content-Type: text/html; charset=utf-8",
          "",
          '<p>Nested HTML part with <img src="cid:logo@fixtures.example"></p>',
          "--inner-rel",
          'Content-Type: image/png; name="logo.png"',
          "Content-Transfer-Encoding: base64",
          "Content-ID: <logo@fixtures.example>",
          'Content-Disposition: inline; filename="logo.png"',
          "",
          b64Lines(PNG),
          "--inner-rel--",
          "--inner-alt--",
          "--outer-fixture",
          'Content-Type: application/octet-stream; name="data.bin"',
          "Content-Transfer-Encoding: base64",
          'Content-Disposition: attachment; filename="data.bin"',
          "",
          b64Lines(BINARY),
          "--outer-fixture--",
          ""
        ),
      };

    case "inlineImage":
      return {
        messageId,
        data: lines(
          ...head("Inline image", "Sun, 11 Jan 2026 15:00:00 +0000", [
            'Content-Type: multipart/related; boundary="rel-fixture"',
          ]),
          "",
          "--rel-fixture",
          "Content-Type: text/html; charset=utf-8",
          "",
          '<p>Picture: <img src="cid:pic@fixtures.example"></p>',
          "--rel-fixture",
          "Content-Type: image/png",
          "Content-Transfer-Encoding: base64",
          "Content-ID: <pic@fixtures.example>",
          "Content-Disposition: inline",
          "",
          b64Lines(PNG),
          "--rel-fixture--",
          ""
        ),
      };

    case "encodedHeaders":
      return {
        messageId,
        data: lines(
          "From: =?ISO-8859-1?Q?J=F6rg_M=FCller?= <joerg@remote.example>",
          `To: =?UTF-8?B?${Buffer.from("Zoë 日本", "utf8").toString("base64")}?= <${to}>`,
          `Subject: =?UTF-8?B?${Buffer.from("Grüße aus Köln ✉", "utf8").toString("base64")}?=`,
          " =?UTF-8?Q?_and_more?=",
          "Date: Mon, 12 Jan 2026 16:00:00 +0000",
          `Message-ID: ${messageId}`,
          "MIME-Version: 1.0",
          "Content-Type: text/plain; charset=iso-8859-1",
          "Content-Transfer-Encoding: quoted-printable",
          "",
          "Gr=FC=DFe, J=F6rg",
          ""
        ),
      };

    case "utf8Body":
      return {
        messageId,
        data: lines(
          ...head("UTF-8 body sent 8bit", "Tue, 13 Jan 2026 17:00:00 +0000", [
            "Content-Type: text/plain; charset=utf-8",
            "Content-Transfer-Encoding: 8bit",
          ]),
          "",
          u("Emoji 😀, CJK 日本語, accents éàü and a euro €."),
          ""
        ),
      };

    case "latin1Body":
      return {
        messageId,
        data: lines(
          ...head("Latin-1 body sent 8bit", "Wed, 14 Jan 2026 18:00:00 +0000", [
            "Content-Type: text/plain; charset=iso-8859-1",
            "Content-Transfer-Encoding: 8bit",
          ]),
          "",
          // Raw ISO-8859-1 octets: é is 0xE9, ü 0xFC, ß 0xDF.
          "Caf\xe9 M\xfcnchen Stra\xdfe",
          ""
        ),
      };

    case "charsets":
      return {
        messageId,
        data: lines(
          ...head("Parts in several charsets", "Thu, 15 Jan 2026 19:00:00 +0000", [
            'Content-Type: multipart/alternative; boundary="cs-fixture"',
          ]),
          "",
          "--cs-fixture",
          "Content-Type: text/plain; charset=windows-1252",
          "Content-Transfer-Encoding: base64",
          "",
          // "Price: 5 € – “quoted”" in windows-1252.
          Buffer.from([0x50, 0x72, 0x69, 0x63, 0x65, 0x3a, 0x20, 0x35, 0x20, 0x80, 0x20, 0x96, 0x20, 0x93, 0x71, 0x75, 0x6f, 0x74, 0x65, 0x64, 0x94]).toString("base64"),
          "--cs-fixture",
          "Content-Type: text/html; charset=koi8-r",
          "Content-Transfer-Encoding: base64",
          "",
          // "<p>Привет</p>" in KOI8-R.
          Buffer.from([0x3c, 0x70, 0x3e, 0xf0, 0xd2, 0xc9, 0xd7, 0xc5, 0xd4, 0x3c, 0x2f, 0x70, 0x3e]).toString("base64"),
          "--cs-fixture--",
          ""
        ),
      };

    case "longLines":
      return {
        messageId,
        data: lines(
          ...head("Very long lines", "Fri, 16 Jan 2026 20:00:00 +0000", ["Content-Type: text/plain; charset=utf-8"]),
          "",
          "short line",
          "L".repeat(900),
          "tail line",
          ""
        ),
      };

    case "emptyBody":
      return {
        messageId,
        data: lines(...head("Empty body", "Sat, 17 Jan 2026 21:00:00 +0000", ["Content-Type: text/plain"]), "", ""),
      };

    case "missingHeaders":
      // No From display name, no To, no Subject, no Date, no MIME-Version.
      return {
        messageId,
        data: lines("From: bare@remote.example", `Message-ID: ${messageId}`, "", "A body with almost no headers.", ""),
      };

    case "addresses":
      return {
        messageId,
        data: lines(
          'From: "Sender, The" <the.sender@remote.example>',
          `To: ${to}, "Quoted \\"Name\\"" <second@remote.example>, third@remote.example`,
          "Cc: Team: one@remote.example, Two <two@remote.example>;, solo@remote.example",
          "Bcc: Hidden <hidden@remote.example>",
          "Reply-To: reply-one@remote.example, Reply Two <reply-two@remote.example>",
          "Sender: Actual Sender <actual@remote.example>",
          "Subject: Many addresses",
          "Date: Sun, 18 Jan 2026 22:00:00 +0000",
          `Message-ID: ${messageId}`,
          "MIME-Version: 1.0",
          "Content-Type: text/plain; charset=utf-8",
          "",
          "Address forms.",
          ""
        ),
      };

    case "quotedSubject":
      return {
        messageId,
        data: lines(
          ...head('Re: "quoted" \\ backslash (parens) {5}', "Mon, 19 Jan 2026 23:00:00 +0000", [
            "Content-Type: text/plain; charset=utf-8",
          ]),
          "",
          'Body with "quotes", a \\ backslash and a literal-looking {10} marker.',
          ""
        ),
      };

    case "reply":
      return {
        messageId,
        data: lines(
          ...head("Re: Plain text message", "Tue, 20 Jan 2026 08:00:00 +0000", [
            "In-Reply-To: <plain@fixtures.example>",
            "References: <root@fixtures.example> <plain@fixtures.example>",
            "X-Custom-Header: custom value",
            "Content-Type: text/plain; charset=utf-8",
          ]),
          "",
          "A reply.",
          ""
        ),
      };

    case "large":
      return {
        messageId,
        data: lines(
          ...head("Large message", "Wed, 21 Jan 2026 09:00:00 +0000", [
            'Content-Type: multipart/mixed; boundary="large-fixture"',
          ]),
          "",
          "--large-fixture",
          "Content-Type: text/plain; charset=utf-8",
          "",
          LARGE_TEXT,
          "--large-fixture",
          'Content-Type: application/zip; name="archive.zip"',
          "Content-Transfer-Encoding: base64",
          'Content-Disposition: attachment; filename="archive.zip"',
          "",
          b64Lines(LARGE_ATTACHMENT),
          "--large-fixture--",
          ""
        ),
      };
  }
};
