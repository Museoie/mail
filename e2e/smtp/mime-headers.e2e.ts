import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { ADMIN_PASSWORD, type TestServer } from "../harness";
import { FIXED_DATE, Mailer, boot, crlf, messageId, octets, uniqueName, type Row } from "./support";

// How the headers of a delivered message end up in the mails row: subject,
// address lists, date, Message-ID and the threading headers.
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

const BASE = ["From: sender@remote.example", `Date: ${FIXED_DATE}`];

/** Stores a message made of the given header lines and a one-line body. */
const withHeaders = (...headers: (string | Uint8Array)[]): Promise<Row> =>
  mailer.store(octets(...headers.flatMap((header) => [header, "\r\n"]), "\r\nbody\r\n"));

const subjectOf = async (...subjectLines: (string | Uint8Array)[]) =>
  (await withHeaders(...BASE, ...subjectLines)).subject as string;

const b = (text: string | number[], charset = "utf-8") =>
  `=?${charset}?B?${Buffer.from(text as never).toString("base64")}?=`;

describe("Subject", () => {
  test("plain ASCII is stored as written, inner spacing included", async () => {
    expect(await subjectOf("Subject: Hello  there,   world")).toBe("Hello  there,   world");
    expect(await subjectOf("Subject:no space after the colon")).toBe("no space after the colon");
    expect(await subjectOf("Subject:    padded   ")).toBe("padded");
    expect(await subjectOf("Subject: Re: Fwd: [list] a: b")).toBe("Re: Fwd: [list] a: b");
  });

  test("RFC 2047 B and Q words in UTF-8", async () => {
    expect(await subjectOf(`Subject: ${b("✓ check 日本語")}`)).toBe("✓ check 日本語");
    expect(await subjectOf("Subject: =?UTF-8?Q?Caf=C3=A9_au_lait?=")).toBe("Café au lait");
    expect(await subjectOf("Subject: =?utf-8?q?lower=20case_markers?=")).toBe("lower case markers");
    expect(await subjectOf(`Subject: =?utf-8?b?${Buffer.from("lower b").toString("base64")}?=`)).toBe("lower b");
  });

  test("other charsets", async () => {
    expect(await subjectOf("Subject: =?ISO-8859-1?Q?Caf=E9_cr=E8me?=")).toBe("Café crème");
    expect(await subjectOf("Subject: =?iso-8859-15?Q?10_=A4?=")).toBe("10 €");
    expect(await subjectOf("Subject: =?windows-1252?Q?=93quoted=94_=96_=80?=")).toBe("“quoted” – €");
    expect(await subjectOf(`Subject: ${b([0x82, 0xb1, 0x82, 0xf1, 0x82, 0xc9, 0x82, 0xbf, 0x82, 0xcd], "Shift_JIS")}`)).toBe("こんにちは");
    expect(await subjectOf(`Subject: ${b([0x1b, 0x24, 0x42, 0x46, 0x7c, 0x4b, 0x5c, 0x1b, 0x28, 0x42], "ISO-2022-JP")}`)).toBe("日本");
    expect(await subjectOf(`Subject: ${b([0xc4, 0xe3, 0xba, 0xc3], "GB2312")}`)).toBe("你好");
    expect(await subjectOf(`Subject: ${b([0xa7, 0x41, 0xa6, 0x6e], "Big5")}`)).toBe("你好");
    expect(await subjectOf("Subject: =?KOI8-R?Q?=F0=D2=C9=D7=C5=D4?=")).toBe("Привет");
    expect(await subjectOf(`Subject: ${b([0xbe, 0xc8, 0xb3, 0xe7], "EUC-KR")}`)).toBe("안녕");
    expect(await subjectOf(`Subject: ${b([0xbe, 0xc8, 0xb3, 0xe7], "ks_c_5601-1987")}`)).toBe("안녕");
  });

  test("charset labels are matched loosely, and us-ascii reads as windows-1252", async () => {
    expect(await subjectOf("Subject: =?UTF8?Q?caf=C3=A9?=")).toBe("café");
    expect(await subjectOf("Subject: =?latin1?Q?caf=E9?=")).toBe("café");
    expect(await subjectOf("Subject: =?cp1252?Q?=80?=")).toBe("€");
    expect(await subjectOf("Subject: =?us-ascii?Q?caf=E9_=80?=")).toBe("café €");
    // A language tag after the charset (RFC 2231) is ignored.
    expect(await subjectOf("Subject: =?utf-8*fr?Q?caf=C3=A9?=")).toBe("café");
  });

  test("an unknown charset is read as UTF-8", async () => {
    expect(await subjectOf("Subject: =?x-made-up?Q?caf=C3=A9?=")).toBe("café");
    expect(await subjectOf("Subject: =?x-made-up?Q?caf=E9?=")).toBe("caf�");
  });

  test("adjacent encoded words are joined without the space between them", async () => {
    expect(await subjectOf(`Subject: ${b("one ")} ${b("two")}`)).toBe("one two");
    expect(await subjectOf("Subject: =?utf-8?Q?a?=   =?utf-8?Q?b?=\t=?utf-8?Q?c?=")).toBe("abc");
    // Different charsets, and B next to Q, join the same way.
    expect(await subjectOf("Subject: =?iso-8859-1?Q?caf=E9?= =?utf-8?B?4pyT?=")).toBe("café✓");
    // Text that is not an encoded word keeps its spaces.
    expect(await subjectOf("Subject: Re: =?utf-8?Q?caf=C3=A9?= and =?utf-8?Q?th=C3=A9?= now")).toBe("Re: café and thé now");
  });

  test("a character split across two words of one charset comes out whole", async () => {
    // "✓" is E2 9C 93: two octets in the first word, one in the second.
    expect(await subjectOf("Subject: =?utf-8?B?4pw=?= =?utf-8?B?kw==?=")).toBe("✓");
    expect(await subjectOf("Subject: =?utf-8?Q?=E2=9C?= =?utf-8?Q?=93?=")).toBe("✓");
  });

  test("words that are not well formed are left as written", async () => {
    expect(await subjectOf("Subject: =?utf-8?X?abc?=")).toBe("=?utf-8?X?abc?=");
    expect(await subjectOf("Subject: =?utf-8?Q?unterminated")).toBe("=?utf-8?Q?unterminated");
    expect(await subjectOf("Subject: =?utf-8?Q??=")).toBe("");
    expect(await subjectOf("Subject: 1 =? 2 ?= 3")).toBe("1 =? 2 ?= 3");
  });

  test("Q decoding: underscores are spaces and a bad escape is kept", async () => {
    expect(await subjectOf("Subject: =?utf-8?Q?a_b=5Fc=3D=3F?=")).toBe("a b_c=?");
    expect(await subjectOf("Subject: =?utf-8?Q?bad=ZZescape=4?=")).toBe("bad=ZZescape=4");
  });

  test("raw 8-bit octets are read as UTF-8", async () => {
    expect(await subjectOf(octets("Subject: ", Buffer.from("Grüße ✓ 日本")))).toBe("Grüße ✓ 日本");
    // Latin-1 octets are not valid UTF-8 and become replacement characters.
    expect(await subjectOf(octets("Subject: caf\xe9 cr\xe8me"))).toBe("caf� cr�me");
  });

  test("characters outside the basic plane survive, encoded or raw", async () => {
    expect(await subjectOf(`Subject: ${b("😀 ok 𝒳")}`)).toBe("😀 ok 𝒳");
    expect(await subjectOf(octets("Subject: ", Buffer.from("family 👨‍👩‍👧 end")))).toBe("family 👨‍👩‍👧 end");
    const mail = await mailer.store(
      octets("From: sender@remote.example\r\nSubject: emoji\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n", Buffer.from("body 😀 𝒳 👨‍👩‍👧 end\r\n"))
    );
    expect(mail.text).toBe("body 😀 𝒳 👨‍👩‍👧 end\n");
  });

  test("a folded subject is unfolded with one space per fold", async () => {
    expect(await subjectOf("Subject: first", " second", "\tthird", "      fourth")).toBe("first second third fourth");
    expect(await subjectOf("Subject:", " starts on the next line")).toBe("starts on the next line");
    expect(await subjectOf(`Subject: ${b("encoded ")}`, ` ${b("across a fold")}`)).toBe("encoded across a fold");
  });

  test("a very long subject is stored whole", async () => {
    const long = Array.from({ length: 500 }, (_, i) => `word${i}`).join(" ");
    expect(long.length).toBeGreaterThan(3000);
    expect(await subjectOf(`Subject: ${long}`)).toBe(long);
    const folded = long.replace(/ (?=word\d*0 )/g, "\r\n ");
    expect(await subjectOf(`Subject: ${folded}`)).toBe(long);
  });

  test("an empty subject is the empty string", async () => {
    expect(await subjectOf("Subject:")).toBe("");
    expect(await subjectOf("Subject:    ")).toBe("");
  });

  test("of two Subject headers the last one is stored", async () => {
    expect(await subjectOf("Subject: first", "Subject: second")).toBe("second");
    // An empty one does not replace an earlier value.
    expect(await subjectOf("Subject: first", "Subject:")).toBe("first");
  });

  test("header names are matched in any letter case, with space before the colon", async () => {
    expect(await subjectOf("SUBJECT: upper")).toBe("upper");
    expect(await subjectOf("sUbJeCt: mixed")).toBe("mixed");
    expect(await subjectOf("Subject : spaced")).toBe("spaced");
  });

  test("an mbox From_ line at the top is dropped", async () => {
    const mail = await mailer.store(crlf("From sender@remote.example Tue Mar  3 10:20:30 2026", ...BASE, "Subject: after mbox line", "", "body"));
    expect(mail.subject).toBe("after mbox line");
    expect(mail.from_address).toEqual([{ name: "", address: "sender@remote.example" }]);
  });
});

const addresses = async (header: string, ...more: string[]) => {
  const mail = await withHeaders(`Date: ${FIXED_DATE}`, "Subject: addresses", header, ...more);
  return {
    from: [mail.from_address, mail.from_text],
    to: [mail.to_address, mail.to_text],
    cc: [mail.cc_address, mail.cc_text],
    bcc: [mail.bcc_address, mail.bcc_text],
    replyTo: [mail.reply_to_address, mail.reply_to_text],
  };
};
const fromOf = async (value: string) => (await addresses(`From: ${value}`)).from;
const toOf = async (...headers: string[]) => (await addresses(headers[0]!, ...headers.slice(1))).to;

describe("address headers", () => {
  test("the common forms of one mailbox", async () => {
    expect(await fromOf("john@remote.example")).toEqual([[{ name: "", address: "john@remote.example" }], "john@remote.example"]);
    expect(await fromOf("<john@remote.example>")).toEqual([[{ name: "", address: "john@remote.example" }], "john@remote.example"]);
    expect(await fromOf("John Doe <john@remote.example>")).toEqual([
      [{ name: "John Doe", address: "john@remote.example" }],
      '"John Doe" <john@remote.example>',
    ]);
    expect(await fromOf('"John Doe" <john@remote.example>')).toEqual([
      [{ name: "John Doe", address: "john@remote.example" }],
      '"John Doe" <john@remote.example>',
    ]);
    expect(await fromOf("John   Doe<john@remote.example>")).toEqual([
      [{ name: "John   Doe", address: "john@remote.example" }],
      '"John   Doe" <john@remote.example>',
    ]);
  });

  test("the address is lower-cased in the list, and kept as written in the text", async () => {
    expect(await fromOf("John <John.Doe@Remote.EXAMPLE>")).toEqual([
      [{ name: "John", address: "john.doe@remote.example" }],
      '"John" <John.Doe@Remote.EXAMPLE>',
    ]);
  });

  test("quoted names keep commas, and escaped quotes are unescaped", async () => {
    expect(await fromOf('"Doe, John" <john@remote.example>')).toEqual([
      [{ name: "Doe, John", address: "john@remote.example" }],
      '"Doe, John" <john@remote.example>',
    ]);
    expect(await fromOf('"John \\"JD\\" Doe" <john@remote.example>')).toEqual([
      [{ name: 'John "JD" Doe', address: "john@remote.example" }],
      '"John "JD" Doe" <john@remote.example>',
    ]);
    expect(await fromOf('"semi; colon: here" <john@remote.example>')).toEqual([
      [{ name: "semi; colon: here", address: "john@remote.example" }],
      '"semi; colon: here" <john@remote.example>',
    ]);
  });

  test("a comment becomes the name when there is no other", async () => {
    expect(await fromOf("john@remote.example (John Doe)")).toEqual([
      [{ name: "John Doe", address: "john@remote.example" }],
      '"John Doe" <john@remote.example>',
    ]);
    expect(await fromOf("John <john@remote.example> (work)")).toEqual([
      [{ name: "John", address: "john@remote.example" }],
      '"John" <john@remote.example>',
    ]);
  });

  test("encoded-word names are decoded, quoted or not", async () => {
    expect(await fromOf(`${b("Jörg Müller")} <joerg@remote.example>`)).toEqual([
      [{ name: "Jörg Müller", address: "joerg@remote.example" }],
      '"Jörg Müller" <joerg@remote.example>',
    ]);
    expect(await fromOf('"=?iso-8859-1?Q?J=F6rg_M=FCller?=" <joerg@remote.example>')).toEqual([
      [{ name: "Jörg Müller", address: "joerg@remote.example" }],
      '"Jörg Müller" <joerg@remote.example>',
    ]);
    expect(await fromOf(`${b([0x93, 0x63, 0x92, 0x86], "Shift_JIS")} <tanaka@remote.example>`)).toEqual([
      [{ name: "田中", address: "tanaka@remote.example" }],
      '"田中" <tanaka@remote.example>',
    ]);
    expect(await fromOf("=?utf-8?Q?J=C3=B6rg?= =?utf-8?Q?_M=C3=BCller?= <joerg@remote.example>")).toEqual([
      [{ name: "Jörg Müller", address: "joerg@remote.example" }],
      '"Jörg Müller" <joerg@remote.example>',
    ]);
  });

  test("an IDN domain in punycode is shown in Unicode", async () => {
    expect(await fromOf("a@xn--bcher-kva.example")).toEqual([[{ name: "", address: "a@bücher.example" }], "a@bücher.example"]);
  });

  test("a list of mailboxes, separated by commas or semicolons", async () => {
    expect(await toOf("To: a@remote.example, B <b@remote.example>,c@remote.example")).toEqual([
      [
        { name: "", address: "a@remote.example" },
        { name: "B", address: "b@remote.example" },
        { name: "", address: "c@remote.example" },
      ],
      'a@remote.example, "B" <b@remote.example>, c@remote.example',
    ]);
    expect(await toOf("To: a@remote.example; b@remote.example")).toEqual([
      [
        { name: "", address: "a@remote.example" },
        { name: "", address: "b@remote.example" },
      ],
      "a@remote.example, b@remote.example",
    ]);
    expect(await toOf("To: a@remote.example,, ,b@remote.example,")).toEqual([
      [
        { name: "", address: "a@remote.example" },
        { name: "", address: "b@remote.example" },
      ],
      "a@remote.example, b@remote.example",
    ]);
  });

  test("a folded list", async () => {
    expect(await toOf("To: a@remote.example,", "  B <b@remote.example>,", "\tc@remote.example")).toEqual([
      [
        { name: "", address: "a@remote.example" },
        { name: "B", address: "b@remote.example" },
        { name: "", address: "c@remote.example" },
      ],
      'a@remote.example, "B" <b@remote.example>, c@remote.example',
    ]);
  });

  test("an RFC 5322 group is flattened in the list and kept in the text", async () => {
    expect(await toOf("To: Team: a@remote.example, B <b@remote.example>;, c@remote.example")).toEqual([
      [
        { name: "", address: "a@remote.example" },
        { name: "B", address: "b@remote.example" },
        { name: "", address: "c@remote.example" },
      ],
      '"Team": a@remote.example, "B" <b@remote.example>;, c@remote.example',
    ]);
  });

  test("an empty group leaves the columns null", async () => {
    expect(await toOf("To: undisclosed-recipients:;")).toEqual([null, null]);
  });

  test("text with no address in it leaves the columns null", async () => {
    expect(await toOf("To: nobody in particular")).toEqual([null, null]);
    expect(await toOf("To:")).toEqual([null, null]);
    expect(await toOf("To: postmaster")).toEqual([null, null]);
  });

  test("an address written without angle brackets beside a name is still found", async () => {
    expect(await fromOf("John Doe john@remote.example")).toEqual([
      [{ name: "John Doe", address: "john@remote.example" }],
      '"John Doe" <john@remote.example>',
    ]);
  });

  test("a quoted address used as a name stays the name when a real address follows", async () => {
    expect(await fromOf('"ceo@bank.example" <john@remote.example>')).toEqual([
      [{ name: "ceo@bank.example", address: "john@remote.example" }],
      '"ceo@bank.example" <john@remote.example>',
    ]);
    // With nothing else beside it, the quoted text is the address.
    expect(await fromOf('"ceo@bank.example"')).toEqual([[{ name: "", address: "ceo@bank.example" }], "ceo@bank.example"]);
    // A quoted local part is not split at the @ inside the quotes.
    expect(await fromOf('"user@evil.example"@remote.example')).toEqual([
      [{ name: "", address: "user@evil.example@remote.example" }],
      "user@evil.example@remote.example",
    ]);
  });

  test("repeated To, Cc and Bcc headers are joined; of repeated From and Reply-To the last is kept", async () => {
    expect(
      await addresses(
        "From: first@remote.example",
        "From: Second <second@remote.example>",
        "To: a@remote.example",
        "To: B <b@remote.example>, c@remote.example",
        "Cc: d@remote.example",
        "CC: e@remote.example",
        "Bcc: f@remote.example",
        "bcc: g@remote.example",
        "Reply-To: r1@remote.example",
        "Reply-To: r2@remote.example"
      )
    ).toEqual({
      from: [[{ name: "Second", address: "second@remote.example" }], '"Second" <second@remote.example>'],
      to: [
        [
          { name: "", address: "a@remote.example" },
          { name: "B", address: "b@remote.example" },
          { name: "", address: "c@remote.example" },
        ],
        'a@remote.example, "B" <b@remote.example>, c@remote.example',
      ],
      cc: [
        [
          { name: "", address: "d@remote.example" },
          { name: "", address: "e@remote.example" },
        ],
        "d@remote.example, e@remote.example",
      ],
      bcc: [
        [
          { name: "", address: "f@remote.example" },
          { name: "", address: "g@remote.example" },
        ],
        "f@remote.example, g@remote.example",
      ],
      replyTo: [[{ name: "", address: "r2@remote.example" }], "r2@remote.example"],
    });
  });

  test("a From with several mailboxes keeps them all", async () => {
    expect(await fromOf("a@remote.example, B <b@remote.example>")).toEqual([
      [
        { name: "", address: "a@remote.example" },
        { name: "B", address: "b@remote.example" },
      ],
      'a@remote.example, "B" <b@remote.example>',
    ]);
  });

  test("a whole address list hidden in B-encoded words is decoded and parsed again", async () => {
    expect(await toOf(`To: ${b("Ann <ann@remote.example>, bob@remote.example")}`)).toEqual([
      [
        { name: "Ann", address: "ann@remote.example" },
        { name: "", address: "bob@remote.example" },
      ],
      '"Ann" <ann@remote.example>, bob@remote.example',
    ]);
  });

  test("raw UTF-8 names", async () => {
    const mail = await withHeaders(`Date: ${FIXED_DATE}`, "Subject: s", octets("From: ", Buffer.from("Jörg Müller"), " <joerg@remote.example>"));
    expect([mail.from_address, mail.from_text]).toEqual([
      [{ name: "Jörg Müller", address: "joerg@remote.example" }],
      '"Jörg Müller" <joerg@remote.example>',
    ]);
  });
});

const dateOf = async (...dateLines: string[]) => {
  const mail = await withHeaders("From: sender@remote.example", "Subject: date", ...dateLines);
  return (mail.date as Date).toISOString();
};

describe("Date", () => {
  test("RFC 5322 dates, with numeric and named zones", async () => {
    expect(await dateOf("Date: Tue, 03 Mar 2026 10:20:30 +0000")).toBe("2026-03-03T10:20:30.000Z");
    expect(await dateOf("Date: Tue, 3 Mar 2026 10:20:30 -0500")).toBe("2026-03-03T15:20:30.000Z");
    expect(await dateOf("Date: 3 Mar 2026 10:20:30 +0930")).toBe("2026-03-03T00:50:30.000Z");
    expect(await dateOf("Date: Tue, 03 Mar 2026 10:20:30 GMT")).toBe("2026-03-03T10:20:30.000Z");
    expect(await dateOf("Date: Tue, 03 Mar 2026 10:20:30 UT")).toBe("2026-03-03T10:20:30.000Z");
    expect(await dateOf("Date: Tue, 03 Mar 2026 10:20:30 EST")).toBe("2026-03-03T15:20:30.000Z");
    expect(await dateOf("Date: Tue, 03 Mar 2026 10:20:30 PDT")).toBe("2026-03-03T17:20:30.000Z");
    expect(await dateOf("Date: Tue, 03 Mar 2026 10:20 +0000")).toBe("2026-03-03T10:20:00.000Z");
  });

  test("a trailing comment and a fold are tolerated", async () => {
    expect(await dateOf("Date: Tue, 03 Mar 2026 10:20:30 +0900 (JST)")).toBe("2026-03-03T01:20:30.000Z");
    expect(await dateOf("Date: Tue, 03 Mar 2026", " 10:20:30 +0000")).toBe("2026-03-03T10:20:30.000Z");
  });

  test("ISO 8601 is accepted too", async () => {
    expect(await dateOf("Date: 2026-03-03T10:20:30Z")).toBe("2026-03-03T10:20:30.000Z");
    expect(await dateOf("Date: 2026-03-03T10:20:30.123+02:00")).toBe("2026-03-03T08:20:30.123Z");
  });

  test("a date far in the past or the future is stored as given", async () => {
    expect(await dateOf("Date: Thu, 01 Jan 1970 00:00:00 +0000")).toBe("1970-01-01T00:00:00.000Z");
    expect(await dateOf("Date: Fri, 31 Dec 2100 23:59:59 +0000")).toBe("2100-12-31T23:59:59.000Z");
  });

  test("an unreadable date is replaced by the time of delivery", async () => {
    for (const line of ["Date: not a date", "Date: 32 Foo 2026 99:99:99 +0000", "Date:"]) {
      const before = Date.now();
      const stored = new Date(await dateOf(line)).getTime();
      expect(stored).toBeGreaterThanOrEqual(before - 1000);
      expect(stored).toBeLessThanOrEqual(Date.now() + 1000);
    }
  });

  test("of two Date headers the last one is stored", async () => {
    expect(await dateOf("Date: Tue, 03 Mar 2026 10:20:30 +0000", "Date: Wed, 04 Mar 2026 11:00:00 +0000")).toBe(
      "2026-03-04T11:00:00.000Z"
    );
  });
});

const idsOf = async (...headers: string[]) => {
  const mail = await withHeaders(...BASE, "Subject: ids", ...headers);
  return { id: mail.message_id as string, inReplyTo: mail.in_reply_to, references: mail.reference_ids };
};

describe("Message-ID, In-Reply-To and References", () => {
  test("a Message-ID is stored with its angle brackets, which are added when missing", async () => {
    expect((await idsOf("Message-ID: <abc.123@remote.example>")).id).toBe("<abc.123@remote.example>");
    expect((await idsOf("Message-Id:   <spaced@remote.example>   ")).id).toBe("<spaced@remote.example>");
    expect((await idsOf("Message-ID: bare@remote.example")).id).toBe("<bare@remote.example>");
    expect((await idsOf("Message-ID: <half@remote.example")).id).toBe("<half@remote.example>");
    expect((await idsOf("MESSAGE-ID: <upper@remote.example>")).id).toBe("<upper@remote.example>");
  });

  test("anything after the id is kept inside the brackets", async () => {
    // NOTE: a trailing comment is not stripped; the closing bracket is appended after it.
    expect((await idsOf("Message-ID: <abc@remote.example> (generated)")).id).toBe("<abc@remote.example> (generated)>");
  });

  test("an empty Message-ID is replaced by eight random hex digits", async () => {
    expect((await idsOf("Message-ID:")).id).toMatch(/^[0-9a-f]{8}$/);
  });

  test("of two Message-ID headers the last one is stored", async () => {
    expect((await idsOf("Message-ID: <one-of-two@remote.example>", "Message-ID: <two-of-two@remote.example>")).id).toBe(
      "<two-of-two@remote.example>"
    );
  });

  test("a Message-ID of 512 characters is stored; a longer one is dropped, as it is from the threading headers", async () => {
    const fits = `<${"x".repeat(495)}@remote.example>`;
    expect(fits.length).toBe(512);
    expect((await idsOf(`Message-ID: ${fits}`)).id).toBe(fits);
    // The column holds 512: one more and the mail is stored as one without a
    // Message-ID. The length is counted with the brackets the id is stored in.
    expect((await idsOf(`Message-ID: <${"x".repeat(496)}@remote.example>`)).id).toMatch(/^[0-9a-f]{8}$/);
    expect((await idsOf(`Message-ID: ${"x".repeat(496)}@remote.example`)).id).toMatch(/^[0-9a-f]{8}$/);
    // Dropped like a header that is not there, so an earlier one stands.
    expect((await idsOf("Message-ID: <kept@remote.example>", `Message-ID: <${"x".repeat(496)}@remote.example>`)).id).toBe(
      "<kept@remote.example>"
    );
  });

  test("a mail whose Message-ID was dropped still threads by its own headers, and cannot be reached by that id", async () => {
    const long = `<${"y".repeat(496)}@remote.example>`;
    const subject = `Plan ${uniqueName("thread")}`;
    const parentId = messageId("parent");
    const parent = await withHeaders("From: sender@remote.example", "Date: Tue, 03 Mar 2026 10:00:00 +0000", `Subject: ${subject}`, `Message-ID: ${parentId}`);
    const dropped = await withHeaders(
      "From: sender@remote.example",
      "Date: Tue, 03 Mar 2026 11:00:00 +0000",
      `Subject: Re: ${subject}`,
      `Message-ID: ${long}`,
      `In-Reply-To: ${parentId}`,
      `References: ${parentId}`
    );
    expect(dropped.message_id).toMatch(/^[0-9a-f]{8}$/);
    expect([dropped.in_reply_to, dropped.reference_ids]).toEqual([parentId, [parentId]]);
    // A reply that names it by the long id loses that id in the same way.
    const answer = await withHeaders(
      "From: other@remote.example",
      "Date: Tue, 03 Mar 2026 12:00:00 +0000",
      `Subject: Answer ${uniqueName("thread")}`,
      `Message-ID: ${messageId("answer")}`,
      `In-Reply-To: ${long}`,
      `References: ${long}`
    );
    expect([answer.in_reply_to, answer.reference_ids]).toEqual([null, null]);

    const client = server.client();
    await client.login("admin", ADMIN_PASSWORD);
    const thread = async (mail: Row) => ((await client.get(`/api/mails/thread/${mail.mail_id}`)).body.body as { id: string }[]).map((row) => row.id);
    expect(await thread(dropped)).toEqual([parent.mail_id, dropped.mail_id]);
    expect(await thread(parent)).toEqual([parent.mail_id, dropped.mail_id]);
    expect(await thread(answer)).toEqual([answer.mail_id]);
  });

  test("In-Reply-To keeps one id: the last bracketed one", async () => {
    expect((await idsOf("In-Reply-To: <parent@remote.example>")).inReplyTo).toBe("<parent@remote.example>");
    expect((await idsOf("In-Reply-To: <a@remote.example> <b@remote.example>")).inReplyTo).toBe("<b@remote.example>");
    expect((await idsOf('In-Reply-To: Message from "X" of Tue <parent@remote.example> (sic)')).inReplyTo).toBe("<parent@remote.example>");
    // An id without angle brackets is not an id.
    expect((await idsOf("In-Reply-To: parent@remote.example")).inReplyTo).toBeNull();
    expect((await idsOf("In-Reply-To:")).inReplyTo).toBeNull();
  });

  test("References keeps the bracketed ids in order, without duplicates", async () => {
    expect(
      (await idsOf("References: <a@remote.example> <b@remote.example>", "\t<c@remote.example>,<a@remote.example> junk <d@remote.example>")).references
    ).toEqual(["<a@remote.example>", "<b@remote.example>", "<c@remote.example>", "<d@remote.example>"]);
    expect((await idsOf("References: none here")).references).toBeNull();
  });

  test("References keeps only the newest fifty", async () => {
    const all = Array.from({ length: 60 }, (_, i) => `<ref${i}@remote.example>`);
    expect((await idsOf(`References: ${all.join("\r\n ")}`)).references).toEqual(all.slice(10));
  });

  test("an id longer than 512 characters is dropped from the threading headers", async () => {
    const long = `<${"x".repeat(520)}@remote.example>`;
    const ids = await idsOf(`In-Reply-To: ${long}`, `References: <ok@remote.example> ${long}`);
    expect(ids.inReplyTo).toBeNull();
    expect(ids.references).toEqual(["<ok@remote.example>"]);
  });

  test("of repeated threading headers the last is stored", async () => {
    const ids = await idsOf(
      "In-Reply-To: <first@remote.example>",
      "In-Reply-To: <second@remote.example>",
      "References: <r1@remote.example>",
      "References: <r2@remote.example> <r3@remote.example>"
    );
    expect(ids.inReplyTo).toBe("<second@remote.example>");
    expect(ids.references).toEqual(["<r2@remote.example>", "<r3@remote.example>"]);
  });
});

describe("the header block", () => {
  test("bare-LF line endings are accepted", async () => {
    const mail = await mailer.store("From: sender@remote.example\nSubject: lf only\nTo: hello@mail.test\n\nline one\nline two\n");
    expect([mail.subject, mail.text]).toEqual(["lf only", "line one\nline two\n"]);
  });

  test("a line with no colon is ignored, and the headers around it are still read", async () => {
    const mail = await withHeaders(...BASE, "this line has no colon", "Subject: survived");
    expect(mail.subject).toBe("survived");
    expect(mail.text).toBe("body\n");
  });

  test("headers other than the ones stored are ignored", async () => {
    const mail = await withHeaders(
      ...BASE,
      "Subject: extras",
      "Received: from a by b; Tue, 03 Mar 2026 10:20:30 +0000",
      "Return-Path: <bounce@remote.example>",
      "X-Spam-Flag: YES",
      "Sender: other@remote.example",
      "List-Unsubscribe: <mailto:leave@remote.example>",
      "DKIM-Signature: v=1; a=rsa-sha256; d=remote.example; s=x; b=abc"
    );
    expect(mail.subject).toBe("extras");
    expect(mail.from_address).toEqual([{ name: "", address: "sender@remote.example" }]);
    expect([mail.spam_score, mail.is_spam]).toEqual([0, false]);
  });

  test("a header block larger than 1 MiB fails the delivery with a transient 450", async () => {
    // NOTE: the parser's refusal is permanent in nature (the message will never
    // parse), but it is answered with the default transient code, so a sender retries it.
    const { final } = await mailer.send(crlf("Subject: huge", `X-Filler: ${"x".repeat(1024 * 1024)}`, "", "body"));
    expect(final.raw).toBe("450 Max header size for a MIME node exceeded");
    // The session carries on.
    expect((await mailer.store(crlf("Subject: after", "", "ok"))).subject).toBe("after");
  });
});
