import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import type { TestServer } from "../harness";
import { FIXED_DATE, Mailer, attachmentBytes, boot, crlf, octets, type Row } from "./support";

// What the text and html columns hold for each shape of message body.
let server: TestServer;
let mailer: Mailer;

beforeAll(async () => {
  server = await boot();
  mailer = new Mailer(server);
}, 180_000);

afterAll(async () => {
  mailer?.close();
  await server?.stop();
});

const HEAD = ["From: Remote Sender <sender@remote.example>", "To: hello@mail.test", "Subject: Body", `Date: ${FIXED_DATE}`];

const body = (mail: Row) => ({ text: mail.text as string, html: mail.html as string });
const files = (mail: Row) =>
  (mail.attachments as { filename: string; contentType: string; size: number }[]).map(
    ({ filename, contentType, size }) => ({ filename, contentType, size })
  );
const fileText = (mail: Row, index = 0) => attachmentBytes(server, mail.attachments[index].content.data).toString("latin1");

/** A single-part message: the given content headers, then the body lines. */
const single = (contentHeaders: string[], ...lines: string[]) => crlf(...HEAD, ...contentHeaders, "", ...lines);
const singleBytes = (contentHeaders: string[], ...content: (string | Uint8Array)[]) =>
  octets(crlf(...HEAD, ...contentHeaders, ""), ...content);
const multipart = (type: string, boundary: string, ...lines: string[]) =>
  crlf(...HEAD, `Content-Type: multipart/${type}; boundary="${boundary}"`, "", ...lines);

describe("a single text part", () => {
  test("text/plain is stored as written in both columns, line endings as LF", async () => {
    const mail = await mailer.store(
      single(
        ["Content-Type: text/plain; charset=utf-8"],
        "Hello,",
        "",
        "second paragraph <b>not bold</b> & https://example.com/x?a=1&b=2",
        "  indented\ttabbed  ",
        "bye"
      )
    );
    const text = "Hello,\n\nsecond paragraph <b>not bold</b> & https://example.com/x?a=1&b=2\n  indented\ttabbed  \nbye\n";
    expect(body(mail)).toEqual({ text, html: text });
    expect(mail.attachments).toEqual([]);
  });

  test("no Content-Type at all is text/plain", async () => {
    const mail = await mailer.store(single([], "Just text"));
    expect(body(mail)).toEqual({ text: "Just text\n", html: "Just text\n" });
  });

  test("the type is matched in any letter case, with any parameter spelling", async () => {
    for (const header of ["Content-Type: TEXT/PLAIN; CHARSET=UTF-8", 'content-type:text/plain;charset="utf-8"', "Content-Type: Text/Plain ; Charset = utf-8"]) {
      const mail = await mailer.store(singleBytes([header], Buffer.from("naïve\r\n")));
      expect({ header, ...body(mail) }).toEqual({ header, text: "naïve\n", html: "naïve\n" });
    }
  });

  test("text/html keeps the markup and gets a plain-text rendering with link targets", async () => {
    const html =
      '<html><head><title>T</title><style>p{color:red}</style></head><body><h1>Title</h1><p>Hello <b>world</b> &amp; <a href="https://example.com/x">a link</a></p>' +
      '<ul><li>one</li><li>two</li></ul><img src="https://example.com/i.png" alt="pic"><script>alert(1)</script></body></html>';
    const mail = await mailer.store(single(["Content-Type: text/html; charset=utf-8"], html));
    expect(body(mail)).toEqual({
      text: "TITLE\n\nHello world & a link [https://example.com/x]\n\n * one\n * two\n\npic [https://example.com/i.png]",
      html: `${html}\n`,
    });
    // The rendering counts as a text part, so the HTML-only spam rule stays quiet.
    expect(mail.spam_reasons).toEqual([]);
  });

  test("an empty body", async () => {
    expect(body(await mailer.store(single(["Content-Type: text/plain"])))).toEqual({ text: "", html: "" });
    expect(body(await mailer.store(single(["Content-Type: text/html"])))).toEqual({ text: "", html: "" });
  });

  test("a body of blank lines only is kept", async () => {
    expect(body(await mailer.store(single(["Content-Type: text/plain"], "", "", "")))).toEqual({ text: "\n\n\n", html: "\n\n\n" });
  });
});

describe("the text derived from HTML", () => {
  const html = [
    "<!DOCTYPE html>",
    '<html lang="en"><head><meta charset="utf-8"><title>Ignored title</title><style>body { color: red; }</style></head>',
    "<body>",
    "<!-- a comment -->",
    "<h1>Main heading</h1>",
    "<h2>Sub heading</h2>",
    "<p>First paragraph with <b>bold</b>, <i>italic</i>, <u>underline</u> and a<br>line break.</p>",
    '<p>A <a href="https://example.com/path?a=1&amp;b=2">named link</a>, a bare one <a href="https://example.org/">https://example.org/</a> and <a href="mailto:someone@remote.example">mail</a>.</p>',
    "<p>Entities: &amp; &lt; &gt; &quot; &nbsp;| &copy; &euro; &#8364; &#x1F600; &unknown;</p>",
    "<ul><li>First item</li><li>Second item<ul><li>Nested item</li></ul></li></ul>",
    "<ol><li>One</li><li>Two</li></ol>",
    "<blockquote>A quoted paragraph.<br>Second quoted line.</blockquote>",
    "<pre>  preformatted\n    text  </pre>",
    "<table><tr><th>Name</th><th>Qty</th></tr><tr><td>Apples</td><td>3</td></tr><tr><td>Pears</td><td>12</td></tr></table>",
    "<hr>",
    '<p><img src="https://example.com/logo.png" alt="Company logo"> <img src="https://example.com/spacer.gif"></p>',
    "<div>Division one</div><div>Division two</div>",
    `<p>${"A long line of words that should run past the eighty column limit to show how wrapping works here. ".repeat(2).trim()}</p>`,
    '<script>alert("ignored")</script>',
    '<p style="display:none">Hidden by style</p>',
    "<p>Last   paragraph   with    spaces\n and a newline.</p>",
    "</body></html>",
  ].join("\r\n");

  test("a message that is one HTML part: headings, links with their targets, lists, quotes, wrapping at 80 columns", async () => {
    const mail = await mailer.store(single(["Content-Type: text/html; charset=utf-8"], html));
    expect(mail.html).toBe(`${html.replace(/\r\n/g, "\n")}\n`);
    // Not rendered: table cells run together, and an element hidden by its style is still text.
    expect(mail.text).toBe(
      [
        "MAIN HEADING",
        "",
        "",
        "SUB HEADING",
        "",
        "First paragraph with bold, italic, underline and a",
        "line break.",
        "",
        "A named link [https://example.com/path?a=1&b=2], a bare one https://example.org/",
        "[https://example.org/] and mail [someone@remote.example].",
        "",
        // The non-breaking space stays one (U+00A0).
        'Entities: & < > "  | © € € 😀 &unknown;',
        "",
        " * First item",
        " * Second item",
        "   * Nested item",
        "",
        " 1. One",
        " 2. Two",
        "",
        "> A quoted paragraph.",
        "> Second quoted line.",
        "",
        "  preformatted",
        "    text  ",
        "",
        "NameQtyApples3Pears12",
        "",
        "-".repeat(80),
        "",
        "Company logo [https://example.com/logo.png] [https://example.com/spacer.gif]",
        "",
        "Division one",
        "Division two",
        "",
        "A long line of words that should run past the eighty column limit to show how",
        "wrapping works here. A long line of words that should run past the eighty column",
        "limit to show how wrapping works here.",
        "",
        "Hidden by style",
        "",
        "Last paragraph with spaces and a newline.",
      ].join("\n")
    );
  });

  test("an HTML part inside a multipart with no text part: a terser rendering, with URLs masked", async () => {
    // The parser leaves the text empty here; it is filled in when the row is
    // written, by a different routine: no link targets, blank lines squeezed
    // out, runs of spaces collapsed, and anything that looks like a URL
    // replaced by "[url]".
    const mail = await mailer.store(multipart("alternative", "alt", "--alt", "Content-Type: text/html; charset=utf-8", "", html, "--alt--"));
    expect(mail.html).toBe(html.replace(/\r\n/g, "\n"));
    expect(mail.text).toBe(
      [
        "MAIN HEADING",
        "SUB HEADING",
        "First paragraph with bold, italic, underline and a",
        "line break.",
        // NOTE: the mask stops before the URL's last character, leaving a stray "/".
        "A named link, a bare one [url]/ and mail.",
        'Entities: & < > " | © € € 😀 &unknown;',
        " * First item",
        "* Second item",
        "* Nested item",
        " 1. One",
        "2. Two",
        "> A quoted paragraph.",
        "> Second quoted line.",
        " preformatted",
        "text",
        "NameQtyApples3Pears12",
        "-".repeat(80),
        "Division one",
        "Division two",
        "A long line of words that should run past the eighty column limit to show how",
        "wrapping works here. A long line of words that should run past the eighty column",
        "limit to show how wrapping works here.",
        "Hidden by style",
        "Last paragraph with spaces and a newline.",
      ].join("\n")
    );
  });
});

describe("transfer encodings of a text part", () => {
  test("quoted-printable: escapes, soft breaks, trailing space, and an escape that is not one", async () => {
    const mail = await mailer.store(
      single(
        ["Content-Type: text/plain; charset=iso-8859-1", "Content-Transfer-Encoding: quoted-printable"],
        "Caf=E9 cr=E8me br=FBl=E9e =3D soft=",
        "break trailing   ",
        "lower=e9 bad =ZZ escape =",
        ""
      )
    );
    expect(mail.text).toBe("Café crème brûlée = softbreak trailing\nloweré bad =ZZ escape \n");
  });

  test("base64, with line breaks and stray characters in the encoding", async () => {
    const encoded = Buffer.from("Grüße aus Köln ✓\r\nzweite Zeile\r\n").toString("base64");
    const wrapped = encoded.replace(/(.{20})/g, "$1\r\n");
    const mail = await mailer.store(single(["Content-Type: text/plain; charset=utf-8", "Content-Transfer-Encoding: base64"], wrapped));
    expect(mail.text).toBe("Grüße aus Köln ✓\nzweite Zeile\n");

    const noisy = await mailer.store(
      single(["Content-Type: text/plain", "Content-Transfer-Encoding: BASE64"], "aGVs bG8g\t!!", "d29y*bGQ=")
    );
    expect(noisy.text).toBe("hello world");
  });

  test("two base64 encodings back to back both decode: padding ends only its own group", async () => {
    const mail = await mailer.store(
      single(["Content-Type: text/plain", "Content-Transfer-Encoding: base64"], "YQ==", "Yg==", "Y2Q=")
    );
    expect(mail.text).toBe("abcd");
  });

  test("7bit, 8bit, binary and unknown encodings pass the octets through", async () => {
    for (const encoding of ["7bit", "8bit", "binary", "x-uuencode", "8BIT"]) {
      const mail = await mailer.store(
        singleBytes(["Content-Type: text/plain; charset=utf-8", `Content-Transfer-Encoding: ${encoding}`], Buffer.from("déjà vu =41\r\n"))
      );
      expect({ encoding, text: mail.text }).toEqual({ encoding, text: "déjà vu =41\n" });
    }
  });

  test("a comment after the encoding is ignored", async () => {
    const mail = await mailer.store(
      single(["Content-Type: text/plain", "Content-Transfer-Encoding: base64 (really)"], Buffer.from("commented").toString("base64"))
    );
    expect(mail.text).toBe("commented");
  });
});

describe("charsets of a text part", () => {
  const stored = async (charset: string, bytes: number[] | Buffer, extra: string[] = ["Content-Transfer-Encoding: 8bit"]) =>
    (await mailer.store(singleBytes([`Content-Type: text/plain; charset=${charset}`, ...extra], Buffer.from(bytes), "\r\n"))).text;

  test("single-byte charsets", async () => {
    expect(await stored("iso-8859-1", [0x63, 0x61, 0x66, 0xe9])).toBe("café\n");
    expect(await stored("ISO-8859-15", [0x31, 0x30, 0x20, 0xa4])).toBe("10 €\n");
    expect(await stored("windows-1252", [0x93, 0x71, 0x94, 0x20, 0x80, 0x20, 0x96])).toBe("“q” € –\n");
    expect(await stored('"koi8-r"', [0xf0, 0xd2, 0xc9, 0xd7, 0xc5, 0xd4])).toBe("Привет\n");
    expect(await stored("windows-1251", [0xcf, 0xf0, 0xe8, 0xe2, 0xe5, 0xf2])).toBe("Привет\n");
    expect(await stored("iso-8859-2", [0xb3, 0xf3, 0x64, 0xbc])).toBe("łódź\n");
    expect(await stored("iso-8859-7", [0xc5, 0xeb, 0xeb, 0xe7, 0xed, 0xe9, 0xea, 0xdc])).toBe("Ελληνικά\n");
    expect(await stored("windows-1255", [0xf9, 0xec, 0xe5, 0xed])).toBe("שלום\n");
  });

  test("multi-byte charsets", async () => {
    expect(await stored("Shift_JIS", [0x82, 0xb1, 0x82, 0xf1, 0x82, 0xc9, 0x82, 0xbf, 0x82, 0xcd])).toBe("こんにちは\n");
    expect(await stored("shift-jis", [0x93, 0xfa, 0x96, 0x7b])).toBe("日本\n");
    expect(await stored("EUC-JP", [0xc6, 0xfc, 0xcb, 0xdc])).toBe("日本\n");
    expect(await stored("ISO-2022-JP", [0x1b, 0x24, 0x42, 0x46, 0x7c, 0x4b, 0x5c, 0x1b, 0x28, 0x42], ["Content-Transfer-Encoding: 7bit"])).toBe("日本\n");
    expect(await stored("gb2312", [0xc4, 0xe3, 0xba, 0xc3])).toBe("你好\n");
    expect(await stored("GBK", [0xc4, 0xe3, 0xba, 0xc3])).toBe("你好\n");
    expect(await stored("gb18030", [0xc4, 0xe3, 0xba, 0xc3])).toBe("你好\n");
    expect(await stored("big5", [0xa7, 0x41, 0xa6, 0x6e])).toBe("你好\n");
    expect(await stored("euc-kr", [0xbe, 0xc8, 0xb3, 0xe7])).toBe("안녕\n");
    expect(await stored("ks_c_5601-1987", [0xbe, 0xc8, 0xb3, 0xe7])).toBe("안녕\n");
    expect(await stored("cp949", [0xbe, 0xc8, 0xb3, 0xe7])).toBe("안녕\n");
  });

  test("UTF-16 with a byte-order mark, and without one by looking at the text", async () => {
    const b64 = ["Content-Transfer-Encoding: base64"];
    const utf16 = (text: string, bigEndian = false) => {
      const little = Buffer.from(text, "utf16le");
      return bigEndian ? Buffer.from(little).swap16() : little;
    };
    const store = async (charset: string, bytes: Buffer) =>
      (await mailer.store(single([`Content-Type: text/plain; charset=${charset}`, ...b64], bytes.toString("base64")))).text;
    expect(await store("utf-16", utf16("﻿Hi ✓"))).toBe("Hi ✓");
    expect(await store("utf-16", utf16("﻿Hi ✓", true))).toBe("Hi ✓");
    expect(await store("utf-16", utf16("no mark, little"))).toBe("no mark, little");
    expect(await store("utf-16", utf16("no mark, big", true))).toBe("no mark, big");
    expect(await store("utf-16le", utf16("explicit le ✓"))).toBe("explicit le ✓");
    expect(await store("utf-16be", utf16("explicit be ✓", true))).toBe("explicit be ✓");
  });

  test("us-ascii and a missing charset are read as UTF-8", async () => {
    expect(await stored("us-ascii", Buffer.from("naïve ✓"), [])).toBe("naïve ✓\n");
    const none = await mailer.store(singleBytes(["Content-Type: text/plain"], Buffer.from("naïve ✓\r\n")));
    expect(none.text).toBe("naïve ✓\n");
  });

  test("an unknown charset is read as UTF-8", async () => {
    expect(await stored("x-made-up", Buffer.from("naïve"))).toBe("naïve\n");
  });

  test("octets that are not valid in the charset become U+FFFD", async () => {
    const mail = await mailer.store(singleBytes(["Content-Type: text/plain; charset=utf-8"], "bad \xff\xfe bytes \xc3( cut \xe2\x9c end\r\n"));
    expect(mail.text).toBe("bad �� bytes �( cut � end\n");
    const latin = await mailer.store(singleBytes(["Content-Type: text/plain"], "caf\xe9\r\n"));
    expect(latin.text).toBe("caf�\n");
  });

  test("a UTF-8 byte-order mark at the start of the body is kept as U+FEFF", async () => {
    const mail = await mailer.store(singleBytes(["Content-Type: text/plain; charset=utf-8"], "\xef\xbb\xbfwith bom\r\n"));
    expect(mail.text).toBe("﻿with bom\n");
  });

  test("the charset applies to HTML parts the same way", async () => {
    const mail = await mailer.store(singleBytes(["Content-Type: text/html; charset=iso-8859-1"], "<p>caf\xe9</p>\r\n"));
    expect(body(mail)).toEqual({ text: "café", html: "<p>café</p>\n" });
  });
});

describe("format=flowed", () => {
  test("soft line breaks are joined and space-stuffing removed; the signature separator is kept", async () => {
    const mail = await mailer.store(
      single(
        ["Content-Type: text/plain; charset=utf-8; format=flowed"],
        "This is a long line that was wrapped ",
        "by the sender's client and continues ",
        "here.",
        "",
        " stuffed line",
        "-- ",
        "Signature"
      )
    );
    expect(mail.text).toBe(
      "This is a long line that was wrapped by the sender's client and continues here.\n\nstuffed line\n-- \nSignature"
    );
  });

  test("quote marks are not treated specially when joining", async () => {
    // NOTE: RFC 3676 joins a quoted paragraph without repeating its `>`; here
    // the marker of the continuation line stays in the middle of the text.
    const mail = await mailer.store(single(["Content-Type: text/plain; format=flowed"], "> quoted ", "> text"));
    expect(mail.text).toBe("> quoted > text");
  });

  test("delsp=yes also removes the space before the break", async () => {
    const mail = await mailer.store(single(["Content-Type: text/plain; format=flowed; delsp=yes"], "Japa ", "nese ", "style.", "next"));
    expect(mail.text).toBe("Japanesestyle.\nnext");
    const kept = await mailer.store(single(["Content-Type: text/plain; format=flowed; delsp=no"], "two ", "words"));
    expect(kept.text).toBe("two words");
  });

  test("without format=flowed a trailing space is just a trailing space", async () => {
    const mail = await mailer.store(single(["Content-Type: text/plain"], "not ", "flowed"));
    expect(mail.text).toBe("not \nflowed\n");
  });
});

describe("multipart/alternative", () => {
  test("the text part is the text and the HTML part the html", async () => {
    const mail = await mailer.store(
      multipart(
        "alternative",
        "alt",
        "This preamble is ignored.",
        "--alt",
        "Content-Type: text/plain; charset=utf-8",
        "",
        "Plain version",
        "second line",
        "--alt",
        "Content-Type: text/html; charset=utf-8",
        "",
        "<p>HTML <i>version</i></p>",
        "--alt--",
        "This epilogue is ignored."
      )
    );
    // The line break before a boundary belongs to the boundary.
    expect(body(mail)).toEqual({ text: "Plain version\nsecond line", html: "<p>HTML <i>version</i></p>" });
    expect(mail.spam_reasons).toEqual([]);
  });

  test("the HTML part first changes nothing", async () => {
    const mail = await mailer.store(
      multipart("alternative", "alt", "--alt", "Content-Type: text/html", "", "<p>HTML</p>", "--alt", "Content-Type: text/plain", "", "Plain", "--alt--")
    );
    expect(body(mail)).toEqual({ text: "Plain", html: "<p>HTML</p>" });
  });

  test("with only an HTML part the text is derived from it, and the HTML-only spam rule scores", async () => {
    const mail = await mailer.store(
      multipart("alternative", "alt", "--alt", "Content-Type: text/html; charset=utf-8", "", '<p>Only <i>HTML</i> here, <a href="https://example.com/">link</a></p>', "--alt--")
    );
    expect(body(mail)).toEqual({
      text: "Only HTML here, link",
      html: '<p>Only <i>HTML</i> here, <a href="https://example.com/">link</a></p>',
    });
    expect([mail.spam_score, mail.spam_reasons]).toEqual([5, ["HTML-only email with no text alternative"]]);
  });

  test("with only a text part both columns hold it", async () => {
    const mail = await mailer.store(multipart("alternative", "alt", "--alt", "Content-Type: text/plain", "", "Only plain", "--alt--"));
    expect(body(mail)).toEqual({ text: "Only plain", html: "Only plain" });
  });

  test("several text and several HTML alternatives are each joined", async () => {
    const mail = await mailer.store(
      multipart(
        "alternative",
        "alt",
        "--alt",
        "Content-Type: text/plain",
        "",
        "Plain A",
        "--alt",
        "Content-Type: text/plain",
        "",
        "Plain B",
        "--alt",
        "Content-Type: text/html",
        "",
        "<p>HTML A</p>",
        "--alt",
        "Content-Type: text/html",
        "",
        "<p>HTML B</p>",
        "--alt--"
      )
    );
    expect(body(mail)).toEqual({ text: "Plain A\nPlain B", html: "<p>HTML A</p><br/>\n<p>HTML B</p>" });
  });
});

describe("multipart/mixed text parts", () => {
  test("text parts are joined with a line feed", async () => {
    const mail = await mailer.store(
      multipart("mixed", "mix", "--mix", "Content-Type: text/plain", "", "First part", "--mix", "Content-Type: text/plain", "", "Second part", "--mix--")
    );
    expect(body(mail)).toEqual({ text: "First part\nSecond part", html: "First part\nSecond part" });
  });

  test("beside an HTML part, each text part is rendered to HTML and each HTML part to text", async () => {
    const mail = await mailer.store(
      multipart(
        "mixed",
        "mix",
        "--mix",
        "Content-Type: text/plain",
        "",
        "First part, see www.example.com or https://example.org/a?b=1&c=2. Mail me@remote.example!",
        "",
        "New <paragraph> & more",
        "same paragraph",
        "--mix",
        "Content-Type: text/html",
        "",
        '<p>Second <b>part</b> <a href="https://example.net/">here</a></p>',
        "--mix",
        "Content-Type: text/plain",
        "",
        "Third part",
        "--mix--"
      )
    );
    expect(body(mail)).toEqual({
      text:
        "First part, see www.example.com or https://example.org/a?b=1&c=2. Mail me@remote.example!\n\nNew <paragraph> & more\nsame paragraph\n" +
        "Second part here [https://example.net/]\n" +
        "Third part",
      html:
        '<p>First part, see <a href="http://www.example.com">www.example.com</a> or <a href="https://example.org/a?b=1&c=2">https://example.org/a?b=1&amp;c=2</a>. ' +
        'Mail <a href="mailto:me@remote.example">me@remote.example</a>!</p><p>New &lt;paragraph&gt; &amp; more<br/>same paragraph</p><br/>\n' +
        '<p>Second <b>part</b> <a href="https://example.net/">here</a></p><br/>\n' +
        "<p>Third part</p>",
    });
  });

  test("text rendered to HTML escapes non-ASCII characters as hexadecimal references", async () => {
    const mail = await mailer.store(
      octets(
        multipart("mixed", "mix", "--mix", "Content-Type: text/plain; charset=utf-8", ""),
        Buffer.from("Grüße ✓ 'quoted' `tick`\r\n"),
        crlf("--mix", "Content-Type: text/html", "", "<p>x</p>", "--mix--")
      )
    );
    expect(body(mail)).toEqual({
      text: "Grüße ✓ 'quoted' `tick`\nx",
      html: "<p>Gr&#xFC;&#xDF;e &#x2713; &#x27;quoted&#x27; &#x60;tick&#x60;</p><br/>\n<p>x</p>",
    });
  });

  test("a part with no headers is text/plain", async () => {
    const mail = await mailer.store(multipart("mixed", "mix", "--mix", "", "Headerless part", "--mix--"));
    expect(body(mail)).toEqual({ text: "Headerless part", html: "Headerless part" });
  });

  test("empty parts contribute nothing", async () => {
    const mail = await mailer.store(
      multipart("mixed", "mix", "--mix", "", "--mix", "Content-Type: text/plain", "", "", "--mix", "Content-Type: text/plain", "", "kept", "--mix--")
    );
    expect(body(mail)).toEqual({ text: "kept", html: "kept" });
  });

  test("nested multiparts are walked in order", async () => {
    const mail = await mailer.store(
      multipart(
        "mixed",
        "outer",
        "--outer",
        "Content-Type: text/plain",
        "",
        "Before",
        "--outer",
        'Content-Type: multipart/mixed; boundary="inner"',
        "",
        "--inner",
        "Content-Type: text/plain",
        "",
        "Inner one",
        "--inner",
        'Content-Type: multipart/mixed; boundary="innermost"',
        "",
        "--innermost",
        "Content-Type: text/plain",
        "",
        "Innermost",
        "--innermost--",
        "--inner",
        "Content-Type: text/plain",
        "",
        "Inner two",
        "--inner--",
        "--outer",
        "Content-Type: text/plain",
        "",
        "After",
        "--outer--"
      )
    );
    expect(mail.text).toBe("Before\nInner one\nInnermost\nInner two\nAfter");
  });

  test("an alternative inside mixed, followed by more text", async () => {
    const mail = await mailer.store(
      multipart(
        "mixed",
        "outer",
        "--outer",
        'Content-Type: multipart/alternative; boundary="alt"',
        "",
        "--alt",
        "Content-Type: text/plain",
        "",
        "Plain body",
        "--alt",
        "Content-Type: text/html",
        "",
        "<p>HTML body</p>",
        "--alt--",
        "--outer",
        "Content-Type: text/plain",
        "",
        "Footer added by a list",
        "--outer--"
      )
    );
    expect(body(mail)).toEqual({
      text: "Plain body\nFooter added by a list",
      html: "<p>HTML body</p><br/>\n<p>Footer added by a list</p>",
    });
  });
});

describe("boundaries", () => {
  test("a multipart with no closing boundary keeps what it has", async () => {
    const mail = await mailer.store(
      multipart("mixed", "mix", "--mix", "Content-Type: text/plain", "", "Part one", "--mix", "Content-Type: text/plain", "", "Part two, never closed")
    );
    expect(mail.text).toBe("Part one\nPart two, never closed\n");
  });

  test("boundaries are case-sensitive", async () => {
    const mail = await mailer.store(
      multipart("mixed", "MiX", "--mix", "Content-Type: text/plain", "", "wrong case", "--MiX", "Content-Type: text/plain", "", "right case", "--MiX--")
    );
    expect(mail.text).toBe("right case");
  });

  test("a line that only starts with the boundary is content", async () => {
    const mail = await mailer.store(multipart("mixed", "ab", "--ab", "Content-Type: text/plain", "", "line", "--abc is not a boundary", "--ab--"));
    expect(mail.text).toBe("line\n--abc is not a boundary");
  });

  test("an unquoted boundary, and one with spaces and punctuation", async () => {
    const unquoted = await mailer.store(
      crlf(...HEAD, "Content-Type: multipart/mixed; boundary=simple_1-2.3", "", "--simple_1-2.3", "", "unquoted", "--simple_1-2.3--")
    );
    expect(unquoted.text).toBe("unquoted");
    const spaced = await mailer.store(
      crlf(...HEAD, 'Content-Type: multipart/mixed; boundary="a b\'()+_,-./:=?"', "", "--a b'()+_,-./:=?", "", "spaced", "--a b'()+_,-./:=?--")
    );
    expect(spaced.text).toBe("spaced");
  });

  test("a boundary line with trailing spaces is not recognized", async () => {
    // NOTE: RFC 2046 §5.1.1 allows linear whitespace after a boundary
    // delimiter; here such a line is not a delimiter, so the part is never
    // opened and its text is lost.
    const mail = await mailer.store(
      crlf(...HEAD, "Content-Type: multipart/mixed; boundary=pad", "", "--pad  ", "Content-Type: text/plain", "", "padded boundary", "--pad--")
    );
    expect(body(mail)).toEqual({ text: "", html: "" });
  });

  test("a multipart with no boundary parameter stores no body", async () => {
    // NOTE: the content is silently dropped rather than shown as text.
    const mail = await mailer.store(crlf(...HEAD, "Content-Type: multipart/mixed", "", "--x", "Content-Type: text/plain", "", "lost", "--x--"));
    expect(body(mail)).toEqual({ text: "", html: "" });
    expect(mail.attachments).toEqual([]);
  });

  test("a multipart whose boundary never appears stores no body", async () => {
    const mail = await mailer.store(multipart("mixed", "nope", "This text has no boundary lines at all.", "Second line."));
    expect(body(mail)).toEqual({ text: "", html: "" });
  });

  test("bare-LF line endings around boundaries work", async () => {
    const mail = await mailer.store(
      `${HEAD.join("\n")}\nContent-Type: multipart/mixed; boundary="lf"\n\n--lf\nContent-Type: text/plain\n\nfirst\nline\n--lf\nContent-Type: text/plain\n\nsecond\n--lf--\n`
    );
    expect(mail.text).toBe("first\nline\nsecond");
  });
});

describe("embedded messages and reports", () => {
  const forwarded = [
    "From: Original Author <orig@remote.example>",
    "To: Someone <some@remote.example>, other@remote.example",
    "Cc: cc@remote.example",
    "Subject: Original =?utf-8?Q?caf=C3=A9?= <subject>",
    "Date: Mon, 02 Mar 2026 08:00:00 +0000",
  ];

  test("an inline message/rfc822 is shown in the text with a header block", async () => {
    const mail = await mailer.store(
      multipart(
        "mixed",
        "mix",
        "--mix",
        "Content-Type: text/plain",
        "",
        "See the forwarded message.",
        "--mix",
        "Content-Type: message/rfc822",
        "Content-Disposition: inline",
        "",
        ...forwarded,
        "Content-Type: text/plain",
        "",
        "Original body text.",
        "--mix--"
      )
    );
    const text =
      "See the forwarded message.\n" +
      "\n" +
      'From: "Original Author" <orig@remote.example>\n' +
      "Subject: Original café <subject>\n" +
      "Date: Mon, 02 Mar 2026 08:00:00 GMT\n" +
      'To: "Someone" <some@remote.example>, other@remote.example\n' +
      "Cc: cc@remote.example\n" +
      "\n" +
      "Original body text.";
    expect(body(mail)).toEqual({ text, html: text });
    expect(mail.attachments).toEqual([]);
    // The outer message's own headers are untouched by the inner ones.
    expect(mail.subject).toBe("Body");
    expect(mail.from_address).toEqual([{ name: "Remote Sender", address: "sender@remote.example" }]);
  });

  test("with HTML involved the header block is a table", async () => {
    const mail = await mailer.store(
      multipart(
        "mixed",
        "mix",
        "--mix",
        "Content-Type: text/html",
        "",
        "<p>See forwarded.</p>",
        "--mix",
        "Content-Type: message/rfc822",
        "Content-Disposition: inline",
        "",
        "From: Original Author <orig@remote.example>",
        "To: Team: a@remote.example, B <b@remote.example>;",
        "Subject: Orig & <b>",
        "Date: Mon, 02 Mar 2026 08:00:00 +0000",
        "Content-Type: text/html",
        "",
        "<p>Original <b>html</b></p>",
        "--mix--"
      )
    );
    // NOTE: the table markup is malformed as produced: each row ends its value
    // cell with `<td>` instead of `</td>`, and the table closes with `<table>`.
    expect(mail.html).toBe(
      "<p>See forwarded.</p><br/>\n" +
        '<table class="mp_head">' +
        '<tr><td class="mp_head_key">From:</td><td class="mp_head_value"><span class="mp_address_group"><span class="mp_address_name">Original Author</span> ' +
        '&lt;<a href="mailto:orig@remote.example" class="mp_address_email">orig@remote.example</a>&gt;</span><td></tr>\n' +
        '<tr><td class="mp_head_key">Subject:</td><td class="mp_head_value"><strong>Orig &#x26; &#x3C;b&#x3E;</strong><td></tr>\n' +
        '<tr><td class="mp_head_key">Date:</td><td class="mp_head_value">Mon, 02 Mar 2026 08:00:00 GMT<td></tr>\n' +
        '<tr><td class="mp_head_key">To:</td><td class="mp_head_value"><span class="mp_address_group"><span class="mp_address_name">Team: </span>' +
        '<span class="mp_address_group"><a href="mailto:a@remote.example" class="mp_address_email">a@remote.example</a></span>, ' +
        '<span class="mp_address_group"><span class="mp_address_name">B</span> &lt;<a href="mailto:b@remote.example" class="mp_address_email">b@remote.example</a>&gt;</span>;' +
        "</span><td></tr><table><br/>\n" +
        "<p>Original <b>html</b></p>"
    );
    // No part is plain text, so the text column is derived from the joined
    // HTML when the row is written: tags dropped, wrapped at 80 columns.
    expect(mail.text).toBe(
      "See forwarded.\nFrom:Original Author <orig@remote.example> Subject:Orig & <b> Date:Mon, 02 Mar\n" +
        "2026 08:00:00 GMT To:Team: a@remote.example, B <b@remote.example>;\nOriginal html"
    );
    expect(mail.spam_reasons).toEqual(["HTML-only email with no text alternative"]);
  });

  test("message/rfc822 as an attachment, or with no disposition, is stored as a file", async () => {
    const inner = ["From: orig@remote.example", "Subject: Original", "", "Original body text."];
    const attached = await mailer.store(
      multipart(
        "mixed",
        "mix",
        "--mix",
        "Content-Type: text/plain",
        "",
        "Forward as attachment.",
        "--mix",
        'Content-Type: message/rfc822; name="orig.eml"',
        'Content-Disposition: attachment; filename="orig.eml"',
        "",
        ...inner,
        "--mix--"
      )
    );
    expect(attached.text).toBe("Forward as attachment.");
    expect(files(attached)).toEqual([{ filename: "orig.eml", contentType: "message/rfc822", size: 67 }]);
    expect(fileText(attached)).toBe(inner.join("\r\n"));

    const bare = await mailer.store(
      multipart("mixed", "mix", "--mix", "Content-Type: text/plain", "", "Forward.", "--mix", "Content-Type: message/rfc822", "", ...inner, "--mix--")
    );
    expect(bare.text).toBe("Forward.");
    expect(files(bare)).toEqual([{ filename: "attachment", contentType: "message/rfc822", size: 67 }]);
  });

  test("an inline message/rfc822 in base64 is not opened: it is stored as a file, decoded", async () => {
    const inner = "From: orig@remote.example\r\nSubject: Original\r\n\r\nOriginal body text.\r\n";
    const mail = await mailer.store(
      multipart(
        "mixed",
        "mix",
        "--mix",
        "Content-Type: text/plain",
        "",
        "Forward.",
        "--mix",
        "Content-Type: message/rfc822",
        "Content-Disposition: inline",
        "Content-Transfer-Encoding: base64",
        "",
        Buffer.from(inner).toString("base64"),
        "--mix--"
      )
    );
    expect(mail.text).toBe("Forward.");
    expect(files(mail)).toEqual([{ filename: "attachment", contentType: "message/rfc822", size: inner.length }]);
    expect(fileText(mail)).toBe(inner);
  });

  test("a delivery report: the status part is text, the returned headers a file", async () => {
    const mail = await mailer.store(
      crlf(
        ...HEAD,
        'Content-Type: multipart/report; report-type=delivery-status; boundary="rep"',
        "",
        "--rep",
        "Content-Type: text/plain",
        "",
        "Your message could not be delivered.",
        "--rep",
        "Content-Type: message/delivery-status",
        "",
        "Reporting-MTA: dns; mx.remote.example",
        "",
        "Final-Recipient: rfc822; nobody@remote.example",
        "Action: failed",
        "Status: 5.1.1",
        "--rep",
        "Content-Type: text/rfc822-headers",
        "",
        "From: hello@mail.test",
        "Subject: Original",
        "--rep--"
      )
    );
    const text =
      "Your message could not be delivered.\n" +
      "Reporting-MTA: dns; mx.remote.example\n\nFinal-Recipient: rfc822; nobody@remote.example\nAction: failed\nStatus: 5.1.1";
    expect(body(mail)).toEqual({ text, html: text });
    expect(files(mail)).toEqual([{ filename: "attachment", contentType: "text/rfc822-headers", size: 40 }]);
    expect(fileText(mail)).toBe("From: hello@mail.test\r\nSubject: Original");
  });
});

describe("limits of the parser", () => {
  /** A multipart/mixed of `count` one-line text parts. */
  const parts = (count: number) => {
    const lines: string[] = [];
    for (let i = 0; i < count; i++) lines.push("--p", "", "x");
    return multipart("mixed", "p", ...lines, "--p--");
  };

  test("a message of 1000 parts, the multipart itself included, is stored", async () => {
    const mail = await mailer.store(parts(999));
    expect(mail.text).toBe(Array.from({ length: 999 }, () => "x").join("\n"));
  });

  test("one part more fails the delivery with a transient 450", async () => {
    // NOTE: as with the header limit, a permanent condition is answered with a
    // code that tells the sender to retry.
    const { final } = await mailer.send(parts(1000));
    expect(final.raw).toBe("450 Max allowed child nodes exceeded");
    expect((await mailer.store(single([], "after"))).text).toBe("after\n");
  });

  test("text bodies over 2 MiB in total are stored as files instead of text", async () => {
    const line = `${"0123456789".repeat(7)}\r\n`;
    const big = line.repeat(30_000);
    expect(big.length).toBeGreaterThan(2 * 1024 * 1024);
    const mail = await mailer.store(single(["Content-Type: text/plain"], big.slice(0, -2)));
    expect(body(mail)).toEqual({ text: "", html: "" });
    expect(files(mail)).toEqual([{ filename: "body.txt", contentType: "text/plain", size: big.length }]);

    const html = await mailer.store(single(["Content-Type: text/html"], `<p>${big.slice(0, -2)}</p>`));
    expect(body(html)).toEqual({ text: "", html: "" });
    expect(files(html)).toEqual([{ filename: "body.html", contentType: "text/html", size: big.length + 7 }]);
  });

  test("the 2 MiB is shared: a second text part that would pass it becomes the file", async () => {
    const chunk = `${"a".repeat(98)}\r\n`.repeat(15_000);
    const mail = await mailer.store(
      multipart(
        "mixed",
        "mix",
        "--mix",
        "Content-Type: text/plain",
        "",
        chunk.slice(0, -2),
        "--mix",
        'Content-Type: text/plain; name="second.txt"',
        "",
        chunk.slice(0, -2),
        "--mix--"
      )
    );
    expect(mail.text.length).toBe(15_000 * 99 - 1);
    expect(files(mail)).toEqual([{ filename: "second.txt", contentType: "text/plain", size: chunk.length - 2 }]);
  });
});
