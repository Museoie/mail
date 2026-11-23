/**
 * The messages the golden parser fixture is made from: every message of the
 * TypeScript parser tests, hand-written malformed ones, and seeded random
 * MIME trees with random damage.
 */

/** Raw bytes as Latin-1 text, a piece at a time; `[text, n]` is the piece repeated. */
export type Raw = (string | [string, number])[];

export const rawBytes = (raw: Raw): Uint8Array =>
  Uint8Array.from(Buffer.from(raw.map((piece) => (typeof piece === "string" ? piece : piece[0].repeat(piece[1]))).join(""), "latin1"));

export interface ParserCase {
  name: string;
  raw: Raw;
}

interface Rng {
  next(): number;
  int(n: number): number;
  pick<T>(list: readonly T[]): T;
  chance(p: number): boolean;
  several(list: readonly string[], min: number, max: number, joiner?: string): string;
}

/** Text as the bytes of its UTF-8 encoding. */
const u = (text: string) => Buffer.from(text, "utf8").toString("latin1");
const b64 = (text: string) => Buffer.from(text, "latin1").toString("base64");
const b64Lines = (text: string, width = 76, eol = "\r\n") => b64(text).replace(new RegExp(`.{${width}}`, "g"), `$&${eol}`);
/** A message from its lines, CRLF-terminated as SMTP delivers them. */
const message = (...lines: string[]) => lines.map((line) => line + "\r\n").join("");
const multipart = (boundary: string, subtype: string, ...parts: string[][]) =>
  message(`Content-Type: multipart/${subtype}; boundary="${boundary}"`, "", ...parts.flatMap((part) => [`--${boundary}`, ...part]), `--${boundary}--`);
const pseudoRandom = (length: number) => String.fromCharCode(...Array.from({ length }, (_, i) => (i * 131 + 7) % 251));

const PNG = String.fromCharCode(0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a);

const handWritten = (): ParserCase[] => {
  const cases: ParserCase[] = [];
  const add = (name: string, ...raw: Raw) => cases.push({ name, raw });

  // --- The messages of parser.test.ts ---
  add("single part", message("From: Sender <sender@example.org>", "To: rcpt@example.com", "Subject: Hello", "Message-ID: <id-1@example.org>", "Date: Tue, 27 Oct 2026 10:00:00 +0900", "", "line one", "line two"));
  add("encoded-word subject", message("Subject: =?EUC-KR?B?x9Gx2w==?= =?ISO-2022-JP?B?GyRCRnxLXDhsGyhC?=", "", "body"));
  add("repeated headers", message("From: one@x.org", "From: two@x.org", "To: a@x.org", "To: b@x.org", "Subject: first", "Subject: last", "", "x"));
  add("bare message id, unreadable date", message("Message-ID: bare@example.org", "Date: not a date", "", "x"));
  add("raw utf-8 subject", message(u("Subject: 8비트 제목"), "", "body"));
  add("alternative", multipart("b", "alternative", ["Content-Type: text/plain", "", "Plain"], ["Content-Type: text/html", "", "<p>HTML</p>"]));
  add("html only", message("Content-Type: text/html", "", "<h1>Title</h1><p>Para <a href='https://bit.ly/3abc'>link</a> <img src='cid:a' alt='Logo'></p>"));
  add("plain part beside an alternative", message('Content-Type: multipart/mixed; boundary="m"', "", "--m", 'Content-Type: multipart/alternative; boundary="a"', "", "--a", "Content-Type: text/plain", "", "Main", "--a", "Content-Type: text/html", "", "<p>Main</p>", "--a--", "--m", "Content-Type: text/plain", "", "Info: https://lists.example.org/info & more", "--m--"));
  add("shift_jis base64", message("Content-Type: text/plain; charset=shift_jis", "Content-Transfer-Encoding: base64", "", "grGC8Q=="));
  add("us-ascii with 8-bit", message("Content-Type: text/plain; charset=us-ascii", "", u("café")));
  add("quoted-printable", message("Content-Type: text/plain; charset=utf-8", "Content-Transfer-Encoding: quoted-printable", "", "caf=C3=A9 =", "au lait  ", "=3D"));
  add("flowed delsp", message("Content-Type: text/plain; format=flowed; delsp=yes", "", "This is a long ", "flowed line.", "", "> quoted"));
  add("lf only", 'Content-Type: multipart/alternative; boundary="b"\n\n--b\nContent-Type: text/plain\n\nA\nB\n--b--\n');
  add("multipart without boundary", message("Content-Type: multipart/mixed", "", "some text"));
  add("inline embedded message", multipart("m", "mixed", ["Content-Type: text/plain", "", "See below."], ["Content-Type: message/rfc822", "Content-Disposition: inline", "", "From: Inner <inner@x.org>", "Subject: Inner subject", "Date: Mon, 26 Oct 2026 09:00:00 +0000", "", "Inner body"]));
  add("delivery status", multipart("r", "report", ["Content-Type: text/plain", "", "Failed."], ["Content-Type: message/delivery-status", "", "Action: failed"]));
  add("base64 attachment", multipart("m", "mixed", ["Content-Type: text/plain", "", "See attached."], ['Content-Type: application/pdf; name="report.pdf"', 'Content-Disposition: attachment; filename="report.pdf"', "Content-Transfer-Encoding: base64", "", b64Lines(pseudoRandom(3000))]));
  add("base64 attachment of a hundred thousand bytes", 'Content-Type: multipart/mixed; boundary="m"\r\n\r\n--m\r\nContent-Type: text/plain\r\n\r\nSee attached.\r\n--m\r\nContent-Type: application/pdf; name="report.pdf"\r\nContent-Disposition: attachment; filename="report.pdf"\r\nContent-Transfer-Encoding: base64\r\n\r\n', ["QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVowMTIzNDU2Nzg5QUJDREVGR0hJSktMTU5PUFFSU1RVVldY\r\n", 1755], "QUJDRA==\r\n--m--\r\n");
  add("rfc 2231 and encoded-word filenames", multipart("m", "mixed", ["Content-Type: application/pdf", "Content-Disposition: attachment;", " filename*0*=UTF-8''%EB%B3%B4%EA%B3%A0;", ' filename*1=".pdf"', "", "x"], ['Content-Type: application/octet-stream; name="=?UTF-8?B?7ZWc6riALmRvY3g=?="', "Content-Disposition: attachment", "", "y"]));
  add("octet-stream named after its extension", multipart("m", "mixed", ["Content-Type: text/plain", "", "x"], ["Content-Type: application/octet-stream", 'Content-Disposition: attachment; filename="photo.jpg"', "", "jpeg"], ["Content-Type: application/x-custom", "Content-Disposition: attachment", "", "raw"]));
  add("forwarded message as attachment", multipart("m", "mixed", ["Content-Type: text/plain", "", "Fwd"], ["Content-Type: message/rfc822", "", "Subject: inner", "", "inner body"]));
  add("cid image", multipart("r", "related", ["Content-Type: text/html", "", '<img src="cid:logo@x">'], ["Content-Type: image/png", "Content-ID: <logo@x>", "Content-Transfer-Encoding: base64", "", b64(PNG)]));
  add("threading headers", message("Message-ID: <c@x>", "In-Reply-To: <b@x>", "References: <a@x>", " <b@x>", "Subject: Re: plan", "", "body"));
  add("empty in-reply-to", message("Message-ID: <c@x>", "In-Reply-To: ", "Subject: hi", "", "body"));

  // --- Sizes and limits ---
  add("cid image past the inline budget", 'Content-Type: multipart/related; boundary="r"\r\n\r\n--r\r\nContent-Type: text/html\r\n\r\n<img src="cid:big@x">\r\n--r\r\nContent-Type: image/png\r\nContent-ID: <big@x>\r\nContent-Transfer-Encoding: base64\r\n\r\n', ["QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVowMTIzNDU2Nzg5QUJDREVGR0hJSktMTU5PUFFSU1RVVldY\r\n", 70_000], "QUJD\r\n--r--\r\n");
  add("two cid images, the second past the budget", 'Content-Type: multipart/related; boundary="r"\r\n\r\n--r\r\nContent-Type: text/html\r\n\r\n<img src="cid:one"><img src="cid:two"><img src=\'cid:one\'>\r\n--r\r\nContent-Type: image/png\r\nContent-ID: <one>\r\nContent-Transfer-Encoding: base64\r\n\r\n', ["QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVowMTIzNDU2Nzg5QUJDREVGR0hJSktMTU5PUFFSU1RVVldY\r\n", 40_000], "\r\n--r\r\nContent-Type: image/gif\r\nContent-ID: <two>\r\nContent-Transfer-Encoding: base64\r\n\r\n", ["QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVowMTIzNDU2Nzg5QUJDREVGR0hJSktMTU5PUFFSU1RVVldY\r\n", 40_000], "\r\n--r--\r\n");
  add("html body past the text budget", "Content-Type: text/html\r\n\r\n<p>", ["x", 2 * 1024 * 1024], "</p>");
  add("plain body exactly at the text budget", "Content-Type: text/plain\r\n\r\n", ["y", 2 * 1024 * 1024]);
  add("plain body one past the text budget", "Content-Type: text/plain\r\n\r\n", ["y", 2 * 1024 * 1024 + 1]);
  add("second text part past what the first left of the budget", 'Content-Type: multipart/mixed; boundary="m"\r\n\r\n--m\r\nContent-Type: text/plain\r\n\r\n', ["a", 1_500_000], '\r\n--m\r\nContent-Type: text/html; name="second.html"\r\n\r\n', ["b", 700_000], "\r\n--m\r\nContent-Type: text/plain\r\n\r\nthird fits\r\n--m--\r\n");
  add("quoted-printable body past the text budget", "Content-Type: text/plain\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n", ["=41=42 c=\r\n", 300_000], "end");
  add("long plain line beside html", 'Content-Type: multipart/mixed; boundary="m"\r\n\r\n--m\r\nContent-Type: text/html\r\n\r\n<p>hi</p>\r\n--m\r\nContent-Type: text/plain\r\n\r\n', ["a", 2 * 1024 * 1024 - 1024], "\r\n--m--\r\n");
  add("html longer than the text rendering reads", "Content-Type: text/html\r\n\r\n", ["<p>word</p>", 30_000], "<p>tail</p>");
  add("html cut inside a surrogate pair for its text", "Content-Type: text/html; charset=utf-8\r\n\r\na", [u("😀"), 140_000]);
  add("too many parts", 'Content-Type: multipart/mixed; boundary="m"\r\n\r\n', ["--m\r\nContent-Type: text/plain\r\n\r\nx\r\n", 1001], "--m--\r\n");
  add("as many parts as allowed", 'Content-Type: multipart/mixed; boundary="m"\r\n\r\n', ["--m\r\nContent-Type: text/plain\r\n\r\nx\r\n", 999], "--m--\r\n");
  add("header line too large", "X-Filler: ", ["a", 1024 * 1024 + 10], "\r\n\r\nbody");
  add("header block too large in many lines", ["X-Filler: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n", 12_000], "\r\nbody");
  add("header block just under the limit", ["X-Filler: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n", 10_000], "Subject: after the filler\r\n\r\nbody");
  add("subject of carriage returns", "Subject: a", ["\r", 900_000], "b\r\n\r\nbody");
  add("encoding of open parentheses", "Content-Transfer-Encoding: ", ["(", 900_000], "\r\n\r\nbody");
  add("recipient of at signs", "To: @@", ["a!", 450_000], "\r\n\r\nbody");
  add("deeply folded subject", "Subject: a", ["\r\n b", 225_000], "\r\n\r\nbody");
  add("flowed lines by the thousand", "Content-Type: text/plain; format=flowed; delsp=yes\r\n\r\n", ["ab \r\n", 400_000]);
  add("large quoted-printable attachment", 'Content-Type: multipart/mixed; boundary="m"\r\n\r\n--m\r\nContent-Type: application/x-data\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n', ["=00=FF=C3=A9 plain text line with trailing space \r\n", 20_000], "soft=\r\nbreak\r\n--m--\r\n");
  add("large binary attachment with no line breaks", 'Content-Type: multipart/mixed; boundary="m"\r\n\r\n--m\r\nContent-Type: application/x-data\r\nContent-Transfer-Encoding: binary\r\n\r\n', [pseudoRandom(251).replace(/\n/g, "x"), 4000], "\r\n--m--\r\n");
  add("large attachment of lone carriage returns", 'Content-Type: multipart/mixed; boundary="m"\r\n\r\n--m\r\nContent-Type: application/x-data\r\n\r\n', ["ab\r", 100_000], "\r\n--m--\r\n");
  add("references by the thousand", "References:", [" <id-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa@example.org>", 2000], " <last@example.org>\r\n\r\nbody");

  // --- Headers ---
  add("no headers at all", "\r\njust a body\r\n");
  add("empty input", "");
  add("only a blank line", "\r\n");
  add("only a line feed", "\n");
  add("headers with no body and no blank line", "Subject: only headers\r\nTo: a@b.c");
  add("headers ending in one line break", "Subject: only headers\r\n");
  add("body with no trailing line break", "Subject: s\r\n\r\nbody");
  add("carriage-return-only line endings", "Subject: s\rTo: a@b.c\r\rbody\rmore");
  add("mixed line endings", "Subject: s\nTo: a@b.c\r\n\nbody\r\nmore\nend\r");
  add("mbox from line", message("From sender@example.org Tue Oct 27 10:00:00 2026", "Subject: after mbox", "", "body"));
  add("lowercase mbox from line", message("from somewhere", "Subject: after mbox", "", "body"));
  add("from line that is a header", message("From: real@example.org", "Subject: s", "", "body"));
  add("first line is a continuation", message(" leading continuation", "Subject: s", "", "body"));
  add("header without a colon", message("NoColonHere", "Subject: s", "", "body"));
  add("header with an empty name", message(": value", "Subject: s", "", "body"));
  add("header names in odd case and spacing", message("SUBJECT : spaced", "  To\t: a@b.c", "x-À: latin", "", "body"));
  add("folded with tabs and many spaces", message("Subject: one", "\ttwo", "     three", "To: a@b.c,", " d@e.f", "", "body"));
  add("folded header ending the block", "Subject: one\r\n two");
  add("subject ending in a no-break space byte", message(u("Subject: voilà"), u("X-Other: Привет Р"), u("To: 😠 <a@b.c>"), "", "body"));
  add("subject with invalid utf-8", message("Subject: bad \xff\xfe \xe2\x82 bytes", "", "body"));
  add("subject in latin-1 bytes", message("Subject: caf\xe9 cr\xe8me", "From: Ren\xe9 <rene@x.fr>", "", "body"));
  add("subject with nul and controls", message("Subject: a\x00b\x01c\x7fd", "", "body"));
  add("empty subject and date", message("Subject:", "Date:", "From:", "To:", "Message-ID:", "", "body"));
  add("subject that decodes to nothing", message("Subject: =?utf-8?q??=", "Subject: =?utf-8?B??=", "", "body"));
  add("subject encoded words in many charsets", message("Subject: =?utf-8?B?7ZWc6riA?= =?iso-8859-1?Q?caf=E9?= =?windows-1251?B?7/Do4uXy?=", " =?GB18030?B?1tDOxA==?= =?Shift_JIS?B?k/qWe4zq?= =?koi8-r?Q?=F0=D2=C9=D7=C5=D4?= plain =?x-nope?Q?raw?=", "", "body"));
  add("subject split inside a character", message("Subject: =?UTF-8?Q?=ED=95?= =?UTF-8?Q?=9C?= =?UTF-8?B?7ZU=?=", "\t=?UTF-8?B?nA==?=", "", "body"));
  add("dates in many shapes", message("Date: 27 Oct 2026 10:00 EST", "", "body"));
  for (const [i, date] of ["Tue, 27 Oct 2026 10:00:00 +0900 (KST)", "2026-10-27T10:00:00Z", "Tuesday, 27-Oct-26 10:00:00 GMT", "Oct 27 10:00:00 2026", "1", "27/10/2026", "Tue, 27 Oct 2026 25:00:00", "=?utf-8?q?Tue,_27_Oct_2026?=", u("Tue, 27 Oct 2026 10:00:00\x0b é"), "Tue, 27 Oct 2026 10:00:00\x0b", "Sep 14 275760", "Tue, 27 Oct 2026 10:00:00 -0000"].entries()) {
    add(`date ${i}`, message(`Date: ${date}`, "Subject: dated", "", "body"));
  }
  add("two dates, the last unreadable", message("Date: Tue, 27 Oct 2026 10:00:00 +0900", "Date: nope", "", "body"));
  add("message ids", message("Message-ID: <a@b>", "Message-ID: =?utf-8?q?enc=40x?=", "", "body"));
  add("message id with spaces and brackets", message("Message-ID:   <<weird id>>  ", "", "body"));
  add("message id of 512 characters", message(`Message-ID: <${"x".repeat(495)}@remote.example>`, "", "body"));
  add("message id too long to store", message(`Message-ID: ${"x".repeat(496)}@remote.example`, "", "body"));
  add("too long a message id after a good one", message("Message-ID: <kept@x>", `Message-ID: <${"x".repeat(496)}@remote.example>`, "", "body"));
  add("nul in headers and body", message("Subject: nu\0l", "From: Jo\0e <jo@x.org>", "Message-ID: <i\0d@x>", "", "a\0b"));
  add("in-reply-to list and repeated references", message("In-Reply-To: <a@x> <b@x> (comment) <c@x>", "In-Reply-To: no ids", "References: <r1@x>", "References: junk", "References: <r2@x> <r2@x> <r3@x>", "", "body"));
  add("address headers of every kind", message("From: =?UTF-8?B?7ZmN6ri464+Z?= <hong@example.kr>", 'To: "Doe, John" <john@example.org>, Team: a@x.org, b@x.org;', "Cc: undisclosed-recipients:;", "Cc: c@x.org (C)", "Bcc: hidden@xn--9n2bp8q.com", "Reply-To: reply@x.org", "Reply-To: last-reply@x.org", "", "body"));
  add("empty address headers", message("To:", "To: ", "Cc: ,", "Bcc: ;", "", "body"));
  add("address list encoded as one word", message(`To: =?UTF-8?B?${Buffer.from("A <a@x.org>, b@y.org").toString("base64")}?=`, "", "body"));
  add("content-id on the root", message("Content-ID: <root@x>", "Content-Type: application/pdf", "", "pdf"));
  add("duplicate content headers", message("Content-Type: text/html", "Content-Type: text/plain", "Content-Transfer-Encoding: base64", "Content-Transfer-Encoding: 7bit", "", "PGI+Ym9sZDwvYj4="));
  add("content-type with a comment and odd case", message("CONTENT-TYPE: TEXT/Plain (comment); CHARSET=\"ISO-8859-1\"", "Content-Transfer-Encoding: Quoted-Printable (qp)", "", "caf=E9"));
  add("content-type with no value", message("Content-Type: ; charset=utf-8", "", "body"));
  add("content-type empty on a part", multipart("m", "mixed", ["Content-Type:", "", "typeless part"], ["Content-Type: ;x=y", 'Content-Disposition: inline; filename="n.txt"', "", "second"]));
  add("unknown transfer encoding", message("Content-Type: text/plain", "Content-Transfer-Encoding: x-uuencode", "", "begin 644 file", "end"));
  add("base64 body with junk", message("Content-Type: text/plain; charset=utf-8", "Content-Transfer-Encoding: base64", "", "7ZWc6riA", "!!", " IHRlc3Q=", "====", "dGFpbA"));
  add("uppercase base64", message("Content-Type: text/plain", "Content-Transfer-Encoding: BASE64", "", "aGVsbG8="));
  add("8bit and binary", multipart("m", "mixed", ["Content-Type: text/plain; charset=utf-8", "Content-Transfer-Encoding: 8bit", "", u("팔 비트")], ["Content-Type: text/plain; charset=utf-8", "Content-Transfer-Encoding: binary", "", u("바이너리")]));
  add("quoted-printable edge cases", message("Content-Type: text/plain; charset=utf-8", "Content-Transfer-Encoding: quoted-printable", "", "=", "a=", "=ZZ =4 =c3=a9 =C3=A9", "trailing \t ", "soft =  ", "break", "==41", "=\r", "end="));
  add("charsets", multipart("m", "mixed",
    ["Content-Type: text/plain; charset=iso-8859-1", "", "caf\xe9"],
    ["Content-Type: text/plain; charset=windows-1252", "", "\x93quoted\x94 \x80"],
    ["Content-Type: text/plain; charset=euc-kr", "", "\xc7\xd1\xb1\xdb"],
    ["Content-Type: text/plain; charset=iso-2022-jp", "", "\x1b$BF|K\\8l\x1b(B"],
    ["Content-Type: text/plain; charset=utf-16", "Content-Transfer-Encoding: base64", "", "//5oAGkA"],
    ["Content-Type: text/plain; charset=utf-16", "Content-Transfer-Encoding: base64", "", "AGgAaQ=="],
    ["Content-Type: text/plain; charset=utf-16be", "Content-Transfer-Encoding: base64", "", "/v8AaABp"],
    ["Content-Type: text/plain; charset=x-unknown", "", u("unknown é")],
    ["Content-Type: text/plain; charset=\"\"", "", u("empty é")],
    ["Content-Type: text/plain; charset=utf-8", "", "\xef\xbb\xbfbom then bad \xff"],
    ["Content-Type: text/plain; charset=unicode-1-1-utf-8", "", "\xef\xbb\xbfbom dropped"],
    ["Content-Type: text/plain; charset=cp949", "", "\xb6\xca"],
    ["Content-Type: text/plain; charset=latin-2", "", "\xb1\xe6"],
    ["Content-Type: text/plain; charset*=us-ascii'en'utf-8", "", u("tagged é")]));
  add("flowed", message("Content-Type: text/plain; format=flowed", "", "soft ", "break kept ", "", " stuffed", "  two spaces", "-- ", "signature", "> quote ", "> more ", "end "));
  add("flowed delsp with a lone space and a signature", message("Content-Type: text/plain; format=Flowed; delsp=YES", "", " ", "x", "a ", " ", "b", "-- ", "sig ", "", ""));
  add("flowed with carriage returns", "Content-Type: text/plain; format=flowed\r\n\r\na \r b \r\n c\rd \n e");
  add("flowed html", message("Content-Type: text/html; format=flowed", "", "<p>a ", "b</p>"));
  add("body only whitespace", message("Content-Type: text/plain", "", " ", "", "\t"));
  add("empty text body", message("Content-Type: text/plain", ""));
  add("empty html body", message("Content-Type: text/html", ""));
  add("html with only markup", message("Content-Type: text/html", "", "<script>x</script><style>y</style><!-- z -->"));
  add("html the renderer throws on", message("Content-Type: text/html", "", "<p><constructor>x"));
  add("html with a roman list past ten thousand", message("Content-Type: text/html", "", '<ol type="I" start="9999"><li>a</li><li>b</li></ol>'));
  add("html that throws, inside an alternative", multipart("b", "alternative", ["Content-Type: text/plain", "", "Plain"], ["Content-Type: text/html", "", "<p><constructor>x"]));
  add("html that throws, beside a text part", multipart("m", "mixed", ["Content-Type: text/plain", "", "Plain"], ["Content-Type: text/html", "", "<p><constructor>x"]));
  add("html with entities and links for text", message("Content-Type: text/html; charset=utf-8", "", u('<h2>Ünïcode &amp; entities &#x1F600;</h2><ul><li>one<li>two</ul><blockquote>quoted <a href="mailto:a@b.c">mail</a></blockquote><pre> pre  text </pre>')));

  // --- Multipart structure ---
  add("mixed with three texts", multipart("m", "mixed", ["Content-Type: text/plain", "", "one"], ["Content-Type: text/plain", "", "two"], ["Content-Type: text/html", "", "<b>three</b>"]));
  add("alternative with two htmls", multipart("b", "alternative", ["Content-Type: text/html", "", "<p>first</p>"], ["Content-Type: text/html", "", "<p>second</p>"]));
  add("alternative without a plain part", multipart("b", "alternative", ["Content-Type: text/html", "", "<p>only <a href=\"https://x.org\">html</a></p>"]));
  add("related inside alternative inside mixed", message('Content-Type: multipart/mixed; boundary="outer"', "", "preamble text", "--outer", 'Content-Type: multipart/alternative; boundary="alt"', "", "--alt", "Content-Type: text/plain", "", "plain alt", "--alt", 'Content-Type: multipart/related; boundary="rel"', "", "--rel", "Content-Type: text/html", "", '<p>html <img src="cid:img1"> <img src="cid:missing"></p>', "--rel", "Content-Type: image/png", "Content-ID: <img1>", "Content-Transfer-Encoding: base64", "", b64(PNG), "--rel--", "--alt--", "--outer", "Content-Type: application/zip", 'Content-Disposition: attachment; filename="a.zip"', "Content-Transfer-Encoding: base64", "", "UEsDBA==", "--outer--", "epilogue text"));
  add("missing end boundary", message('Content-Type: multipart/mixed; boundary="m"', "", "--m", "Content-Type: text/plain", "", "never closed"));
  add("missing end boundary of a nested multipart", message('Content-Type: multipart/mixed; boundary="m"', "", "--m", 'Content-Type: multipart/alternative; boundary="a"', "", "--a", "Content-Type: text/plain", "", "inner", "--m", "Content-Type: text/plain", "", "after the unclosed inner", "--m--"));
  add("parent end inside a nested multipart", message('Content-Type: multipart/mixed; boundary="m"', "", "--m", 'Content-Type: multipart/alternative; boundary="a"', "", "--a", "Content-Type: text/plain", "", "inner", "--m--", "tail"));
  add("parent end while reading part headers", message('Content-Type: multipart/mixed; boundary="m"', "", "--m", "Content-Type: application/pdf", 'Content-Disposition: attachment; filename="cut.pdf"', "--m--"));
  add("parent end while reading text part headers", message('Content-Type: multipart/mixed; boundary="m"', "", "--m", "Content-Type: text/plain", "--m--", "after"));
  add("sibling boundary while reading part headers", message('Content-Type: multipart/mixed; boundary="m"', "", "--m", "Content-Type: text/plain", "--m", "Content-Type: text/plain", "", "second", "--m--"));
  add("boundary right after the headers", message('Content-Type: multipart/mixed; boundary="m"', "--m", "Content-Type: text/plain", "", "x", "--m--"));
  add("no parts between boundaries", message('Content-Type: multipart/mixed; boundary="m"', "", "--m", "--m", "", "--m", "", "", "--m--"));
  add("only the end boundary", message('Content-Type: multipart/mixed; boundary="m"', "", "--m--"));
  add("end boundary twice and parts after it", message('Content-Type: multipart/mixed; boundary="m"', "", "--m", "Content-Type: text/plain", "", "x", "--m--", "--m", "Content-Type: text/plain", "", "after the end", "--m--"));
  add("duplicate boundary parameter", message('Content-Type: multipart/mixed; boundary="first"; boundary="second"', "", "--first", "Content-Type: text/plain", "", "under first", "--first--", "--second", "Content-Type: text/plain", "", "under second", "--second--"));
  add("same boundary at two levels", message('Content-Type: multipart/mixed; boundary="b"', "", "--b", 'Content-Type: multipart/alternative; boundary="b"', "", "--b", "Content-Type: text/plain", "", "inner", "--b--", "--b", "Content-Type: text/plain", "", "outer", "--b--"));
  add("boundary with spaces and specials", message("Content-Type: multipart/mixed; boundary=\"a b'()+_,-./:=?\"", "", "--a b'()+_,-./:=?", "Content-Type: text/plain", "", "x", "--a b'()+_,-./:=?--"));
  add("unquoted boundary", message("Content-Type: multipart/mixed; boundary=simple-boundary_1", "", "--simple-boundary_1", "", "implicit text", "--simple-boundary_1--"));
  add("boundary in rfc 2231 form", message("Content-Type: multipart/mixed; boundary*=utf-8''b%20x", "", "--b x", "Content-Type: text/plain", "", "x", "--b x--"));
  add("boundary with a non-latin character", message(u("Content-Type: multipart/mixed; boundary*=utf-8''%E3%81%82"), "", "--B", "Content-Type: text/plain", "", "low byte of the code unit", "--B--"));
  add("boundary seventy characters long", multipart("b".repeat(70), "mixed", ["Content-Type: text/plain", "", "x"]));
  add("boundary of one dash", multipart("-", "mixed", ["Content-Type: text/plain", "", "x", "---", "y"]));
  add("empty boundary", message('Content-Type: multipart/mixed; boundary=""', "", "--", "Content-Type: text/plain", "", "x", "----"));
  add("lines that only look like boundaries", multipart("m", "mixed", ["Content-Type: text/plain", "", "--mx", "--m x", "--m-", "--m -", " --m", "--M", "--m--x", "--m---", "-- m", "--m\t", "--", "---m"]));
  add("boundary followed by spaces", message('Content-Type: multipart/mixed; boundary="m"', "", "--m  ", "Content-Type: text/plain", "", "x", "--m-- "));
  add("boundary with line feed only and carriage return only", 'Content-Type: multipart/mixed; boundary="m"\r\n\r\n--m\nContent-Type: text/plain\n\nA\r--m\rB\n--m--\n');
  add("boundary after a stray carriage return", 'Content-Type: multipart/mixed; boundary="m"\r\n\r\n--m\r\nContent-Type: text/plain\r\n\r\nA\r\n\r--m\r\nContent-Type: text/plain\r\n\r\nB\r\n\rX--m--\r\n');
  add("end boundary as the last bytes", 'Content-Type: multipart/mixed; boundary="m"\r\n\r\n--m\r\nContent-Type: text/plain\r\n\r\nA\r\n--m--');
  add("boundary as the last bytes", 'Content-Type: multipart/mixed; boundary="m"\r\n\r\n--m\r\nContent-Type: text/plain\r\n\r\nA\r\n--m');
  add("boundary and one dash as the last bytes", 'Content-Type: multipart/mixed; boundary="m"\r\n\r\n--m\r\nContent-Type: text/plain\r\n\r\nA\r\n--m-');
  add("part body ends without a line break before the boundary", 'Content-Type: multipart/mixed; boundary="m"\r\n\r\n--m\r\nContent-Type: text/plain\r\n\r\nA--m\r\n--m--\r\n');
  add("blank lines before the boundary", multipart("m", "mixed", ["Content-Type: text/plain", "", "A", "", ""], ["Content-Type: text/plain", "", "", "B"]));
  add("part with no headers", multipart("m", "mixed", ["", "headerless"], ["", ""], ["Content-Type: text/html", "", "<i>x</i>"]));
  add("part with headers but no blank line", message('Content-Type: multipart/mixed; boundary="m"', "", "--m", "Content-Type: text/plain", "--m--"));
  add("text preamble and epilogue", message('Content-Type: multipart/mixed; boundary="m"', "", "This is a preamble.", "--not-the-boundary", "--m", "Content-Type: text/plain", "", "x", "--m--", "This is the epilogue.", "--m", "ignored?"));
  add("multipart digest and unknown subtypes", message('Content-Type: multipart/digest; boundary="d"', "", "--d", "", "Subject: digest one", "", "body one", "--d", "Content-Type: multipart/x-unknown; boundary=\"u\"", "", "--u", "Content-Type: text/plain", "", "in unknown", "--u--", "--d--"));
  add("multipart content type on a leaf with base64", message('Content-Type: multipart/mixed; boundary="m"', "Content-Transfer-Encoding: base64", "", "--m", "Content-Type: text/plain", "", "x", "--m--"));
  add("four levels of nesting", message('Content-Type: multipart/mixed; boundary="1"', "", "--1", 'Content-Type: multipart/mixed; boundary="2"', "", "--2", 'Content-Type: multipart/mixed; boundary="3"', "", "--3", 'Content-Type: multipart/alternative; boundary="4"', "", "--4", "Content-Type: text/plain", "", "deep plain", "--4", "Content-Type: text/html", "", "<p>deep html</p>", "--4--", "--3--", "--2", "Content-Type: text/plain", "", "level two", "--2--", "--1", "Content-Type: text/plain", "", "level one", "--1--"));
  add("alternative nested in alternative", multipart("a", "alternative", ["Content-Type: text/plain", "", "outer plain"], ['Content-Type: multipart/mixed; boundary="i"', "", "--i", "Content-Type: text/plain", "", "inner plain", "--i", "Content-Type: text/html", "", "<p>inner html</p>", "--i--"]));
  add("long line that cannot be a boundary", 'Content-Type: multipart/mixed; boundary="m"\r\n\r\n--m\r\nContent-Type: text/plain\r\n\r\n', ["z", 5000], "\r\n--m--\r\n");
  add("long line ending in a carriage return", 'Content-Type: multipart/mixed; boundary="m"\r\n\r\n--m\r\nContent-Type: text/plain\r\n\r\n', ["z", 5000], "\r", ["y", 5000], "\r\r\n--m--\r\n");

  // --- Embedded messages ---
  add("embedded message with every header", multipart("m", "mixed", ["Content-Type: text/html", "", "<p>outer</p>"], ["Content-Type: message/rfc822", "Content-Disposition: inline", "", "From: =?utf-8?q?Inn=C3=A9r?= <inner@x.org>", "To: a@x.org", "To: b@x.org, Team: c@x.org;", "Cc: <d@x.org>", "Bcc: e@x.org", "Subject: <Inner> & subject", "Date: Mon, 26 Oct 2026 09:00:00 +0000", "Content-Type: text/html", "", "<p>inner html</p>"]));
  add("embedded message with an unreadable date", multipart("m", "mixed", ["Content-Type: text/plain", "", "outer"], ["Content-Type: message/rfc822", "Content-Disposition: inline", "", "Subject: no date", "Date: whenever", "", "inner"]));
  add("embedded message with no headers", multipart("m", "mixed", ["Content-Type: text/plain", "", "outer"], ["Content-Type: message/rfc822", "Content-Disposition: inline", "", "", "bare inner body"]));
  add("embedded multipart message", multipart("m", "mixed", ["Content-Type: text/plain", "", "outer"], ["Content-Type: message/rfc822", "Content-Disposition: inline", "", "Subject: inner multipart", 'Content-Type: multipart/alternative; boundary="in"', "", "--in", "Content-Type: text/plain", "", "inner plain", "--in", "Content-Type: text/html", "", "<p>inner html</p>", "--in--"], ["Content-Type: text/plain", "", "after the embedded message"]));
  add("embedded message inside an embedded message", multipart("m", "mixed", ["Content-Type: message/rfc822", "Content-Disposition: inline", "", "Subject: level one", "Content-Type: message/rfc822", "Content-Disposition: inline", "", "Subject: level two", "", "innermost"]));
  add("embedded message in base64", multipart("m", "mixed", ["Content-Type: message/rfc822", "Content-Disposition: inline", "Content-Transfer-Encoding: base64", "", b64("Subject: encoded\r\n\r\nbody")]));
  add("embedded message as the root", message("Content-Type: message/rfc822", "Content-Disposition: inline", "Subject: outer", "", "Subject: inner", "From: in@x.org", "", "inner body"));
  add("embedded message ended by the boundary in its headers", multipart("m", "mixed", ["Content-Type: message/rfc822", "Content-Disposition: inline", "", "Subject: cut short"], ["Content-Type: text/plain", "", "next"]));
  add("embedded message with 8bit encoding and a comment", multipart("m", "mixed", ["Content-Type: message/rfc822", "Content-Disposition: INLINE", "Content-Transfer-Encoding: 8bit (as is)", "", "Subject: eight", "", "bits"]));
  add("message/rfc822 with attachment disposition", multipart("m", "mixed", ["Content-Type: message/rfc822", 'Content-Disposition: attachment; filename="fwd.eml"', "", "Subject: attached", "", "kept whole"]));

  // --- Attachments ---
  const attachment = (headers: string[], body = "data") => multipart("m", "mixed", ["Content-Type: text/plain", "", "see attached"], [...headers, "", body]);
  const filenames: [string, string[]][] = [
    ["plain", ['Content-Disposition: attachment; filename="report.pdf"']],
    ["unquoted", ["Content-Disposition: attachment; filename=report.final.PDF"]],
    ["name parameter only", ['Content-Type: application/octet-stream; name="from-name.png"']],
    ["both, disposition wins", ['Content-Type: application/octet-stream; name="type.png"', 'Content-Disposition: attachment; filename="disposition.gif"']],
    ["empty filename falls back to name", ['Content-Type: application/octet-stream; name="fallback.txt"', 'Content-Disposition: attachment; filename=""']],
    ["raw utf-8", [u('Content-Disposition: attachment; filename="한글 파일.txt"')]],
    ["raw latin-1", ['Content-Disposition: attachment; filename="caf\xe9.txt"']],
    ["encoded word", ['Content-Disposition: attachment; filename="=?UTF-8?B?7ZWc6riALmRvY3g=?="']],
    ["encoded words split", ['Content-Disposition: attachment; filename="=?UTF-8?Q?=ED=95?= =?UTF-8?Q?=9C.txt?="']],
    ["rfc 2231 single", ["Content-Disposition: attachment; filename*=UTF-8''%ED%95%9C%EA%B8%80.pdf"]],
    ["rfc 2231 latin-1 with language", ["Content-Disposition: attachment; filename*=iso-8859-1'fr'caf%E9%20cr%E8me.txt"]],
    ["rfc 2231 continuations out of order", ["Content-Disposition: attachment; filename*2=\".pdf\"; filename*0*=UTF-8''%EB%B3%B4; filename*1*=%EA%B3%A0"]],
    ["rfc 2231 beside a plain filename", ["Content-Disposition: attachment; filename=\"plain.txt\"; filename*=UTF-8''star.txt"]],
    ["rfc 2231 unknown charset", ["Content-Disposition: attachment; filename*=x-nope''%41%42.bin"]],
    ["path separators", ['Content-Disposition: attachment; filename="../../etc/passwd"']],
    ["windows path", ['Content-Disposition: attachment; filename="C:\\\\Users\\\\x\\\\doc.docx"']],
    ["dot file", ['Content-Disposition: attachment; filename=".bashrc"']],
    ["trailing dot", ['Content-Disposition: attachment; filename="name."']],
    ["only dots", ['Content-Disposition: attachment; filename="..."']],
    ["double extension", ['Content-Disposition: attachment; filename="archive.tar.gz"']],
    ["extension with spaces", ['Content-Disposition: attachment; filename="photo. J P G"']],
    ["extension then a slash", ['Content-Disposition: attachment; filename="photo.jpg/"']],
    ["extension then a path", ['Content-Disposition: attachment; filename="photo.jpg/more"']],
    ["no extension", ['Content-Disposition: attachment; filename="README"']],
    ["a name that is an extension", ['Content-Disposition: attachment; filename="pdf"']],
    ["empty", ['Content-Disposition: attachment; filename=""']],
    ["semicolon and quotes inside", ['Content-Disposition: attachment; filename="a;b\\"c.txt"']],
    ["very long", [`Content-Disposition: attachment; filename="${"n".repeat(300)}.txt"`]],
    ["nul in the extension", ['Content-Disposition: attachment; filename="a.b\x00c"']],
    ["nul in the name", ['Content-Disposition: attachment; filename="a\x00b.png"']],
    ["extension as long as a path", [`Content-Disposition: attachment; filename="a.${"x".repeat(4094)}"`]],
    ["extension one short of a path", [`Content-Disposition: attachment; filename="a.${"x".repeat(4093)}"`]],
    ["decodes to nothing", ['Content-Disposition: attachment; filename="=?utf-8?q??="']],
    ["emoji", [u('Content-Disposition: attachment; filename="😀.png"')]]
  ];
  for (const [name, headers] of filenames) {
    add(`filename: ${name}, octet-stream`, attachment(["Content-Type: application/octet-stream", ...headers]));
    add(`filename: ${name}, no content type`, attachment(headers));
  }
  add("no content type, attachment disposition", attachment(["Content-Disposition: attachment"]));
  add("no content type, inline disposition", attachment(["Content-Disposition: inline"]));
  add("no content type, no disposition", attachment(["X-Nothing: here"]));
  add("no content type, inline with a text filename", attachment(['Content-Disposition: inline; filename="notes.txt"']));
  add("no content type, inline with an html filename", attachment(['Content-Disposition: inline; filename="page.html"'], "<b>from the extension</b>"));
  const dispositions = ["attachment", "ATTACHMENT", "inline", "Inline ", "form-data", "=?utf-8?q?inline?=", "=?utf-8?q?attachment?=", "=?utf-8?q??=", "x", "\"attachment\"", "attachment (really)", ""];
  for (const disposition of dispositions) {
    add(`disposition ${JSON.stringify(disposition)} on text`, attachment(["Content-Type: text/plain", `Content-Disposition: ${disposition}`], "text body"));
    add(`disposition ${JSON.stringify(disposition)} on an image`, attachment(["Content-Type: image/png", `Content-Disposition: ${disposition}`], "png"));
  }
  add("inline disposition on html and on delivery status", multipart("m", "mixed", ["Content-Type: text/html", "Content-Disposition: inline", "", "<p>shown</p>"], ["Content-Type: message/delivery-status", "Content-Disposition: inline", "", "Status: 5.0.0"], ["Content-Type: text/plain", 'Content-Disposition: attachment; filename="t.txt"', "", "kept as a file"]));
  const contentIds = ["<a@b>", "a@b", " < spaced > ", "<>", "<<double>>", "<", ">", "<a@b> trailing", u("<한글@x>"), ""];
  for (const contentId of contentIds) add(`content id ${JSON.stringify(contentId)}`, attachment(["Content-Type: image/png", `Content-ID: ${contentId}`]));
  add("two content ids on one part", attachment(["Content-Type: image/png", "Content-ID: <first>", "Content-ID: <second>"]));
  add("attachment types", multipart("m", "mixed", ["Content-Type: image/svg+xml", "", "<svg/>"], ["Content-Type: TEXT/CSV; charset=utf-8", "", "a,b"], ["Content-Type: application/json", "", "{}"], ["Content-Type: text/calendar", "", "BEGIN:VCALENDAR"], ["Content-Type: text/x-vcard", "", "BEGIN:VCARD"], ["Content-Type: weird", "", "no slash"], ["Content-Type: a/b/c ; x", "", "two slashes"]));
  add("empty attachments", multipart("m", "mixed", ["Content-Type: application/pdf", ""], ["Content-Type: application/pdf", "Content-Transfer-Encoding: base64", ""], ["Content-Type: application/pdf", "Content-Transfer-Encoding: quoted-printable", "", ""]));
  add("attachment with binary bytes and line endings kept", multipart("m", "mixed", ["Content-Type: application/x-bin", "", "\x00\x01\r\n\n\r\xff\xfe--not-m\r\n\r\n"]));
  add("base64 attachment without padding and with breaks inside quanta", attachment(["Content-Type: application/x-bin", "Content-Transfer-Encoding: base64"], "QU\r\nJD\r\nR\r\nEVG\r\nRw"));
  add("base64 attachment of concatenated encodings", attachment(["Content-Type: application/x-bin", "Content-Transfer-Encoding: base64"], "QQ==QUI=QUJD\r\n=QUJD"));
  add("quoted-printable attachment", attachment(["Content-Type: application/x-bin", "Content-Transfer-Encoding: quoted-printable"], "=00=01=FF bin=\r\nary=0D=0A\r\ntail "));
  add("attachment three write batches long", 'Content-Type: multipart/mixed; boundary="m"\r\n\r\n--m\r\nContent-Type: application/x-bin\r\nContent-Transfer-Encoding: base64\r\n\r\n', ["QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVowMTIzNDU2Nzg5QUJDREVGR0hJSktMTU5PUFFSU1RVVldY\r\n", 14_000], "--m--\r\n");
  add("attachment as the root", message("Content-Type: application/pdf", 'Content-Disposition: attachment; filename="root.pdf"', "Content-Transfer-Encoding: base64", "", "JVBERi0="));
  add("image as the root with a cid", message("Content-Type: image/gif", "Content-ID: <root-image>", "", "GIF89a"));

  // --- cid references ---
  const related = (html: string, ...images: [string, string][]) => multipart("r", "related", ["Content-Type: text/html; charset=utf-8", "", html], ...images.map(([type, cid]) => [`Content-Type: ${type}`, `Content-ID: ${cid}`, "Content-Transfer-Encoding: base64", "", b64(PNG)]));
  add("cid in every position", related('<img src="cid:a"><img src=cid:a><img src=\'cid:a\'> text cid:a, xcid:a _cid:a -cid:a CID:a cid: a cid:', ["image/png", "<a>"]));
  add("cid of an image type the pattern refuses", related('<img src="cid:svg"><img src="cid:icon"><img src="cid:jpeg">', ["image/svg+xml", "<svg>"], ["image/x-icon", "<icon>"], ["IMAGE/JPEG", "<jpeg>"]));
  add("cid that is not an image", related('<img src="cid:doc">', ["application/pdf", "<doc>"]));
  add("two images with one cid", related('<img src="cid:same">', ["image/gif", "<same>"], ["image/png", "<same>"]));
  add("cid with specials", related(u('<img src="cid:logo@x.example"><img src="cid:한글"><img src="cid:a&amp;b"><img src="cid:a b">'), ["image/png", "<logo@x.example>"], ["image/png", u("<한글>")], ["image/png", "<a&amp;b>"], ["image/png", "<a>"]));
  add("cid longer than the pattern reads", related(`<img src="cid:${"c".repeat(300)}"><img src="cid:${"d".repeat(256)}">`, ["image/png", `<${"c".repeat(256)}>`], ["image/png", `<${"d".repeat(256)}>`]));
  add("cid cut inside a surrogate pair", related(u(`<img src="cid:${"c".repeat(255)}😀">`), ["image/png", `<${"c".repeat(255)}>`]));
  add("cid in a text part beside html", multipart("m", "mixed", ["Content-Type: text/plain", "", "see cid:a here"], ["Content-Type: text/html", "", "<p>x</p>"], ["Content-Type: image/png", "Content-ID: <a>", "Content-Transfer-Encoding: base64", "", b64(PNG)]));
  add("cid image with no encoding", multipart("r", "related", ["Content-Type: text/html", "", '<img src="cid:raw">'], ["Content-Type: image/png", "Content-ID: <raw>", "", "raw bytes \x00\xff"]));
  return cases;
};

const SUBJECTS = ["Hello", "Re: plan", "=?UTF-8?B?7ZWc6riA?=", "=?iso-8859-1?Q?caf=E9?= au lait", u("한글 제목"), "caf\xe9", "", "a".repeat(200), "=?utf-8?Q?broken", "Fwd: =?utf-8?q?a?= =?utf-8?q?b?="];
const ADDRESSES = ["a@example.com", "Bob <bob@example.com>", '"Doe, John" <john@example.org>', "Team: a@x.org, b@x.org;", "=?UTF-8?B?7ZmN6ri464+Z?= <hong@example.kr>", "x@xn--9n2bp8q.com", "broken <", "a@b, c@d", u("한글 <han@x.kr>"), ""];
const DATES = ["Tue, 27 Oct 2026 10:00:00 +0900", "27 Oct 2026 10:00 GMT", "2026-10-27", "Mon, 2 Jan 2006 15:04:05 -0700 (MST)", "garbage", "", "Tue, 27 Oct 2026 10:00:00 PST"];
const TEXTS = ["plain text", "line one\r\nline two", u("유니코드 본문 😀"), "caf\xe9", "", " ", "--not a boundary", "https://example.com/x?a=1&b=2 and a@b.example", "> quoted\r\n\r\nreply ", "a".repeat(120), "tab\there", "=3D literal", "trailing space \r\n next"];
const HTMLS = ["<p>hello</p>", "<h1>T</h1><ul><li>a<li>b</ul>", '<a href="https://x.org">link</a> <img src="cid:img" alt="i">', u("<p>유니코드 &amp; &#x1F600;</p>"), "<div>unclosed", "<script>x</script>visible", "", "<blockquote>q</blockquote>after", '<img src="cid:other">', "<table><tr><td>a<td>b</table>"];
const CHARSETS = ["utf-8", "UTF-8", "iso-8859-1", "us-ascii", "windows-1252", "euc-kr", "x-unknown", "utf-16", ""];
const ENCODINGS = ["", "7bit", "8bit", "binary", "base64", "quoted-printable", "BASE64", "x-other", "base64 (comment)"];
const BINARY_TYPES = ["application/pdf", "application/octet-stream", "image/png", "image/jpeg", "application/zip", "text/csv", "application/x-custom", "message/rfc822", "audio/mpeg", ""];
const FILENAMES = ["report.pdf", "photo.JPG", u("한글.docx"), "=?UTF-8?B?7ZWc6riALmRvY3g=?=", "noext", "a.tar.gz", "weird name .txt", "x.unknownext", ".hidden", "a.b.c."];

const qpEncode = (text: string, rng: Rng) => {
  let line = "";
  let out = "";
  for (const char of text) {
    const code = char.charCodeAt(0);
    const literal = code >= 0x21 && code <= 0x7e && char !== "=" && rng.chance(0.8);
    const next = literal ? char : char === " " && rng.chance(0.6) ? " " : char === "\r" || char === "\n" ? char : "=" + code.toString(16).toUpperCase().padStart(2, "0");
    if (line.length + next.length > 70 && char !== "\r" && char !== "\n") {
      out += line + "=\r\n";
      line = "";
    }
    line += next;
  }
  return out + line;
};

const encodeBody = (body: string, encoding: string, rng: Rng) => {
  const bare = encoding.replace(/\(.*\)/, "").trim().toLowerCase();
  if (bare === "base64") return b64Lines(body, rng.pick([76, 60, 4, 1000]));
  if (bare === "quoted-printable") return qpEncode(body, rng);
  return body;
};

/** A random MIME tree, written out. */
const randomMessage = (rng: Rng): string => {
  let counter = 0;
  const eol = rng.pick(["\r\n", "\r\n", "\r\n", "\n"]);
  const lines = (...parts: string[]) => parts.join(eol) + eol;
  const leafHeaders = (type: string, extra: string[]) => {
    const encoding = rng.pick(ENCODINGS);
    const headers = [...extra];
    if (type || rng.chance(0.9)) headers.unshift(`Content-Type: ${type}`);
    if (encoding || rng.chance(0.2)) headers.push(`Content-Transfer-Encoding: ${encoding}`);
    if (rng.chance(0.3)) {
      // Shuffled by hand: a sort with a random comparator is not the same from run to run.
      for (let i = headers.length - 1; i > 0; i--) {
        const j = rng.int(i + 1);
        [headers[i], headers[j]] = [headers[j], headers[i]];
      }
    }
    return { headers, encoding };
  };
  const textLeaf = (): string => {
    const html = rng.chance(0.4);
    const charset = rng.pick(CHARSETS);
    const type = (html ? "text/html" : rng.pick(["text/plain", "text/plain", "TEXT/PLAIN", "message/delivery-status"])) + (charset || rng.chance(0.3) ? `; charset=${rng.pick(["", '"'])}${charset}`.replace(/"(.*)$/, '"$1"') : "") + (rng.chance(0.15) ? `; format=flowed${rng.chance(0.5) ? "; delsp=yes" : ""}` : "");
    const extra = rng.chance(0.2) ? [`Content-Disposition: ${rng.pick(["inline", "attachment", `attachment; filename="${rng.pick(FILENAMES)}"`])}`] : [];
    const { headers, encoding } = leafHeaders(type, extra);
    return lines(...headers, "", encodeBody(rng.pick(html ? HTMLS : TEXTS), encoding, rng));
  };
  const binaryLeaf = (): string => {
    const type = rng.pick(BINARY_TYPES);
    const extra: string[] = [];
    const filename = rng.pick(FILENAMES);
    if (rng.chance(0.7)) extra.push(rng.pick([`Content-Disposition: attachment; filename="${filename}"`, "Content-Disposition: attachment", `Content-Disposition: inline; filename="${filename}"`, `Content-Disposition: attachment; filename*=UTF-8''${encodeURIComponent(Buffer.from(filename, "latin1").toString("utf8"))}`]));
    if (rng.chance(0.4)) extra.push(`Content-ID: ${rng.pick(["<img>", "<other>", "img", "<x@y>"])}`);
    const typeLine = type && rng.chance(0.3) ? `${type}; name="${filename}"` : type;
    const { headers, encoding } = leafHeaders(typeLine, extra);
    const body = rng.pick([() => pseudoRandom(rng.int(600)), () => "small", () => "", () => PNG, () => "text\r\nlines\r\n"])();
    return lines(...headers, "", encodeBody(body, encoding, rng));
  };
  const embedded = (depth: number): string => {
    const headers = ["Content-Type: message/rfc822"];
    if (rng.chance(0.8)) headers.push("Content-Disposition: inline");
    if (rng.chance(0.2)) headers.push(`Content-Transfer-Encoding: ${rng.pick(["7bit", "8bit", "binary", "base64"])}`);
    return lines(...headers, "") + envelope() + node(depth - 1);
  };
  const envelope = (): string => {
    const headers: string[] = [];
    if (rng.chance(0.7)) headers.push(`From: ${rng.pick(ADDRESSES)}`);
    for (let i = rng.int(3); i > 0; i--) headers.push(`To: ${rng.pick(ADDRESSES)}`);
    if (rng.chance(0.3)) headers.push(`Cc: ${rng.pick(ADDRESSES)}`);
    if (rng.chance(0.2)) headers.push(`Bcc: ${rng.pick(ADDRESSES)}`);
    if (rng.chance(0.2)) headers.push(`Reply-To: ${rng.pick(ADDRESSES)}`);
    if (rng.chance(0.8)) headers.push(`Subject: ${rng.pick(SUBJECTS)}`);
    if (rng.chance(0.6)) headers.push(`Date: ${rng.pick(DATES)}`);
    if (rng.chance(0.6)) headers.push(`Message-ID: ${rng.pick(["<id@x>", "bare@x", "<a b>", ""])}`);
    if (rng.chance(0.3)) headers.push(`In-Reply-To: ${rng.pick(["<p@x>", "<p@x> <q@x>", "none"])}`);
    if (rng.chance(0.3)) headers.push(`References: ${rng.pick(["<r1@x> <r2@x>", `<r1@x>${eol} <r2@x>`, "junk"])}`);
    if (rng.chance(0.2)) headers.push("MIME-Version: 1.0", `X-Long: ${"v".repeat(rng.int(300))}`);
    return headers.length ? headers.join(eol) + eol : "";
  };
  const container = (depth: number): string => {
    const boundary = rng.pick([`b${counter++}`, `b${counter++}`, `=_Part_${counter++}_=`, `x y ${counter++}`, "same"]);
    const subtype = rng.pick(["mixed", "mixed", "alternative", "alternative", "related", "report", "digest"]);
    const parameter = rng.pick([`boundary="${boundary}"`, `boundary="${boundary}"`, /^[\w-]+$/.test(boundary) ? `boundary=${boundary}` : `boundary="${boundary}"`]);
    let out = lines(`Content-Type: multipart/${subtype}; ${parameter}${rng.chance(0.2) ? "; charset=utf-8" : ""}`, "");
    if (rng.chance(0.3)) out += lines(rng.pick(["This is a multi-part message.", "", "--not-it"]));
    const children = 1 + rng.int(4);
    for (let i = 0; i < children; i++) {
      out += `--${boundary}${rng.chance(0.05) ? " " : ""}${eol}` + node(depth - 1);
      if (rng.chance(0.85) && !out.endsWith(eol)) out += eol;
    }
    if (rng.chance(0.9)) out += `--${boundary}--${rng.chance(0.8) ? eol : ""}`;
    if (rng.chance(0.2)) out += lines("epilogue");
    return out;
  };
  const node = (depth: number): string => {
    if (depth <= 0) return rng.chance(0.6) ? textLeaf() : binaryLeaf();
    return rng.pick([textLeaf, textLeaf, binaryLeaf, () => container(depth), () => container(depth), () => embedded(depth)])();
  };
  return envelope() + node(rng.pick([0, 1, 2, 2, 3, 4]));
};

/** A message with a few bytes removed, repeated, swapped or added. */
const damage = (raw: string, rng: Rng): string => {
  let out = raw;
  for (let i = 1 + rng.int(3); i > 0 && out.length; i--) {
    const at = rng.int(out.length);
    const length = 1 + rng.int(rng.pick([1, 4, 40]));
    out = rng.pick([
      () => out.slice(0, at) + out.slice(at + length),
      () => out.slice(0, at) + out.slice(at, at + length) + out.slice(at),
      () => out.slice(0, at) + rng.pick(["\r\n", "\n", "\r", "--", "\r\n\r\n", ":", ";", "\"", "=", " ", "\x00", "\xff", "(", ")", "<", ">", "?=", "=?"]) + out.slice(at),
      () => out.slice(0, at),
      () => out.slice(0, at) + String.fromCharCode(rng.int(256)) + out.slice(at + 1)
    ])();
  }
  return out;
};

export const parserCases = (rng: Rng): ParserCase[] => {
  const cases = handWritten();
  for (let i = 0; i < 320; i++) cases.push({ name: `random ${i}`, raw: [randomMessage(rng)] });
  for (let i = 0; i < 260; i++) cases.push({ name: `damaged ${i}`, raw: [damage(randomMessage(rng), rng)] });
  const names = new Set<string>();
  for (const entry of cases) {
    if (names.has(entry.name)) throw new Error(`parser corpus: two cases named ${entry.name}`);
    names.add(entry.name);
  }
  return cases;
};
