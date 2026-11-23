/**
 * What IMAP serves for each fixture of `messages.ts`, and the layout the
 * server writes a stored mail in.
 *
 * The server does not keep the message it received: it stores the parsed
 * parts (headers, a text body, an HTML body, attachments) and writes a fresh
 * message from them on every FETCH. `wireMessage` is that layout, spelled out
 * once so each test can state a message as its parts; `fetch-body.e2e.ts`
 * also pins it against fully written-out literals.
 *
 * Not a test file.
 */
import { BINARY, LARGE_ATTACHMENT, LARGE_TEXT, PNG, type FixtureName } from "./messages";

/** UTF-8 octets of `text` as a latin1 string (one char per octet). */
const octets = (text: string): string => Buffer.from(text, "utf8").toString("latin1");

const base64 = (data: string | Buffer): string =>
  (typeof data === "string" ? Buffer.from(data, "utf8") : data).toString("base64");

export interface ServedAttachment {
  contentType: string;
  filename: string;
  bytes: Buffer;
}

/** A stored mail as the parts the server writes a message from. */
export interface StoredParts {
  mailId: string;
  /** The header lines before `MIME-Version`, in the order served. */
  headers: string[];
  text?: string;
  html?: string;
  attachments?: ServedAttachment[];
}

const textPart = (boundary: string, subtype: "plain" | "html", content: string): string =>
  `--${boundary}\r\n` +
  `Content-Type: text/${subtype}; charset=utf-8\r\n` +
  "Content-Transfer-Encoding: base64\r\n\r\n" +
  `${base64(content)}\r\n`;

/**
 * The header block of `BODY[]` for a stored mail, with its blank line. (Not
 * what `BODY[HEADER]` returns: see `wireHeaderSection`.)
 */
export const wireTopHeader = (parts: StoredParts): string => {
  const hasText = !!parts.text;
  const hasHtml = !!parts.html;
  const boundary = `boundary_${parts.mailId}`;
  let contentType: string[];
  if (parts.attachments?.length) contentType = [`Content-Type: multipart/mixed; boundary="${boundary}"`];
  else if (hasText && hasHtml) contentType = [`Content-Type: multipart/alternative; boundary="${boundary}"`];
  else if (hasHtml) contentType = ["Content-Type: text/html; charset=utf-8", "Content-Transfer-Encoding: base64"];
  else contentType = ["Content-Type: text/plain; charset=utf-8", "Content-Transfer-Encoding: base64"];
  return octets([...parts.headers, "MIME-Version: 1.0", ...contentType].join("\r\n")) + "\r\n\r\n";
};

/**
 * What `BODY[HEADER]` and `RFC822.HEADER` return: the same header lines, but
 * always declared `text/plain`, whatever the message's structure.
 */
export const wireHeaderSection = (parts: StoredParts): string =>
  octets(
    [
      ...parts.headers,
      "MIME-Version: 1.0",
      "Content-Type: text/plain; charset=utf-8",
      "Content-Transfer-Encoding: base64",
    ].join("\r\n")
  ) + "\r\n\r\n";

/** Everything of `BODY[]` after the header block, without the final CRLF: `BODY[TEXT]` minus its trailer. */
export const wireBody = (parts: StoredParts): string => {
  const { text, html, attachments = [], mailId } = parts;
  const boundary = `boundary_${mailId}`;
  const alt = `alt_${mailId}`;
  if (!attachments.length) {
    if (text && html) return textPart(boundary, "plain", text) + textPart(boundary, "html", html) + `--${boundary}--`;
    if (text) return `${base64(text)}\r\n`;
    if (html) return `${base64(html)}\r\n`;
    return "";
  }
  let body = "";
  if (text && html) {
    body +=
      `--${boundary}\r\nContent-Type: multipart/alternative; boundary="${alt}"\r\n\r\n` +
      textPart(alt, "plain", text) +
      textPart(alt, "html", html) +
      `--${alt}--\r\n`;
  } else if (text) body += textPart(boundary, "plain", text);
  else if (html) body += textPart(boundary, "html", html);
  for (const attachment of attachments) {
    body +=
      `--${boundary}\r\n` +
      `Content-Type: ${attachment.contentType}\r\n` +
      "Content-Transfer-Encoding: base64\r\n" +
      `Content-Disposition: attachment; filename="${attachment.filename}"\r\n\r\n` +
      `${base64(attachment.bytes)}\r\n`;
  }
  return `${body}--${boundary}--`;
};

/**
 * The octets of the `BODY[]` literal for a stored mail: header block, body,
 * and the CRLF the server appends to the literal (which RFC822.SIZE counts).
 */
export const wireMessage = (parts: StoredParts): string => `${wireTopHeader(parts)}${wireBody(parts)}\r\n`;

/** One text part of a BODYSTRUCTURE, with (`extension`) or without the extension fields. */
const structureText = (subtype: "PLAIN" | "HTML", size: number, extension: boolean): string =>
  `("TEXT" "${subtype}" ("CHARSET" "UTF-8") NIL NIL "BASE64" ${size} ${size === 0 ? 0 : 1}${
    extension ? " NIL NIL NIL NIL" : ""
  })`;

/** The multipart/alternative of a text and an HTML part of the given encoded sizes. */
export const structureAlternative = (textSize: number, htmlSize: number, extension = true): string =>
  `(${structureText("PLAIN", textSize, extension)}${structureText("HTML", htmlSize, extension)} "alternative"${
    extension ? " NIL NIL NIL NIL" : ""
  })`;

export const structureSingle = (subtype: "PLAIN" | "HTML", size: number, extension = true): string =>
  structureText(subtype, size, extension);

/** An attachment part: `lines` is given for a `text/*` attachment only. */
export const structureAttachment = (
  type: string,
  subtype: string,
  filename: string,
  size: number,
  extension = true
): string =>
  `("${type}" "${subtype}" ("NAME" "${filename}") NIL NIL "BASE64" ${size}${type === "text" ? " 1" : ""}${
    extension ? ` NIL ("ATTACHMENT" ("FILENAME" "${filename}")) NIL NIL` : ""
  })`;

export const structureMixed = (parts: string[], extension = true): string =>
  `(${parts.join("")} "mixed"${extension ? " NIL NIL NIL NIL" : ""})`;

export interface Served {
  /** INTERNALDATE as served by a server running in UTC; undefined when it is the arrival time. */
  internalDate?: string;
  /** RFC822.SIZE, for a seven-letter account name. */
  size: number;
  /** ENVELOPE, parenthesized; `DATE` stands for the date string when it is the arrival time. */
  envelope: string;
  /** BODYSTRUCTURE, parenthesized. */
  bodyStructure: string;
  /** BODY (the structure without extension data), parenthesized. */
  body: string;
  /** Header lines served before MIME-Version; `DATE` as in `envelope`. */
  headers: string[];
  text: string;
  html: string;
  attachments: ServedAttachment[];
}

const CAROL = '(("Carol Sender" NIL "carol" "remote.example"))';
const CAROL_HEADER = 'From: "Carol Sender" <carol@remote.example>';
const PNG_DATA_URI = `data:image/png;base64,${PNG.toString("base64")}`;

/**
 * What the server serves for the fixture `name` delivered to
 * `hello@<domain>`. The sizes hold for a 17-octet `domain`, such as
 * `fetcher.mail.test`.
 */
export const served = (name: FixtureName, domain: string): Served => {
  const to = `((NIL NIL "hello" "${domain}"))`;
  const toHeader = `To: hello@${domain}`;
  const id = `<${name}@fixtures.example>`;
  const usual = (date: string, subject: string) => ({
    envelope: `("${date}" "${subject}" ${CAROL} ${CAROL} NIL ${to} NIL NIL NIL "${id}")`,
    headers: [`Message-ID: ${id}`, `Date: ${date}`, CAROL_HEADER, toHeader, `Subject: ${subject}`],
  });
  const both = (textSize: number, htmlSize: number) => ({
    bodyStructure: structureAlternative(textSize, htmlSize),
    body: structureAlternative(textSize, htmlSize, false),
  });
  const mixed = (textSize: number, htmlSize: number, attachments: [string, string, string, number][]) => ({
    bodyStructure: structureMixed([
      structureAlternative(textSize, htmlSize),
      ...attachments.map(([type, subtype, filename, size]) => structureAttachment(type, subtype, filename, size)),
    ]),
    body: structureMixed(
      [
        structureAlternative(textSize, htmlSize, false),
        ...attachments.map(([type, subtype, filename, size]) =>
          structureAttachment(type, subtype, filename, size, false)
        ),
      ],
      false
    ),
  });

  switch (name) {
    case "plain": {
      const text = "Hello,\n\nThis is a plain text message.\n-- \nCarol\n";
      const date = "Mon, 05 Jan 2026 09:15:30 GMT";
      return {
        internalDate: " 5-Jan-2026 09:15:30 +0000",
        size: 826,
        envelope:
          `("${date}" "Plain text message" ${CAROL} ${CAROL} (("Replies" NIL "replies" "remote.example")) ` +
          `(("Hello There" NIL "hello" "${domain}")) (("Dave" NIL "dave" "remote.example")) NIL NIL "${id}")`,
        ...both(64, 64),
        headers: [
          `Message-ID: ${id}`,
          `Date: ${date}`,
          CAROL_HEADER,
          `To: "Hello There" <hello@${domain}>`,
          'Cc: "Dave" <dave@remote.example>',
          'Reply-To: "Replies" <replies@remote.example>',
          "Subject: Plain text message",
        ],
        // A message with no HTML part is stored with its text as the HTML body too.
        text,
        html: text,
        attachments: [],
      };
    }
    case "html":
      return {
        internalDate: " 6-Jan-2026 09:00:00 +0000",
        size: 744,
        ...usual("Tue, 06 Jan 2026 09:00:00 GMT", "HTML only message"),
        ...both(40, 104),
        // A message with no text part gets one derived from its HTML.
        text: "TITLE\n\nAn HTML only message.",
        html: "<html><body><h1>Title</h1><p>An <b>HTML</b> only message.</p></body></html>\n",
        attachments: [],
      };
    case "alternative":
      return {
        internalDate: " 7-Jan-2026 16:30:00 +0000",
        size: 689,
        ...usual("Wed, 07 Jan 2026 16:30:00 GMT", "Alternative message"),
        ...both(32, 48),
        text: "The plain alternative.",
        html: "<p>The <i>HTML</i> alternative.</p>",
        attachments: [],
      };
    case "attachmentBase64":
      return {
        internalDate: " 8-Jan-2026 12:00:00 +0000",
        size: 1369,
        ...usual("Thu, 08 Jan 2026 12:00:00 GMT", "Attachment in base64"),
        ...mixed(32, 32, [["application", "pdf", "report.pdf", 344]]),
        text: "See the attached report.",
        html: "See the attached report.",
        attachments: [{ contentType: "application/pdf", filename: "report.pdf", bytes: BINARY }],
      };
    case "attachmentQp":
      return {
        internalDate: " 9-Jan-2026 13:00:00 +0000",
        size: 1459,
        ...usual("Fri, 09 Jan 2026 13:00:00 GMT", "Attachments in quoted-printable and 7bit"),
        ...mixed(60, 60, [
          ["text", "csv", "people.csv", 164],
          ["text", "plain", "notes.txt", 36],
        ]),
        text: "Two attachments, café and a soft line break.",
        html: "Two attachments, café and a soft line break.",
        attachments: [
          {
            contentType: "text/csv",
            filename: "people.csv",
            bytes: Buffer.from(`name;city\r\nRenée;Zürich\r\nlong;${"x".repeat(90)}`, "utf8"),
          },
          { contentType: "text/plain", filename: "notes.txt", bytes: Buffer.from("Plain notes.\r\nSecond line.") },
        ],
      };
    case "nested":
      return {
        internalDate: "10-Jan-2026 14:00:00 +0000",
        size: 1798,
        ...usual("Sat, 10 Jan 2026 14:00:00 GMT", "Nested multiparts"),
        ...mixed(24, 212, [
          ["image", "png", "logo.png", 96],
          ["application", "octet-stream", "data.bin", 344],
        ]),
        text: "Nested plain part.",
        // The inline image is written into the HTML as a data: URI, and kept as an attachment too.
        html: `<p>Nested HTML part with <img src="${PNG_DATA_URI}"></p>`,
        attachments: [
          { contentType: "image/png", filename: "logo.png", bytes: PNG },
          { contentType: "application/octet-stream", filename: "data.bin", bytes: BINARY },
        ],
      };
    case "inlineImage":
      return {
        internalDate: "11-Jan-2026 15:00:00 +0000",
        size: 1246,
        ...usual("Sun, 11 Jan 2026 15:00:00 GMT", "Inline image"),
        ...mixed(12, 196, [["image", "png", "attachment", 96]]),
        text: "Picture:",
        html: `<p>Picture: <img src="${PNG_DATA_URI}"></p>`,
        // A part with no filename is served as `attachment`.
        attachments: [{ contentType: "image/png", filename: "attachment", bytes: PNG }],
      };
    case "encodedHeaders": {
      const date = "Mon, 12 Jan 2026 16:00:00 GMT";
      const joerg = octets('(("Jörg Müller" NIL "joerg" "remote.example"))');
      return {
        internalDate: "12-Jan-2026 16:00:00 +0000",
        size: 680,
        // RFC 2047 words are decoded and served as raw UTF-8 inside quoted strings.
        envelope:
          `("${date}" "${octets("Grüße aus Köln ✉ and more")}" ${joerg} ${joerg} NIL ` +
          `(("${octets("Zoë 日本")}" NIL "hello" "${domain}")) NIL NIL NIL "${id}")`,
        ...both(20, 20),
        headers: [
          `Message-ID: ${id}`,
          `Date: ${date}`,
          'From: "Jörg Müller" <joerg@remote.example>',
          `To: "Zoë 日本" <hello@${domain}>`,
          "Subject: Grüße aus Köln ✉ and more",
        ],
        text: "Grüße, Jörg\n",
        html: "Grüße, Jörg\n",
        attachments: [],
      };
    }
    case "utf8Body":
      return {
        internalDate: "13-Jan-2026 17:00:00 +0000",
        size: 767,
        ...usual("Tue, 13 Jan 2026 17:00:00 GMT", "UTF-8 body sent 8bit"),
        ...both(80, 80),
        text: "Emoji 😀, CJK 日本語, accents éàü and a euro €.\n",
        html: "Emoji 😀, CJK 日本語, accents éàü and a euro €.\n",
        attachments: [],
      };
    case "latin1Body":
      return {
        internalDate: "14-Jan-2026 18:00:00 +0000",
        size: 675,
        ...usual("Wed, 14 Jan 2026 18:00:00 GMT", "Latin-1 body sent 8bit"),
        ...both(32, 32),
        text: "Café München Straße\n",
        html: "Café München Straße\n",
        attachments: [],
      };
    case "charsets":
      return {
        internalDate: "15-Jan-2026 19:00:00 +0000",
        size: 680,
        ...usual("Thu, 15 Jan 2026 19:00:00 GMT", "Parts in several charsets"),
        ...both(40, 28),
        text: "Price: 5 € – “quoted”",
        html: "<p>Привет</p>",
        attachments: [],
      };
    case "longLines": {
      const text = `short line\n${"L".repeat(900)}\ntail line\n`;
      return {
        internalDate: "16-Jan-2026 20:00:00 +0000",
        size: 3067,
        ...usual("Fri, 16 Jan 2026 20:00:00 GMT", "Very long lines"),
        ...both(1232, 1232),
        text,
        html: text,
        attachments: [],
      };
    }
    case "emptyBody":
      return {
        internalDate: "17-Jan-2026 21:00:00 +0000",
        size: 273,
        ...usual("Sat, 17 Jan 2026 21:00:00 GMT", "Empty body"),
        bodyStructure: structureSingle("PLAIN", 0),
        body: structureSingle("PLAIN", 0, false),
        text: "",
        html: "",
        attachments: [],
      };
    case "missingHeaders":
      return {
        // No Date header: the arrival time stands in, as Date and as INTERNALDATE.
        size: 623,
        envelope:
          `("DATE" NIL ((NIL NIL "bare" "remote.example")) ((NIL NIL "bare" "remote.example")) ` +
          `NIL NIL NIL NIL NIL "${id}")`,
        ...both(44, 44),
        headers: [`Message-ID: ${id}`, "Date: DATE", "From: bare@remote.example"],
        text: "A body with almost no headers.\n",
        html: "A body with almost no headers.\n",
        attachments: [],
      };
    case "addresses": {
      const date = "Sun, 18 Jan 2026 22:00:00 GMT";
      const sender = '(("Sender, The" NIL "the.sender" "remote.example"))';
      return {
        internalDate: "18-Jan-2026 22:00:00 +0000",
        size: 906,
        envelope:
          `("${date}" "Many addresses" ${sender} ${sender} ` +
          '((NIL NIL "reply-one" "remote.example") ("Reply Two" NIL "reply-two" "remote.example")) ' +
          `((NIL NIL "hello" "${domain}") ("Quoted \\"Name\\"" NIL "second" "remote.example") (NIL NIL "third" "remote.example")) ` +
          '((NIL NIL "one" "remote.example") ("Two" NIL "two" "remote.example") (NIL NIL "solo" "remote.example")) ' +
          `(("Hidden" NIL "hidden" "remote.example")) NIL "${id}")`,
        ...both(20, 20),
        headers: [
          `Message-ID: ${id}`,
          `Date: ${date}`,
          'From: "Sender, The" <the.sender@remote.example>',
          // The quotes inside the display name are served unescaped.
          `To: hello@${domain}, "Quoted "Name"" <second@remote.example>, third@remote.example`,
          'Cc: "Team": one@remote.example, "Two" <two@remote.example>;, solo@remote.example',
          'Bcc: "Hidden" <hidden@remote.example>',
          'Reply-To: reply-one@remote.example, "Reply Two" <reply-two@remote.example>',
          "Subject: Many addresses",
        ],
        text: "Address forms.\n",
        html: "Address forms.\n",
        attachments: [],
      };
    }
    case "quotedSubject": {
      const date = "Mon, 19 Jan 2026 23:00:00 GMT";
      const text = 'Body with "quotes", a \\ backslash and a literal-looking {10} marker.\n';
      return {
        internalDate: "19-Jan-2026 23:00:00 +0000",
        size: 813,
        envelope: `("${date}" "Re: \\"quoted\\" \\\\ backslash (parens) {5}" ${CAROL} ${CAROL} NIL ${to} NIL NIL NIL "${id}")`,
        ...both(92, 92),
        headers: [
          `Message-ID: ${id}`,
          `Date: ${date}`,
          CAROL_HEADER,
          toHeader,
          'Subject: Re: "quoted" \\ backslash (parens) {5}',
        ],
        text,
        html: text,
        attachments: [],
      };
    }
    case "reply":
      return {
        internalDate: "20-Jan-2026 08:00:00 +0000",
        size: 630,
        // In-Reply-To is never served in the envelope; References and X- headers are not served at all.
        ...usual("Tue, 20 Jan 2026 08:00:00 GMT", "Re: Plain text message"),
        ...both(12, 12),
        text: "A reply.\n",
        html: "A reply.\n",
        attachments: [],
      };
    case "large": {
      const text = LARGE_TEXT.replace(/\r\n/g, "\n");
      return {
        internalDate: "21-Jan-2026 09:00:00 +0000",
        size: 2293992,
        ...usual("Wed, 21 Jan 2026 09:00:00 GMT", "Large message"),
        ...mixed(146524, 146524, [["application", "zip", "archive.zip", 2000000]]),
        text,
        html: text,
        attachments: [{ contentType: "application/zip", filename: "archive.zip", bytes: LARGE_ATTACHMENT }],
      };
    }
  }
};

/** The parts `wireMessage` needs for a served fixture stored as `mailId`. */
export const storedParts = (fixture: Served, mailId: string, date?: string): StoredParts => ({
  mailId,
  headers: date === undefined ? fixture.headers : fixture.headers.map((line) => line.replace("DATE", date)),
  text: fixture.text,
  html: fixture.html,
  attachments: fixture.attachments,
});
