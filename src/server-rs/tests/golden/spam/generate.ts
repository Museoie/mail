/**
 * Golden fixtures for the Rust port of the spam filter.
 *
 * Runs the Bun server's own spam code over a fixed corpus and writes inputs
 * and outputs next to this file. `src/server-rs/tests/spam_golden.rs` and the
 * repository unit tests load the fixtures and require the Rust port to answer
 * identically, so the corpus is where a difference between the two servers is
 * caught.
 *
 * Regenerate after changing anything under `src/server/lib/spam` or the spam
 * table models, from the repository root:
 *
 *   bun src/server-rs/tests/golden/spam/generate.ts
 *
 * The output is deterministic: no clock, no network, a seeded generator.
 */

import { writeFileSync } from "fs";
import { join } from "path";

import { pool, resetPool } from "../../../../server/lib/postgres/client";
import {
  ALLOWLIST_COUNT_MAX,
  ALLOWLIST_PATTERN_MAX_BYTES,
  spamAllowlistTable,
} from "../../../../server/lib/postgres/models/spam_allowlist";
import { spamTrainingTable } from "../../../../server/lib/postgres/models/spam_training";
import { isEnvelopeAligned } from "../../../../server/lib/spam/alignment";
import { classifyEmail, extractTokens, tokenize } from "../../../../server/lib/spam/classifier";
import {
  aggregateVerdicts,
  checkDnsbls,
  DEFAULT_DNSBLS,
  DnsblAnswer,
  DnsblVerdict,
  isRealListing,
  toIpv4,
  verdictForAnswer,
  verdictForDnsError,
} from "../../../../server/lib/spam/dnsbl";
import { DEFAULT_RULES, evaluateRules } from "../../../../server/lib/spam/rules";
import { checkSpam, CheckSpamDeps } from "../../../../server/lib/spam/service";
import { DnsBlocklist, EmailContext, SpamFilterConfig } from "../../../../server/lib/spam/types";

// The cases that make a lookup throw are expected to; keep their warnings out
// of the generator's output.
process.env.LOG_LEVEL = "error";

const OUT_DIR = import.meta.dir;

/** One case per line, so a regenerated fixture diffs by case. */
const write = (name: string, header: Record<string, unknown>, cases: unknown[]): void => {
  const head = Object.entries(header).map(([key, value]) => `${JSON.stringify(key)}:${JSON.stringify(value)},`);
  const body = cases.map((c) => JSON.stringify(c)).join(",\n");
  writeFileSync(join(OUT_DIR, name), `{${head.join("")}"cases":[\n${body}\n]}\n`);
  console.log(`${name}: ${cases.length} cases`);
};

/** mulberry32: a small seeded generator, so every run draws the same corpus. */
const seeded = (seed: number) => {
  let state = seed >>> 0;
  const next = (): number => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
  const int = (bound: number): number => Math.floor(next() * bound);
  const pick = <T>(items: readonly T[]): T => items[int(items.length)];
  return { next, int, pick };
};

/** Distinct letter-only words: aaa, aab, aac, ... */
const letterWord = (index: number): string => {
  let n = index;
  let word = "";
  for (let i = 0; i < 4; i++) {
    word = String.fromCharCode(97 + (n % 26)) + word;
    n = Math.floor(n / 26);
  }
  return `w${word}`;
};

// ---------------------------------------------------------------------------
// Lowercasing: every code point and the context-sensitive forms
// ---------------------------------------------------------------------------

const lowercaseFixture = (): void => {
  const mappings: [number, number[]][] = [];
  for (let cp = 0; cp <= 0x10ffff; cp++) {
    if (cp >= 0xd800 && cp <= 0xdfff) continue;
    const source = String.fromCodePoint(cp);
    const lowered = source.toLowerCase();
    if (lowered !== source) mappings.push([cp, [...lowered].map((c) => c.codePointAt(0) as number)]);
  }
  // Final sigma depends on what surrounds it; the dotted capital I expands.
  const contexts = [
    "ΌΣΟΣ", "Σ", "ΑΣ", "ΑΣ.", "ΑΣΑ", "Σ.Σ", "AΣ", "A\u0301Σ\u0301", "aΣ b", "aΣ'b", "ΑΣ\u00adΑ", "\u{1d7ca}Σ",
    "İ", "İSTANBUL", "ǅ", "ẞ", "\u212a", "\u212b", "ΑΣ\u0345", "Σ\u0345Σ", "aΣ:b", "aΣ·b", "aΣ.b", "aΣ1", "1Σ",
    "ᾈΣ", "Σς", "ΣΣ", "ΣΣΣ ", "a\u{e0001}Σ", "aΣ\u{e0001}b", "aΣ\u200db", "ⅠΣ", "ⒶΣ", "ªΣ", "ʰΣ", "aΣʰ",
    "aΣ\u02b0b", "ΟΔΟΣ.GR", "ΟΔΟΣ GR", "ΟΔΟΣ", "ΟΔΟΣ@ΟΔΟΣ", "STRASSE", "STRAẞE", "ÉCOLE", "ǅEMAL", "ǱAKARTA",
    "ＦＵＬＬＷＩＤＴＨ", "𐐀𐐁𐐂", "ᲐᲑᲒ", "ꙀꙂ", "ԱԲԳ", "ΆΈΉ", "I\u0307", "IJ", "Ǆ", "Ω", "\u2126", "ϴ", "ẞß",
  ];
  writeFileSync(
    join(OUT_DIR, "lowercase.json"),
    `${JSON.stringify({
      unicode: process.versions.unicode,
      mappings,
      contexts: contexts.map((input) => [input, input.toLowerCase()]),
    })}\n`
  );
  console.log(`lowercase.json: ${mappings.length} mappings, ${contexts.length} contexts`);
};

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

const tokenizeInputs = (): string[] => {
  const inputs: string[] = [
    // Nothing to find
    "", " ", "   ", "\n", "\t\r\n", "!!!", "... --- ...", "?!.,;:'\"()[]{}", "12345", "1 2 3", "3.14159", "50% off 100%",
    "a", "ab", "a b c", "ab cd ef", "ab1cd2ef", "a1b2c3d4", "I am", "to be or", "\u0000", "\u0000\u0001\u0002",
    // Plain words
    "abc", "abcd", "Hello World foo", "hello hello HELLO Hello hELLo", "The quick brown fox jumps over the lazy dog",
    "spam spam spam", "zeta alpha zeta beta alpha gamma", "Buy now! 50% off. Call 1-800-SPAM today.",
    "hi me it go the to", "UPPER lower MiXeD", "abc1def", "abc123def456ghi", "x1abc", "abc1x", "abcabc abc",
    "don't can't won't it's", "e-mail co-operate re-enter", "snake_case_word camelCaseWord PascalCase",
    "under_score __dunder__ __total__", "word.word.word", "a.b.c.def", "one,two;three:four", "tab\tseparated\tvalues",
    "line\none\r\ntwo\rthree", "trailing   ", "   leading", "mid   dle", "abc\u0000def", "abc\u0007def\u001fghi",
    // HTML
    "<b>Click</b> here for <a href='x'>offers</a>", "<p>Hello</p>", "<p>Hello</p><p>World</p>", "<br>", "<br/>",
    "<>", "< >", "<<>>", "<<<>>>", "<", ">", "<<", ">>", "><", "<a", "a>", "a < b > c", "a < b", "a > b", "1 < 2 > 1",
    "if (abc < def && ghi > jkl) return", "<abc def ghi>", "<abc\ndef\nghi>", "<abc\r\ndef>visible", "x<y>z", "x <y> z",
    "<p>one<p>two", "</closing>only", "<p>unclosed <b>tag", "text <b unclosed", "text <b unclosed\nmore text here",
    "<a href=\"http://bit.ly/x\">click here</a>", "<a href='a>b'>label</a>", "<img alt=\"a > b\" src=x>tail",
    "<!-- comment text -->shown", "<!-- a --> middle <!-- b -->", "<!--", "-->", "<!---->", "<![CDATA[inside]]>after",
    "<script>var hidden = true;</script>", "<style>.cls { color: red }</style>", "<p style=\"color:red\">styled</p>",
    "&nbsp;&amp;&lt;&gt;&quot;", "&lt;b&gt;escaped&lt;/b&gt;", "fish &amp; chips", "&#169; &#x41;&#x42;&#x43;",
    "<p>caf&eacute;</p>", "<div><span>deeply <em>nested <strong>markup</strong></em></span></div>",
    "<p\n  class=\"multi\"\n  id=\"line\"\n>body</p>", "<<b>>double<</b>>", "<b><i></i></b>", "<><><>", "<a><b><c>",
    "word<br>word<br/>word", "abc<>def", "abc< >def", "abc<x>def", "ab<x>cd", "abc<1>def", "<abc>", "<abc", "abc>",
    "<table><tr><td>cell one</td><td>cell two</td></tr></table>", "<html><head><title>Title</title></head><body>Body</body></html>",
    "<p>Click <a href='http://bit.ly/x'>here</a> for your free money prize winner!!!</p>", "<\n>", "<\t>", "<é>", "<中>",
    "< b >spaced tag< /b >", "<b >trailing</b >", "5 < 6 and 7 > 3 but <b>bold</b>", "a<b", "a<b>c<d", "<a>b</a>c<d",
    // URLs and addresses
    "http://bit.ly/x", "https://www.example.com/path/to/page?query=one&other=two#fragment", "user.name+tag@example.co.uk",
    "ftp://files.example.org:21/pub/readme.txt", "www.example.com", "example.com/abc/def", "mailto:someone@example.com",
    "https://t.co/abc123 https://goo.gl/maps/xyz https://tinyurl.com/foobar", "//cdn.example.com/lib.min.js",
    "http://192.168.1.1:8080/admin", "http://[2001:db8::1]/", "https://xn--bcher-kva.example/", "a@b.c",
    "data:text/html;base64,PGh0bWw+PGJvZHk+aGVsbG88L2JvZHk+PC9odG1sPg==", "magnet:?xt=urn:btih:abcdef",
    // Long words
    "a".repeat(99), "a".repeat(100), "a".repeat(101), "a".repeat(150), "a".repeat(1000), "ab".repeat(2000),
    `${"x".repeat(100)} ${"y".repeat(101)} short`, `${"q".repeat(300)}1${"r".repeat(300)}`,
    "aGVsbG8gd29ybGQgdGhpcyBpcyBhIGJhc2U2NCBlbmNvZGVkIHN0cmluZyB0aGF0IGlzIHF1aXRlIGxvbmc=",
    "0123456789abcdef0123456789abcdef", "deadbeefcafebabe", `${"word ".repeat(50)}`, "supercalifragilisticexpialidocious",
    "pneumonoultramicroscopicsilicovolcanoconiosis and floccinaucinihilipilification",
    // Unicode
    "naïve café résumé", "über straße groß", "STRASSE STRAẞE", "Москва привет мир", "ΣΟΦΟΣ σοφός ΌΣΟΣ", "ΑΣ ΑΣΑ Σ",
    "中文字符 日本語 한국어", "emoji 😀 party 🎉 time", "😀😀😀", "👨‍👩‍👧‍👦 family", "flag 🇯🇵 code", "𝐀𝐁𝐂 𝒶𝒷𝒸 mathematical",
    "ＡＢＣ ｆｕｌｌｗｉｄｔｈ", "ⓐⓑⓒ circled", "ⅰⅱⅲ roman", "𐐀𐐁𐐂 deseret", "ǅ titlecase ǆ", "clic\u212a here", "\u212a\u212a\u212a",
    "\u212aelvin \u212bngstrom", "İstanbul İİİ", "İSTANBUL", "ıstanbul dotless", "I\u0307stanbul", "e\u0301cole (combining)",
    "cafe\u0301 au lait", "zero\u200bwidth\u200bspace", "soft\u00adhyphen\u00adated", "non\u00a0breaking\u00a0space",
    "word\u2060joiner", "bidi\u202eoverride", "עברית שלום", "العربية مرحبا", "हिन्दी नमस्ते", "ไทย สวัสดี", "ǈǉ ǋǌ ǲǳ",
    "ﬁne ﬂow oﬃce", "ß ẞ ſ long s", "ĳ Ĳ digraph", "æon Æon œuvre Œuvre", "Ångström Å", "smart “quotes” and ‘single’",
    "em—dash en–dash", "ellipsis… more", "bullet • point", "trade™ mark® copy©", "price €100 £50 ¥1000", "½ ¼ ¾ ²³",
    "a\u0300b\u0301c\u0302", "abc\ufeffdef", "\ufeffabc", "abc\u0085def", "abc\u2028def\u2029ghi", "tab\u000bvert\u000cform",
    "\u{1f600}abc\u{1f600}def", "abc\u{10ffff}def", "\u{e0041}\u{e0042} tags", "A\u030aB", "K\u212ak", "ΑΒΓ abc ΑΒΓ",
    "mixedΣigma", "wordΣ", "Σword", "aΣb", "abcΣdef", "теst mіxed sсript", "раypal", "g00gle", "micr0soft", "ａｂｃabc",
    // Mixed content
    "Subject: Re: [list] Fwd: FW: hello", "Dear Sir/Madam,\n\nI am writing to inform you...\n\nRegards", "$$$ MAKE MONEY FAST $$$",
    "FREE!!! Click here >>> http://bit.ly/x <<< NOW", "Order #12345 has shipped. Track: 1Z999AA10123456784",
    "Meeting at 3pm in room B-12, bring the Q3 report", "SELECT * FROM users WHERE id = 1; DROP TABLE users;--",
    "{\"json\": \"value\", \"nested\": {\"key\": [1, 2, 3]}}", "function foo() { return bar < baz ? qux : quux; }",
    "C:\\Users\\Name\\Documents\\file.txt", "/usr/local/bin/script.sh --flag=value", "=?UTF-8?B?SGVsbG8gV29ybGQ=?=",
    "BEGIN:VCALENDAR\nVERSION:2.0\nEND:VCALENDAR", "-----BEGIN PGP MESSAGE-----", "unsubscribe | manage preferences | view in browser",
  ];

  // The cap on distinct words: 499, 500, 501 and 600, with and without repeats.
  for (const count of [499, 500, 501, 600]) {
    inputs.push(Array.from({ length: count }, (_, i) => letterWord(i)).join(" "));
  }
  inputs.push(Array.from({ length: 600 }, (_, i) => `word${i}`).join(" "));
  inputs.push(Array.from({ length: 700 }, (_, i) => letterWord(i % 350)).join(" "));
  inputs.push(`${Array.from({ length: 500 }, (_, i) => letterWord(i)).join(" ")} waaab late extra`);
  inputs.push(Array.from({ length: 520 }, (_, i) => `<i>${letterWord(i)}</i>`).join(""));
  inputs.push(Array.from({ length: 1200 }, (_, i) => letterWord(i)).join("1"));

  // Seeded noise from an alphabet that mixes words, markup and scripts.
  const random = seeded(0x5eed0001);
  const pieces = [
    "a", "b", "c", "e", "t", "o", "A", "Z", "Q", "\u212a", "İ", "ı", "é", "É", "Σ", "σ", "ς", "ß", "中", "😀", "𝐀", "\u0301",
    "\u200b", "\u00ad", " ", " ", "\n", "\t", "<", "<", ">", ">", "/", "=", "'", "\"", "&", "1", "9", "-", "_", ".", "!", "@",
  ];
  for (let i = 0; i < 220; i++) {
    const length = random.int(70);
    let text = "";
    for (let j = 0; j < length; j++) text += random.pick(pieces);
    inputs.push(text);
  }
  const fragments = [
    "<b>", "</b>", "<a href='x'>", "</a>", "<", ">", "<>", "< >", "word", "Hello", "&nbsp;", "\n", "<br/>", "<!--", "-->",
    "spam", "FREE", " ", " ", "the", "Offer", "<p class=\"x y\">", "</p>", "ab", "c", "İ", "\u212a", "é", "1", "!", "<<", ">>",
  ];
  for (let i = 0; i < 220; i++) {
    const length = random.int(24);
    let text = "";
    for (let j = 0; j < length; j++) text += random.pick(fragments);
    inputs.push(text);
  }
  return inputs;
};

// ---------------------------------------------------------------------------
// Mails for the rule engine, the classifier and the service
// ---------------------------------------------------------------------------

const ADDRESS_POOL: (string | undefined)[] = [
  undefined, "", "a@example.com", "A@EXAMPLE.COM", "b@example.com", "b@mail.example.com", "a@notexample.com",
  "postmaster", "b@", "@example.com", "@", "a@b@c", "a@example.com@evil.test", " a@example.com", "a@example.com ",
  "a@exämple.com", "a@EXÄMPLE.com", "a@İstanbul.tr", "a@i\u0307stanbul.tr", "a@istanbul.tr", "a@ΟΔΟΣ", "a@οδος",
  "a@οδοσ", "a@ΟΔΟΣ.gr", "a@οδοσ.gr", "a@οδος.gr", "a@STRASSE.de", "a@straße.de", "a@\u212aelvin.example",
  "a@kelvin.example", "a@example.com.", "<a@example.com>", "a@[127.0.0.1]", "bounces+9f@github.com",
  "notifications@github.com", "a\u0000@example.com", "a@example.com\n", "Spammer@UT-Allow.Example",
  "bounce@ut-allow.example", "envelope-sender@external.example",
];

const SHORTENERS = [
  "bit.ly", "tinyurl.com", "t.co", "goo.gl", "ow.ly", "is.gd", "buff.ly", "adf.ly", "cutt.ly", "rebrand.ly", "short.link",
];

const MARKETING = ["special offer", "limited time", "act now", "buy now", "click here", "free gift", "exclusive deal"];
const UNSUBSCRIBE = ["unsubscribe", "opt out", "opt-out", "manage preferences"];
const SUSPICIOUS = [
  "congratulations you have won", "you are a winner", "claim your prize", "urgent action required",
  "your account will be suspended", "verify your account immediately", "nigerian prince", "wire transfer", "lottery winner",
];
const CRYPTO = ["bitcoin investment", "crypto millionaire", "guaranteed returns", "double your bitcoin", "free crypto"];

const base = (overrides: EmailContext): EmailContext => ({
  fromAddress: "sender@example.com",
  subject: "A perfectly ordinary subject",
  ...overrides,
});

const ruleEmails = (): EmailContext[] => {
  const emails: EmailContext[] = [
    {},
    { subject: "Test", text: "Hello" },
    { fromAddress: "test@example.com", subject: "Test", text: "Hello" },
    { fromName: "Someone", subject: "Test", text: "Hello" },
    { fromAddress: "", fromName: "", subject: "Test", text: "Hello" },
    { fromAddress: "", fromName: "Name", subject: "Test", text: "Hello" },
    { fromAddress: "a@b.c", fromName: "", subject: "Test", text: "Hello" },
    { fromAddress: "colleague@work.com", subject: "Quick question about the report", text: "Hi, can you send me the latest version of the quarterly report? Thanks!" },
    { subject: "URGENT!!!! YOU HAVE WON!!!!", text: "Click bit.ly/scam now! Congratulations you have won!" },
    { fromAddress: "newsletter@company.com", replyToAddress: "marketing@othercompany.com", fromName: "Newsletter", subject: "Weekly newsletter", html: "<p>Read our weekly update. To unsubscribe click here.</p>" },
    { fromAddress: "spammer@ut-allow.example", fromName: "spammer@ut-allow.example", subject: "FREE MONEY WINNER ACT NOW!!!", text: "Click http://bit.ly/x to claim your free money prize winner. Act now!!!", html: "<p>Click <a href='http://bit.ly/x'>here</a> for your free money prize winner!!!</p>" },
    { fromAddress: "x@y.com", subject: "Hello there", text: "Hi friend, hope you are well." },
    // An unused field must not change anything.
    { fromAddress: "x@y.com", subject: "Hello there", text: "hello", headers: { "x-mailer": "bit.ly special offer" }, remoteAddress: "198.51.100.7", envelopeFromAddress: "bounce@y.com" },
  ];

  // reply-to-mismatch: every pairing of well-formed and malformed addresses.
  const replyPool = ADDRESS_POOL.slice(0, 30);
  for (const fromAddress of replyPool) {
    for (const replyToAddress of replyPool) {
      emails.push({ fromAddress, replyToAddress, fromName: "N", subject: "Pairing", text: "body" });
    }
  }

  // excessive-caps-subject and excessive-exclamation
  for (const subject of [
    "THIS IS ALL CAPS Message", "This is a normal subject line", "ABCDe", "ABCde", "ABcde", "abcde", "ABCD", "ABCDE", "AbCdEf",
    "ABCdef", "ABCDef", "AAAAA11111", "aaaaaAAAAA", "aaaaAAAAAA", "A1B2C3D4E5", "A B C D E", "ÀÉÎÕÜ", "ÀÉÎÕÜ abcde", "ÀÉÎÕÜ ABCDE",
    "ΑΒΓΔΕ", "ＡＢＣＤＥ", "RE: FW: OK", "Re: your invoice", "URGENT", "URGENT reply needed", "URGENT REPLY needed", "12345", "!!!!!",
    "a!", "a!!", "a!!!", "a!!!!", "!a!b!", "! ! !", "！！！", "wow!!", "wow!!!", "WOW!!!", "Sale!! Now!", "\u212a\u212a\u212a\u212a\u212a",
    "İİİİİ", "ABCDE".repeat(500), "abcdE".repeat(500), "Abcde!!!".repeat(200),
  ]) {
    emails.push(base({ subject, text: "plain body" }));
  }

  // empty-subject: what `trim()` strips, and what merely looks blank.
  const blanks = [
    0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x85, 0xa0, 0x1680, 0x180e, 0x2000, 0x2001, 0x2002, 0x2003,
    0x2004, 0x2005, 0x2006, 0x2007, 0x2008, 0x2009, 0x200a, 0x200b, 0x200c, 0x200d, 0x200e, 0x200f, 0x2028, 0x2029, 0x202f,
    0x205f, 0x2060, 0x2800, 0x3000, 0x3164, 0xfeff, 0xffa0, 0x00, 0x7f, 0xad,
  ];
  for (const cp of blanks) {
    emails.push(base({ subject: String.fromCodePoint(cp), text: "plain body" }));
    emails.push(base({ subject: ` ${String.fromCodePoint(cp)} `, text: "plain body" }));
  }
  for (const subject of [undefined, "", " ", "  \t\r\n  ", "\u00a0\u3000\ufeff", " x ", ".", "0"]) {
    emails.push({ fromAddress: "sender@example.com", subject, text: "plain body" });
  }

  // url-shortener: every shortener as a host, and inside something that is not one.
  const around: [string, string][] = [
    ["", ""], ["https://", "/abc"], ["http://", ""], ["Click ", " now"], ["(", ")"], ["<", ">"], ["\"", "\""], ["=", "&x=1"],
    ["x", ""], ["x.", ""], ["www.", "/x"], ["-", ""], ["_", ""], ["9", ""], ["", "x"], ["", "9"], ["", "-x"], ["", ".evil.com"],
    ["", "/"], ["", "."], ["", ","], ["", "_"], ["", ":8080"], ["é", ""], ["", "é"], ["中", "中"], ["\n", "\n"], ["\t", "\t"],
    ["@", ""], ["user@", ""], ["//", "//"], ["a ", " b"], ["A", ""], ["", "A"], ["Z.", ""], ["😀", "😀"], [".", ""], ["..", ""],
    ["\u212a", ""], ["", "\u212a"], ["\u0130", ""], ["", "\u0130"], ["\u00a0", "\u00a0"],
  ];
  for (const shortener of SHORTENERS) {
    for (const [before, after] of around) {
      emails.push(base({ text: `${before}${shortener}${after}` }));
    }
    emails.push(base({ text: shortener.toUpperCase() }));
    emails.push(base({ text: `see ${shortener.replace(".", " . ")} maybe` }));
    emails.push(base({ text: `see ${shortener.replace(".", "\u2024")} maybe` }));
    emails.push(base({ html: `<a href="https://${shortener}/x">link</a>` }));
    emails.push(base({ html: `<p>visit ${shortener}/x today</p>` }));
    emails.push(base({ html: `<p>visit</p>${shortener}<p>today</p>` }));
    emails.push(base({ text: "nothing here", html: `<p>${shortener}/x</p>` }));
    emails.push(base({ text: "", html: `<p>${shortener}/x</p>` }));
  }
  for (const text of [
    "https://email.wealthfront.com/ls/click?upn=abc", "https://www.marriott.com/hotels/reservation", "Sign in: https://www.att.com/myaccount",
    "Check https://scott.com/orders", "https://abbott.com/labs", "https://elliot.com/products", "Visit https://wealthfront.com or https://bit.ly/foo",
    "t.co t.co t.co", "att.co", "t.com", "t.co.uk", "xt.co t.co", "t.cot.co", "t.co-t.co", "bit.lybit.ly", "bit.ly.bit.ly", "bit-ly", "bitly",
    "bit,ly", "bit.l", "it.ly", "rebrand.ly/x and brand.ly", "short.lin\u212a", "SHORT.LIN\u212a", "b\u0130t.ly", "bıt.ly", "BİT.LY", "ＢＩＴ.ＬＹ",
    "goo.gle", "goo.gl.", "is.gd/", "this.gd", "now.ly", "ow.ly", "how.ly", "buff.ly!", "stuff.ly", "cutt.lyx", "adf.lyadf.ly",
  ]) {
    emails.push(base({ text }));
  }

  // marketing-no-unsubscribe
  for (const phrase of MARKETING) {
    emails.push(base({ text: `Hello, ${phrase} for you` }));
    emails.push(base({ text: phrase.toUpperCase() }));
    emails.push(base({ text: phrase.replace(" ", "  ") }));
    emails.push(base({ text: phrase.replace(" ", "\n") }));
    emails.push(base({ text: phrase.replace(" ", "\u00a0") }));
    emails.push(base({ html: `<p>${phrase}</p>` }));
    emails.push(base({ html: `<p>${phrase.replace(" ", " <b>")}</b></p>` }));
    emails.push(base({ html: `<p>${phrase.replace(" ", "<br>")}</p>` }));
    emails.push(base({ subject: phrase, text: "nothing to see" }));
    for (const out of UNSUBSCRIBE) {
      emails.push(base({ text: `${phrase}. To leave: ${out}` }));
      emails.push(base({ text: `${phrase}. To leave: ${out.toUpperCase()}` }));
    }
  }
  for (const out of UNSUBSCRIBE) emails.push(base({ text: `Just ${out}` }));
  for (const text of [
    "Limited time offer! Buy now and save 50%! Act now before it's too late!", "Limited time offer! Buy now and save 50%! Click here to unsubscribe.",
    "special offers", "a special offering", "react now", "act nowhere", "CLIC\u212a HERE", "cl\u0130ck here", "click  here", "click-here",
    "opt  out", "optout", "opt_out", "un-subscribe", "act now optout", "act now opt\u2011out", "free gifts opt out", "buy now... manage  preferences",
  ]) {
    emails.push(base({ text }));
  }

  // suspicious-phrases and cryptocurrency-spam
  for (const phrase of [...SUSPICIOUS, ...CRYPTO]) {
    emails.push(base({ text: `Notice: ${phrase}!` }));
    emails.push(base({ text: phrase.toUpperCase() }));
    emails.push(base({ text: phrase.replace(" ", "\n") }));
    emails.push(base({ text: phrase.slice(0, -1) }));
    emails.push(base({ text: phrase.slice(1) }));
    emails.push(base({ html: `<div>${phrase}</div>` }));
    emails.push(base({ html: `<div>${phrase.replace(" ", "</div><div>")}</div>` }));
    emails.push(base({ subject: phrase, text: "nothing to see" }));
  }
  for (const text of [
    "Congratulations you have won the lottery! Claim your prize now!", "Let's meet at 3pm to discuss the project.",
    "congratulations, you have won", "You are a WINNER", "a wire  transfer", "wiretransfer", "free cryptography", "freecrypto",
    "FREE CRYPTO", "nigerian princess", "lottery winners", "guaranteed return", "Double Your Bitcoin", "\u212a free crypto \u212a",
  ]) {
    emails.push(base({ text }));
  }

  // html-only-no-text, and which body the content rules read
  for (const [text, html] of [
    [undefined, undefined], [undefined, ""], ["", undefined], ["", ""], [undefined, "<p>hi</p>"], ["", "<p>hi</p>"], ["hi", "<p>hi</p>"],
    ["hi", undefined], ["hi", ""], [" ", "<p>bit.ly/x special offer</p>"], ["plain", "<p>bit.ly/x special offer</p>"],
    [undefined, "bit.ly"], [undefined, "<bit.ly>"], [undefined, "<a>bit.ly</a>"], [undefined, "x<a>bit.ly"], [undefined, "<a\n>bit.ly"],
    [undefined, "<>bit.ly<>"], [undefined, "<<>>bit.ly"], [undefined, "special<>offer"], [undefined, "special<b>offer"], [undefined, "special<b> offer"],
    [undefined, "<p>free <!-- gap -->crypto</p>"], [undefined, "<p>free crypto"], [undefined, "free > crypto < done"], [undefined, "<free crypto"],
    [undefined, "act now <unclosed special offer"], [undefined, "<p title=\"a > b\">wire transfer</p>"], [undefined, "<"], [undefined, ">"],
  ] as [string | undefined, string | undefined][]) {
    emails.push(base({ text, html }));
  }

  // Seeded combinations of everything above.
  const random = seeded(0x5eed0002);
  const subjects = [
    undefined, "", " ", "Hello", "HELLO WORLD", "Win!!!", "WIN BIG NOW!!!", "re: lunch", "Invoice 2026-11", "ΣΟΦΟΣ", "😀😀😀", "Sale!! Ends!",
    "FREE gift", "act now", "Ａｃｔ ｎｏｗ", "\u00a0", "urgent action required", "Your account",
  ];
  const bodies = [
    undefined, "", "Hello there, how are you?", "Click bit.ly/x now", "special offer inside, unsubscribe below", "limited time only!",
    "wire transfer details attached", "double your bitcoin today", "see https://www.att.com/ for more", "visit T.CO/abc", "buy now\nopt out",
    "you are a winner", "<not html> in text", "lottery winner announcement: claim your prize", "manage preferences", "free gift free crypto",
    "Meeting notes attached. Thanks!", "exclusive deal: https://tinyurl.com/deal", "guaranteed returns — opt-out anytime",
  ];
  const htmls = [
    undefined, "", "<p>Hello</p>", "<p>Click <a href='http://bit.ly/x'>here</a></p>", "<p>special offer</p><p>unsubscribe</p>",
    "<b>act</b> now", "<p>visit goo.gl/x</p>", "<div>buy now<br>opt-out</div>", "<p>crypto millionaire</p>", "plain html without tags",
    "<p>nigerian prince</p", "<span>is.gd</span>", "<img src='x'>", "<p>claim your <b>prize</b></p>",
  ];
  const names = [undefined, "", "Alice", "Bob Smith", "bit.ly"];
  for (let i = 0; i < 400; i++) {
    emails.push({
      fromAddress: random.pick(ADDRESS_POOL),
      fromName: random.pick(names),
      replyToAddress: random.next() < 0.4 ? random.pick(ADDRESS_POOL) : undefined,
      subject: random.pick(subjects),
      text: random.pick(bodies),
      html: random.pick(htmls),
    });
  }
  return emails;
};

/** Mails that exercise the tokenizer through every field. */
const tokenEmails = (): EmailContext[] => [
  {},
  { subject: "" },
  { subject: "Hello there" },
  { text: "Only a text body" },
  { html: "<p>Only an <b>html</b> body</p>" },
  { subject: "Win prizes", text: "Click here to claim your reward", html: "<p>Visit our <b>website</b></p>" },
  { subject: "", text: "", html: "" },
  { subject: "ab", text: "cd", html: "ef" },
  { subject: "abc", text: "abc", html: "abc" },
  { subject: "Subject with <", text: "text closes > it", html: "<p>and html</p>" },
  { subject: "open <", html: "close > here" },
  { subject: "a <b", text: "c> d <e", html: "f> g" },
  { subject: "joined", text: "", html: "words" },
  { subject: "Team standup tomorrow" },
  { subject: "Buy cheap meds", text: "Click here now" },
  { subject: "Win a free prize today" },
  { subject: "Team standup meeting tomorrow" },
  { subject: "CLIC\u212a HERE", text: "İstanbul calling", html: "<p>ΣΟΦΟΣ naïve</p>" },
  { subject: "x".repeat(120), text: "y".repeat(100), html: "z".repeat(99) },
  { subject: Array.from({ length: 300 }, (_, i) => letterWord(i)).join(" "), text: Array.from({ length: 300 }, (_, i) => letterWord(i + 250)).join(" ") },
  { subject: "Re: invoice", text: "Please find attached the invoice for October. Regards, Accounting", html: "<html><body><p>Please find attached the invoice for October.</p><p>Regards,<br>Accounting</p></body></html>" },
  { subject: "FREE MONEY WINNER ACT NOW!!!", text: "Click http://bit.ly/x to claim your free money prize winner. Act now!!!", html: "<p>Click <a href='http://bit.ly/x'>here</a> for your free money prize winner!!!</p>" },
  { fromAddress: "words@in.address", fromName: "Name Words", replyToAddress: "reply@words.example", subject: "only subject counts" },
  { subject: "Weekly newsletter", html: "<p>Read our weekly update. To unsubscribe click here.</p>" },
  { subject: "Hello there", text: "Hi friend, hope you are well." },
  { subject: "Click here" },
  { subject: "Win a prize" },
  { subject: "Hello team" },
];

// ---------------------------------------------------------------------------
// Classifier
// ---------------------------------------------------------------------------

type WordCounts = Record<string, { spamCount: number; hamCount: number }>;

interface ClassifierCase {
  /** Index into the fixture's `emails`. */
  email: number;
  /** null: the document-count lookup throws. */
  docCounts: { spamDocs: number; hamDocs: number } | null;
  /** null: the word-count lookup throws. */
  wordCounts: WordCounts | null;
  result: { score: number | "NaN"; reason: string | null };
}

const classifierCases = async (): Promise<{ emails: EmailContext[]; cases: ClassifierCase[] }> => {
  const random = seeded(0x5eed0003);
  const emails = tokenEmails();
  const indexOf = (email: EmailContext): number => {
    const key = JSON.stringify(email);
    const found = emails.findIndex((candidate) => JSON.stringify(candidate) === key);
    return found >= 0 ? found : emails.push(email) - 1;
  };
  const docCountPool = [
    { spamDocs: 2, hamDocs: 1 }, { spamDocs: 0, hamDocs: 10 }, { spamDocs: 10, hamDocs: 0 }, { spamDocs: 3, hamDocs: 2 },
    { spamDocs: 4, hamDocs: 1 }, { spamDocs: 1, hamDocs: 4 }, { spamDocs: 2, hamDocs: 2 }, { spamDocs: 5, hamDocs: 5 },
    { spamDocs: 10, hamDocs: 10 }, { spamDocs: 20, hamDocs: 20 }, { spamDocs: 100, hamDocs: 3 }, { spamDocs: 3, hamDocs: 100 },
    { spamDocs: 7, hamDocs: 13 }, { spamDocs: 1000000, hamDocs: 1 }, { spamDocs: 1, hamDocs: 1000000 },
    { spamDocs: 2147483647, hamDocs: 2147483647 }, { spamDocs: 123, hamDocs: 456 }, { spamDocs: 999, hamDocs: 1001 },
    // Counts a healthy table never holds; the arithmetic still has an answer.
    { spamDocs: -3, hamDocs: 10 }, { spamDocs: 10, hamDocs: -3 }, { spamDocs: -2, hamDocs: 9 }, { spamDocs: 7, hamDocs: -2 },
    { spamDocs: -1, hamDocs: 6 }, { spamDocs: -10, hamDocs: -10 },
  ];

  const cases: ClassifierCase[] = [];
  const run = async (
    email: EmailContext,
    docCounts: ClassifierCase["docCounts"],
    wordCounts: ClassifierCase["wordCounts"]
  ): Promise<void> => {
    const result = await classifyEmail("user", email, {
      getClassifierDocCounts: async () => {
        if (!docCounts) throw new Error("DB error");
        return docCounts;
      },
      getWordCounts: async () => {
        if (!wordCounts) throw new Error("DB error");
        return new Map(Object.entries(wordCounts));
      },
    });
    cases.push({
      email: indexOf(email),
      docCounts,
      wordCounts,
      result: { score: Number.isNaN(result.score) ? "NaN" : result.score, reason: result.reason },
    });
  };

  // The suite's own cases
  await run({ subject: "Win a free prize today" }, { spamDocs: 20, hamDocs: 20 }, {
    win: { spamCount: 18, hamCount: 1 }, prize: { spamCount: 16, hamCount: 1 }, free: { spamCount: 15, hamCount: 1 },
  });
  await run({ subject: "Team standup meeting tomorrow" }, { spamDocs: 20, hamDocs: 20 }, {
    standup: { spamCount: 0, hamCount: 15 }, meeting: { spamCount: 1, hamCount: 18 }, tomorrow: { spamCount: 0, hamCount: 17 },
  });
  await run({ subject: "Click here" }, { spamDocs: 10, hamDocs: 10 }, { click: { spamCount: 8, hamCount: 2 } });
  await run({ subject: "Hello there" }, null, {});
  await run({ subject: "Hello there" }, { spamDocs: 10, hamDocs: 10 }, null);
  await run({}, null, null);

  const draw = (bound: number, mode: number): number => {
    switch (mode) {
      case 0: return 0;
      case 1: return random.int(bound + 1);
      case 2: return bound;
      case 3: return bound + random.int(4);
      default: return random.int(3);
    }
  };

  for (const email of [...emails]) {
    const tokens = extractTokens(email);
    for (const docCounts of docCountPool) {
      // Corrupted totals and very long mails are drawn less often.
      const corrupted = docCounts.spamDocs < 0 || docCounts.hamDocs < 0;
      if ((corrupted && random.next() < 0.7) || (tokens.length > 60 && random.next() < 0.85)) continue;
      const scale = Math.min(Math.abs(docCounts.spamDocs), 5000);
      const hamScale = Math.min(Math.abs(docCounts.hamDocs), 5000);
      for (const mode of [0, 1, 2, 3, 4, 5]) {
        const wordCounts: WordCounts = {};
        if (mode === 0) {
          // Nothing known: both classes see every word for the first time.
        } else if (mode === 1) {
          for (const token of tokens) wordCounts[token] = { spamCount: draw(scale, 2), hamCount: draw(hamScale, 4) };
        } else if (mode === 2) {
          for (const token of tokens) wordCounts[token] = { spamCount: draw(scale, 4), hamCount: draw(hamScale, 2) };
        } else {
          for (const token of tokens) {
            if (random.next() < 0.35) continue;
            wordCounts[token] = { spamCount: draw(scale, random.int(5)), hamCount: draw(hamScale, random.int(5)) };
          }
          // A word the mail does not hold must not matter.
          wordCounts.unrelatedword = { spamCount: 999, hamCount: 0 };
        }
        if (mode === 5 && tokens.length > 0) {
          // Corrupted rows: a count of -1 is log(0), a count of -2 the log of a negative.
          wordCounts[tokens[0]] = { spamCount: random.pick([-1, -2, 0]), hamCount: random.pick([-1, -2, 3]) };
        }
        if (random.next() < 0.3) await run(email, docCounts, wordCounts);
      }
    }
  }

  // An even split with nothing known sits exactly on the boundary.
  for (const docs of [3, 5, 50, 12345]) {
    await run({ subject: "boundary case words here" }, { spamDocs: docs, hamDocs: docs }, {});
  }
  // Near the boundary from both sides.
  for (let spam = 0; spam <= 12; spam++) {
    await run({ subject: "alpha beta gamma" }, { spamDocs: 10, hamDocs: 10 }, {
      alpha: { spamCount: spam, hamCount: 12 - spam }, beta: { spamCount: 6, hamCount: 6 }, gamma: { spamCount: 12 - spam, hamCount: spam },
    });
    await run({ subject: "alpha beta" }, { spamDocs: 9, hamDocs: 11 }, {
      alpha: { spamCount: spam, hamCount: 5 }, beta: { spamCount: 5, hamCount: spam },
    });
    await run({ subject: "alpha" }, { spamDocs: 10 + spam, hamDocs: 22 - spam }, { alpha: { spamCount: 4, hamCount: 4 } });
  }
  // Many words: the sums run long before they are compared.
  const many = Array.from({ length: 500 }, (_, i) => letterWord(i));
  for (const [spamCount, hamCount] of [[9, 1], [1, 9], [5, 5], [6, 5], [5, 6], [0, 0], [10, 10], [1000, 0]]) {
    const wordCounts: WordCounts = {};
    for (const word of many) wordCounts[word] = { spamCount, hamCount };
    await run({ text: many.join(" ") }, { spamDocs: 10, hamDocs: 10 }, wordCounts);
  }
  for (let round = 0; round < 12; round++) {
    const words = many.slice(0, 20 + round * 40);
    const wordCounts: WordCounts = {};
    for (const word of words) wordCounts[word] = { spamCount: random.int(40), hamCount: random.int(40) };
    await run({ text: words.join(" ") }, { spamDocs: 37, hamDocs: 41 }, wordCounts);
  }
  return { emails, cases };
};

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

type DnsblStub = { score: number; reasons: string[]; evaluated: boolean } | "throw";
type ClassifierStub = { score: number; reason: string | null } | "throw";
type AllowlistStub = boolean | "throw";
type ConfigOverrides = Partial<Pick<SpamFilterConfig, "spamThreshold" | "enableDnsbl" | "enableRules" | "dnsbls">>;

interface ServiceCase {
  /** Index into the fixture's `emails`. */
  email: number;
  config: ConfigOverrides;
  deps: { allowlisted: AllowlistStub; dnsbl: DnsblStub; classifier: ClassifierStub };
  result: unknown;
}

const serviceCases = async (): Promise<{ emails: EmailContext[]; cases: ServiceCase[] }> => {
  const random = seeded(0x5eed0004);
  const spam: EmailContext = {
    fromAddress: "spammer@ut-allow.example",
    fromName: "spammer@ut-allow.example",
    subject: "FREE MONEY WINNER ACT NOW!!!",
    text: "Click http://bit.ly/x to claim your free money prize winner. Act now!!!",
    html: "<p>Click <a href='http://bit.ly/x'>here</a> for your free money prize winner!!!</p>",
  };
  const emails: EmailContext[] = [
    spam,
    { ...spam, envelopeFromAddress: "bounce@ut-allow.example", remoteAddress: "198.51.100.7" },
    { ...spam, envelopeFromAddress: "envelope-sender@external.example" },
    { ...spam, envelopeFromAddress: "bounce@ut-allow.example" },
    { ...spam, envelopeFromAddress: "bounce@ut-allow.example", remoteAddress: "2001:db8::1" },
    { ...spam, envelopeFromAddress: "bounce@UT-ALLOW.example", remoteAddress: "::ffff:198.51.100.7" },
    { ...spam, envelopeFromAddress: "bounce@mail.ut-allow.example", remoteAddress: "198.51.100.7" },
    { ...spam, envelopeFromAddress: "", remoteAddress: "198.51.100.7" },
    { ...spam, envelopeFromAddress: "bounce@ut-allow.example", remoteAddress: "" },
    { ...spam, fromAddress: "", envelopeFromAddress: "bounce@ut-allow.example", remoteAddress: "198.51.100.7" },
    { ...spam, fromAddress: undefined, fromName: undefined, remoteAddress: "198.51.100.7" },
    { fromAddress: "newsletter@company.com", replyToAddress: "marketing@othercompany.com", fromName: "Newsletter", subject: "Weekly newsletter", html: "<p>Read our weekly update. To unsubscribe click here.</p>" },
    { fromAddress: "x@y.com", subject: "Hello there", text: "Hi friend, hope you are well." },
    { fromAddress: "x@y.com", subject: "Hello there", text: "hello there" },
    { fromAddress: "x@y.com", envelopeFromAddress: "x@y.com", subject: "Hello there", text: "hello there", remoteAddress: "203.0.113.9" },
    { fromAddress: "x@y.com", envelopeFromAddress: "bounce@y.com", subject: "Hello there", text: "hello there", remoteAddress: "10.0.0.1" },
    { fromAddress: "a@b@c", envelopeFromAddress: "a@b@c", subject: "Odd address", text: "body text", remoteAddress: "203.0.113.9" },
    { fromAddress: "postmaster", envelopeFromAddress: "postmaster", subject: "No domain", text: "body text", remoteAddress: "203.0.113.9" },
    {},
    { subject: "Only a subject" },
    { text: "Only a body with a special offer" },
    { fromName: "Name Only", subject: "", html: "<p>crypto millionaire</p>" },
    { fromAddress: "friend@example.org", envelopeFromAddress: "friend@example.org", fromName: "Friend", subject: "Lunch on Friday?", text: "Are you free for lunch on Friday? Let me know.", remoteAddress: "192.0.2.44" },
    { fromAddress: "promo@shop.example", envelopeFromAddress: "bounces@shop.example", subject: "Exclusive deal", text: "An exclusive deal just for you. Unsubscribe any time.", remoteAddress: "192.0.2.45" },
    { fromAddress: "win@lotto.example", replyToAddress: "claims@other.example", subject: "LOTTERY WINNER!!!", text: "You are a winner. Wire transfer fee required. tinyurl.com/claim", remoteAddress: "192.0.2.46" },
  ];

  const allowlistPool: AllowlistStub[] = [false, true, "throw"];
  const dnsblPool: DnsblStub[] = [
    { score: 0, reasons: [], evaluated: true },
    { score: 0, reasons: [], evaluated: false },
    { score: 40, reasons: ["Listed in Spamhaus ZEN"], evaluated: true },
    { score: 70, reasons: ["Listed in Spamhaus ZEN", "Listed in SpamCop"], evaluated: false },
    { score: 95, reasons: ["Listed in Spamhaus ZEN", "Listed in SpamCop", "Listed in Barracuda"], evaluated: true },
    "throw",
  ];
  const classifierPool: ClassifierStub[] = [
    { score: 0, reason: null },
    { score: 49, reason: null },
    { score: 30, reason: null },
    { score: 50, reason: "Bayesian classifier: 50% spam probability (10 spam / 10 ham documents trained)" },
    { score: 75, reason: "Bayesian classifier: 75% spam probability (20 spam / 20 ham documents trained)" },
    { score: 100, reason: "Bayesian classifier: 100% spam probability (20 spam / 20 ham documents trained)" },
    { score: 30, reason: "" },
    "throw",
  ];
  const configPool: ConfigOverrides[] = [
    {}, {}, {}, { enableDnsbl: false }, { enableRules: false }, { enableDnsbl: false, enableRules: false }, { spamThreshold: 20 },
    { spamThreshold: 0 }, { spamThreshold: 1000 }, { spamThreshold: 60 }, { spamThreshold: 61 }, { dnsbls: [] },
    { dnsbls: [{ hostname: "only.example", name: "Only", score: 5 }] },
  ];

  const cases: ServiceCase[] = [];
  const run = async (
    email: EmailContext,
    config: ConfigOverrides,
    deps: ServiceCase["deps"]
  ): Promise<void> => {
    const stubs: CheckSpamDeps = {
      isAllowlisted: async () => {
        if (deps.allowlisted === "throw") throw new Error("allowlist unreachable");
        return deps.allowlisted;
      },
      classifyEmail: async () => {
        if (deps.classifier === "throw") throw new Error("classifier unreachable");
        return deps.classifier;
      },
      checkDnsbls: async () => {
        if (deps.dnsbl === "throw") throw new Error("resolver unreachable");
        return { ...deps.dnsbl, listedIn: [] };
      },
    };
    const result = await checkSpam("user", email, config, stubs);
    cases.push({ email: emails.indexOf(email), config, deps, result });
  };

  // Every dependency answer against the two mails the exemption turns on.
  for (const email of [emails[1], emails[2]]) {
    for (const allowlisted of allowlistPool) {
      for (const dnsbl of dnsblPool) {
        for (const classifier of classifierPool) {
          await run(email, {}, { allowlisted, dnsbl, classifier });
        }
      }
    }
  }
  // Every mail under every configuration, with drawn dependency answers.
  for (const email of emails) {
    for (const config of configPool) {
      for (let i = 0; i < 2; i++) {
        await run(email, config, {
          allowlisted: random.pick(allowlistPool),
          dnsbl: random.pick(dnsblPool),
          classifier: random.pick(classifierPool),
        });
      }
    }
  }
  return { emails, cases };
};

// ---------------------------------------------------------------------------
// DNS blocklists (everything that needs no network)
// ---------------------------------------------------------------------------

const dnsblFixture = async (): Promise<void> => {
  const ipInputs = [
    "198.51.100.7", "0.0.0.0", "255.255.255.255", "1.2.3.4", "::ffff:198.51.100.7", "::FFFF:203.0.113.5", "::FfFf:1.2.3.4",
    "2001:db8::1", "::1", "::", "::ffff:2001:db8::1", "::ffff:", "::ffff", ":ffff:1.2.3.4", "::ffff:1.2.3", "::ffff:1.2.3.4.5",
    "::ffff:256.1.1.1", "::ffff:::ffff:1.2.3.4", " ::ffff:1.2.3.4", "::ffff: 1.2.3.4", "0:0:0:0:0:ffff:1.2.3.4", "::ffff:0102:0304",
    "not-an-ip", "256.1.1.1", "1.2.3", "1.2.3.4.5", "", ".", "...", "....", "1...", "1.2.3.", ".1.2.3", "1..2.3", "1.2.3.4.",
    "01.02.03.04", "001.002.003.004", "0000000001.2.3.4", "010.1.2.3", "0127.0.0.1", "00000000000000000000255.1.1.1",
    "1.2.3.0256", "1.2.3.256", "1.2.3.999", "1.2.3.1000", "99999999999999999999.1.1.1", "1.2.3.4294967296",
    "1.2.3.-4", "1.2.3.+4", "1.2.3.4 ", " 1.2.3.4", "1.2.3.4\n", "1.2.3.4a", "a.b.c.d", "1,2,3,4", "1.2.3.4/24", "1.2.3.4:25",
    "0x7f.0.0.1", "1e2.0.0.1", "1.2.3.٤", "１.２.３.４", "1.2.3.4\u0000", "１２７.0.0.1", "127.0.0.1", "127.255.255.255", "126.0.0.1",
    "128.0.0.1", "10.0.0.1", "10.255.255.255", "9.255.255.255", "11.0.0.0", "172.15.255.255", "172.16.0.1", "172.31.255.255",
    "172.32.0.0", "192.168.1.1", "192.167.1.1", "192.169.1.1", "193.168.1.1", "169.254.1.1", "169.253.1.1", "169.255.1.1",
    "170.254.1.1", "::ffff:10.0.0.1", "::ffff:127.0.0.1", "::ffff:192.168.0.1", "::ffff:172.20.1.1", "::ffff:169.254.0.9",
    "010.0.0.1", "0010.0.0.1", "127.000.000.001", "192.168.01.01", "0172.016.0.1", "172.016.0.1", "172.0016.0.1", "00.0.0.0",
    "100.64.0.1", "224.0.0.1", "8.8.8.8", "1.1.1.1", "203.0.113.9", "\u212a.1.1.1", "İ.1.1.1", "::ffff\u212a1.2.3.4",
  ];
  const isPublic = (ip: string): boolean => {
    const quad = toIpv4(ip);
    if (!quad) return false;
    const [a, b] = quad.split(".").map(Number);
    return !(a === 127 || a === 10 || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168) || (a === 169 && b === 254));
  };
  const cases: unknown[] = [];
  for (const ip of ipInputs) {
    // A public address would be looked up; ask about it only with no blocklists.
    const result = await checkDnsbls(ip, isPublic(ip) ? [] : DEFAULT_DNSBLS);
    cases.push({ kind: "ip", ip, toIpv4: toIpv4(ip), usedDefaults: !isPublic(ip), checkDnsbls: result });
  }

  const addressLists = [
    [], ["127.0.0.2"], ["127.0.0.4"], ["127.0.0.10"], ["127.0.0.2", "127.0.0.10"], ["127.255.255.252"], ["127.255.255.253"],
    ["127.255.255.254"], ["127.255.255.255"], ["127.255.255.252", "127.255.255.254"], ["127.0.0.2", "127.255.255.254"],
    ["127.255.255.254", "127.0.0.2"], ["192.0.2.1"], ["10.0.0.1"], ["127.0.1.2"], ["127.0.255.255"], ["127.1.0.2"], ["127.0."],
    ["127.0"], ["127.00.0.2"], ["0127.0.0.2"], [" 127.0.0.2"], ["127.0.0.2 "], ["x127.0.0.2"], ["127.0.x"], [""], ["", "127.0.0.3"],
    ["1127.0.0.2"], ["127.10.0.2"], ["::ffff:127.0.0.2"], ["127.0.0.2", "192.0.2.1"],
  ];
  for (const addresses of addressLists) {
    cases.push({ kind: "answer", addresses, isRealListing: isRealListing(addresses), verdict: verdictForAnswer(addresses) });
  }

  const codes = [
    undefined, "", "ENOTFOUND", "ENODATA", "NOTFOUND", "NODATA", "ETIMEOUT", "ESERVFAIL", "EREFUSED", "ECONNREFUSED", "EFORMERR",
    "ENOTIMP", "EBADRESP", "EBADNAME", "ECANCELLED", "enotfound", "ENOTFOUND ", " ENODATA", "NXDOMAIN", "ENOENT", "0", "null",
  ];
  for (const code of codes) {
    cases.push({ kind: "error", code: code ?? null, verdict: verdictForDnsError(code) });
  }

  const random = seeded(0x5eed0005);
  const lists: DnsBlocklist[] = [
    ...DEFAULT_DNSBLS,
    { hostname: "zero.example", name: "Zero", score: 0 },
    { hostname: "negative.example", name: "Negative", score: -15 },
    { hostname: "big.example", name: "Big <&> \"name\"", score: 1000 },
  ];
  const verdicts: DnsblVerdict[] = ["listed", "clean", "unknown"];
  const answerSets: DnsblAnswer[][] = [
    [],
    [{ status: "rejected", reason: "query failed" }],
    [{ status: "fulfilled", value: { dnsbl: lists[0], verdict: "listed" } }, { status: "fulfilled", value: { dnsbl: lists[1], verdict: "clean" } }],
    [{ status: "fulfilled", value: { dnsbl: lists[0], verdict: "clean" } }, { status: "fulfilled", value: { dnsbl: lists[1], verdict: "unknown" } }],
    [{ status: "fulfilled", value: { dnsbl: lists[0], verdict: "listed" } }, { status: "fulfilled", value: { dnsbl: lists[1], verdict: "unknown" } }],
    [{ status: "fulfilled", value: { dnsbl: lists[0], verdict: "clean" } }, { status: "rejected", reason: "query failed" }],
    [{ status: "fulfilled", value: { dnsbl: lists[0], verdict: "listed" } }, { status: "fulfilled", value: { dnsbl: lists[1], verdict: "listed" } }],
    [{ status: "fulfilled", value: { dnsbl: lists[0], verdict: "listed" } }, { status: "fulfilled", value: { dnsbl: lists[0], verdict: "listed" } }],
  ];
  for (let i = 0; i < 120; i++) {
    const answers: DnsblAnswer[] = [];
    const length = random.int(7);
    for (let j = 0; j < length; j++) {
      answers.push(
        random.next() < 0.12
          ? { status: "rejected", reason: "query failed" }
          : { status: "fulfilled", value: { dnsbl: random.pick(lists), verdict: random.pick(verdicts) } }
      );
    }
    answerSets.push(answers);
  }
  for (const answers of answerSets) {
    cases.push({
      kind: "aggregate",
      answers: answers.map((answer) =>
        answer.status === "fulfilled"
          ? { status: "fulfilled", dnsbl: answer.value.dnsbl, verdict: answer.value.verdict }
          : { status: "rejected" }
      ),
      result: aggregateVerdicts(answers),
    });
  }

  write("dnsbl.json", { defaultDnsbls: DEFAULT_DNSBLS }, cases);
};

// ---------------------------------------------------------------------------
// The statements the table objects issue
// ---------------------------------------------------------------------------

interface Captured {
  sql: string;
  params: string[];
}

const sqlFixtures = async (): Promise<void> => {
  let captured: Captured[] = [];
  // Installed through the pool Proxy, the way the model tests do it: nothing
  // connects, every statement is recorded and answered with no rows.
  resetPool();
  (pool as unknown as { query: (sql: string, params?: unknown[]) => Promise<unknown> }).query = async (sql, params) => {
    // The pool binds every parameter as text.
    captured.push({ sql, params: (params ?? []).map((value) => String(value)) });
    return { rows: [], rowCount: 0 };
  };
  const record = async <T>(call: () => Promise<T>): Promise<{ outcome: T; statements: Captured[] }> => {
    captured = [];
    const outcome = await call();
    return { outcome, statements: captured };
  };

  const userId = "11111111-1111-1111-1111-111111111111";
  const otherUser = "22222222-2222-2222-2222-222222222222";

  const training: unknown[] = [];
  const wordLists: string[][] = [
    [], ["free"], ["free", "prize", "winner"], ["free", "free", "prize", "free"], ["a".repeat(100)], ["a".repeat(101)],
    ["__total__"], ["with space", "quote'd", "semi;colon", "$1", "ünïcode"], Array.from({ length: 500 }, (_, i) => letterWord(i)),
    tokenize("Click http://bit.ly/x to claim your free money prize winner. Act now!!!"),
  ];
  for (const words of wordLists) {
    for (const isSpam of [true, false]) {
      const { statements } = await record(() => spamTrainingTable.train(userId, words, isSpam));
      training.push({ call: "train", userId, words, isSpam, statements });
    }
    const { statements } = await record(() => spamTrainingTable.getWordCounts(otherUser, words));
    training.push({ call: "getWordCounts", userId: otherUser, words, statements });
  }
  for (const call of ["getAllForUser", "getDocCounts", "clearForUser"] as const) {
    const { statements } = await record(async () => {
      await spamTrainingTable[call](userId);
    });
    training.push({ call, userId, statements });
  }
  write("sql_training.json", {}, training);

  const allowlist: unknown[] = [];
  const patterns = [
    "alice@example.com", "ALICE@Example.COM", "*@example.com", "*@EXAMPLE.com", "BOB@Example.COM", "stranger@unknown.com",
    "x@y.com", "postmaster", "trailing@", "@leading.example", "a@B.example@c.example", "", "@", "ÀLICE@ÉXAMPLE.COM",
    "user@\u212aelvin.example", "user@İstanbul.example", "ΟΔΟΣ@ΟΔΟΣ.GR", "ΟΔΟΣ@ΟΔΟΣ", "STRAẞE@example.com", " padded@example.com ",
    `${"a".repeat(ALLOWLIST_PATTERN_MAX_BYTES - "@example.com".length)}@example.com`,
    `${"a".repeat(ALLOWLIST_PATTERN_MAX_BYTES - "@example.com".length + 1)}@example.com`,
    `${"A".repeat(ALLOWLIST_PATTERN_MAX_BYTES)}@example.com`, `${"é".repeat(200)}@example.com`, `${"é".repeat(154)}@example.com`,
    `${"é".repeat(155)}@example.com`, `${"İ".repeat(107)}@x`, `${"İ".repeat(106)}@x`, `${"😀".repeat(79)}@xy`, `${"😀".repeat(80)}@xy`,
  ];
  for (const pattern of patterns) {
    const added = await record(async () => (await spamAllowlistTable.addEntry(userId, pattern)).status);
    allowlist.push({ call: "addEntry", userId, argument: pattern, ...added });
    const listed = await record(() => spamAllowlistTable.isAllowlisted(userId, pattern));
    allowlist.push({ call: "isAllowlisted", userId, argument: pattern, ...listed });
    const removed = await record(() => spamAllowlistTable.removeByPattern(otherUser, pattern));
    allowlist.push({ call: "removeByPattern", userId: otherUser, argument: pattern, ...removed });
  }
  const byId = await record(() => spamAllowlistTable.removeById(userId, "55555555-5555-5555-5555-555555555555"));
  allowlist.push({ call: "removeById", userId, argument: "55555555-5555-5555-5555-555555555555", ...byId });
  const all = await record(async () => (await spamAllowlistTable.getAllForUser(userId)).length);
  allowlist.push({ call: "getAllForUser", userId, argument: "", ...all });
  write(
    "sql_allowlist.json",
    { countMax: ALLOWLIST_COUNT_MAX, patternMaxBytes: ALLOWLIST_PATTERN_MAX_BYTES },
    allowlist
  );
  resetPool();
};

// ---------------------------------------------------------------------------

const main = async (): Promise<void> => {
  lowercaseFixture();

  const tokenized = tokenizeInputs().map((input) => ({ input, tokens: tokenize(input) }));
  write("tokenize.json", {}, tokenized);

  const emails = ruleEmails();
  write(
    "extract_tokens.json",
    {},
    [...tokenEmails(), ...emails.filter((_, index) => index % 7 === 0)].map((email) => ({ email, tokens: extractTokens(email) }))
  );

  write(
    "rules.json",
    { defaultRules: DEFAULT_RULES.map(({ id, name, score }) => ({ id, name, score })) },
    emails.map((email) => {
      const { score, matchedRules } = evaluateRules(email);
      return { email, score, matched: matchedRules.map((rule) => rule.id) };
    })
  );

  const alignment: unknown[] = [];
  for (const header of ADDRESS_POOL) {
    for (const envelope of ADDRESS_POOL) {
      alignment.push({ header: header ?? null, envelope: envelope ?? null, aligned: isEnvelopeAligned(header, envelope) });
    }
  }
  write("alignment.json", {}, alignment);

  const classifier = await classifierCases();
  write("classifier.json", { emails: classifier.emails }, classifier.cases);
  const service = await serviceCases();
  write("service.json", { emails: service.emails }, service.cases);
  await dnsblFixture();
  await sqlFixtures();
};

await main();
