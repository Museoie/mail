/**
 * Golden fixtures for the Rust port of the MIME and mail-text modules.
 *
 * Runs the TypeScript functions over a corpus of inputs and writes inputs and
 * outputs to `fixtures/*.json`; `tests/golden.rs` runs the Rust functions over
 * the same inputs and requires identical output.
 *
 *   bun src/server-rs/tests/golden/generate.ts
 *
 * The corpus is seeded, so a run without code changes rewrites the same files.
 * Run it with TZ unset (UTC): dates without a zone are read as local time.
 *
 * To fuzz, draw a different corpus into a scratch directory and point the
 * Rust tests at it:
 *
 *   GOLDEN_SEED=7 GOLDEN_OUT=/tmp/golden-7 bun src/server-rs/tests/golden/generate.ts
 *   GOLDEN_FIXTURES=/tmp/golden-7 cargo test --manifest-path src/server-rs/Cargo.toml --test golden
 */
import { createHash } from "crypto";
import fs from "fs";
import os from "os";
import path from "path";
import punycode from "punycode";
import { decodeAttributeEntities, decodeEntities } from "../../../server/lib/mails/entities";
import { htmlToPlainText } from "../../../server/lib/mails/plain-text";
import { buildSnippet } from "../../../server/lib/mails/snippet";
import { bodyHtml, plainTextToHtml } from "../../../server/lib/mails/text-to-html";
import {
  bareMessageId,
  collectThread,
  hasThreadHeaders,
  isReplySubject,
  linkedByHeaders,
  linkedIds,
  matchesBySubject,
  normalizeSubject,
  otherParticipantsOf,
  storedIdForms,
  ThreadCandidate
} from "../../../server/lib/mails/threading";
import { getText, nestedPath } from "../../../server/lib/mails/util";
import { parseAddressHeader } from "../../../server/lib/mime/address";
import { decodeBodyBytes, decodeHeaderBytes } from "../../../server/lib/mime/charset";
import {
  decodeWords,
  ensureMessageIdFormat,
  parseHeaderValue,
  parseMessageIdList,
  rawOctetsAsUtf8,
  splitHeaderLine
} from "../../../server/lib/mime/header";
import { parseMessage } from "../../../server/lib/mime/parser";
import {
  addressesToHtml,
  embeddedHeadersAsHtml,
  embeddedHeadersAsText,
  EmbeddedHeaders,
  encodeEntities,
  textToHtml
} from "../../../server/lib/mime/text";
import { createTransferDecoder } from "../../../server/lib/mime/transfer-decoder";
import { LONG_EXTENSIONS } from "./mime-extensions";
import { parserCases, ParserCase, Raw, rawBytes } from "./parser-corpus";

/** Where the fixtures go; another directory holds a corpus drawn for one fuzzing run. */
const FIXTURES = process.env.GOLDEN_OUT ?? path.join(import.meta.dir, "fixtures");

/** Added to every seed: 0 draws the committed corpus, any other number a fresh one. */
const SEED_OFFSET = Number(process.env.GOLDEN_SEED ?? 0);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** A seeded generator, so every run draws the same corpus. */
const random = (seed: number) => {
  let state = (seed + SEED_OFFSET * 7919) >>> 0;
  const next = () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
  const int = (n: number) => Math.floor(next() * n);
  const pick = <T>(list: readonly T[]): T => list[int(list.length)];
  const chance = (p: number) => next() < p;
  /** `count` picks from `list`, joined. */
  const several = (list: readonly string[], min: number, max: number, joiner = "") =>
    Array.from({ length: min + int(max - min + 1) }, () => pick(list)).join(joiner);
  return { next, int, pick, chance, several };
};

type Random = ReturnType<typeof random>;

const LONE_SURROGATE = /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/;

const hasLoneSurrogate = (value: unknown): boolean => {
  if (typeof value === "string") return LONE_SURROGATE.test(value);
  if (Array.isArray(value)) return value.some(hasLoneSurrogate);
  if (value && typeof value === "object") {
    return Object.entries(value).some(([key, item]) => hasLoneSurrogate(key) || hasLoneSurrogate(item));
  }
  return false;
};

/** A string as it is once stored or sent as UTF-8: lone surrogates are U+FFFD. */
const wellFormed = <T>(value: T): T => {
  if (typeof value === "string") {
    return value.replace(new RegExp(LONE_SURROGATE.source, "g"), "\uFFFD") as T;
  }
  if (Array.isArray(value)) return value.map(wellFormed) as T;
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value).map(([key, item]) => [wellFormed(key), wellFormed(item)])
    ) as T;
  }
  return value;
};

const hex = (bytes: Uint8Array) => Buffer.from(bytes).toString("hex");
const sha256 = (bytes: Uint8Array | string) => createHash("sha256").update(bytes).digest("hex");

/** Outputs past this length are recorded by hash. */
const INLINE_OUTPUT_LIMIT = 4000;

const compact = (text: string): string | { sha256: string; length: number } =>
  text.length > INLINE_OUTPUT_LIMIT ? { sha256: sha256(text), length: text.length } : text;

/** A value with each long string in it recorded by hash. */
const compactDeep = (value: unknown): unknown => {
  if (typeof value === "string") return compact(value);
  if (Array.isArray(value)) return value.map(compactDeep);
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, compactDeep(item)]));
  }
  return value;
};

/** An error message, a long one by its start and hash. */
const errorText = (error: unknown): string | { start: string; sha256: string } => {
  const message = (error as Error).message;
  return message.length > 300 ? { start: [...message].slice(0, 80).join(""), sha256: sha256(message) } : message;
};

interface KnownDifference {
  function: string;
  input: unknown;
  /** The TypeScript output as JSON text, lone surrogates escaped. */
  typescript: string;
  /** What the Rust port answers: the same with each lone surrogate as U+FFFD. */
  rust: unknown;
  why: string;
}

const knownDifferences: KnownDifference[] = [];

const LONE_SURROGATE_WHY =
  "A JavaScript string can end in half a surrogate pair after a slice; a Rust string cannot. " +
  "The half is U+FFFD in Rust, as it is in JavaScript once the string is encoded as UTF-8.";

/**
 * Runs `fn` over `inputs` and pairs each input with its output. An output
 * holding a lone surrogate cannot be represented in Rust and goes to the
 * known differences instead.
 */
const run = <I, O>(name: string, inputs: I[], fn: (input: I) => O): [I, O][] => {
  const cases: [I, O][] = [];
  const seen = new Set<string>();
  for (const input of inputs) {
    const key = JSON.stringify(input);
    if (seen.has(key)) continue;
    seen.add(key);
    if (hasLoneSurrogate(input)) throw new Error(`${name}: input with a lone surrogate: ${key}`);
    const output = fn(input);
    if (hasLoneSurrogate(output)) {
      knownDifferences.push({
        function: name,
        input,
        typescript: JSON.stringify(output),
        rust: wellFormed(output),
        why: LONE_SURROGATE_WHY
      });
      continue;
    }
    cases.push([input, output]);
  }
  return cases;
};

const counts: Record<string, number> = {};

/** Writes a fixture as an object of case lists, one case per line. */
const writeFixture = (name: string, sections: Record<string, unknown[]>) => {
  const body = Object.entries(sections)
    .map(([section, cases]) => {
      counts[`${name}.${section}`] = cases.length;
      return `${JSON.stringify(section)}: [\n${cases.map((entry) => JSON.stringify(entry)).join(",\n")}\n]`;
    })
    .join(",\n");
  fs.writeFileSync(path.join(FIXTURES, `${name}.json`), `{\n${body}\n}\n`);
};

// Text that trips byte, code-unit and code-point handling apart.
const UNICODE_SAMPLES = [
  "é", "ü", "ß", "İ", "ı", "ſ", "\u212a", "Σ", "ς", "ǆ", "ŉ", "한글", "日本語", "中文", "Привет", "שלום", "مرحبا",
  "😀", "👨‍👩‍👧", "🇰🇷", "\u{1F600}\u{1F601}", "a\u0301", "\u00a0", "\u2028", "\u2029", "\ufeff", "\u200b",
  "\u3000", "\u1680", "\u0085", "\u202f", "\u0000", "\u0001", "\u007f", "\ufffd", "\u{10FFFF}", "\uffff"
];

const WHITESPACE = [" ", " ", " ", "  ", "\t", "\n", "\r\n", "\r", "\u000b", "\u000c", "\u00a0", "\u2028", "\u3000", "\ufeff", "\u0085"];

const WORDS = ["hello", "world", "the", "quick", "brown", "fox", "a", "I", "mail", "report", "2026", "x1", "re", "on", "wrote", "it's"];

// ---------------------------------------------------------------------------
// charset
// ---------------------------------------------------------------------------

/** Every label of the WHATWG Encoding Standard. */
const WHATWG_LABELS =
  "l1 l2 l3 l4 l5 l6 l9 866 mac koi gbk big5 utf8 koi8 sjis ucs-2 ms932 cp866 utf-8 cp819 ascii x-gbk greek cp1250 cp1251 latin1 gb2312 cp1252 latin2 cp1253 latin3 cp1254 latin4 cp1255 csbig5 latin5 utf-16 cp1256 ibm866 latin6 cp1257 cp1258 greek8 ibm819 arabic visual korean euc-jp koi8-r koi8_r euc-kr x-sjis koi8-u hebrew tis-620 gb18030 ksc5601 gb_2312 dos-874 cn-big5 unicode chinese logical cskoi8r cseuckr koi8-ru x-cp1250 ksc_5601 x-cp1251 iso88591 csgb2312 x-cp1252 iso88592 x-cp1253 iso88593 ecma-114 x-cp1254 iso88594 x-cp1255 iso88595 x-x-big5 x-cp1256 csibm866 iso88596 x-cp1257 iso88597 asmo-708 ecma-118 elot_928 x-cp1258 iso88598 iso88599 cyrillic utf-16be utf-16le us-ascii ms_kanji x-euc-jp iso885910 iso8859-1 iso885911 iso8859-2 iso8859-3 iso885913 iso8859-4 iso885914 iso8859-5 iso885915 iso8859-6 iso8859-7 iso8859-8 iso-ir-58 iso8859-9 csunicode macintosh shift-jis shift_jis iso-ir-100 iso8859-10 iso-ir-110 gb_2312-80 iso-8859-1 iso_8859-1 iso-ir-101 iso8859-11 iso-8859-2 iso_8859-2 hz-gb-2312 iso-8859-3 iso_8859-3 iso8859-13 iso-8859-4 iso_8859-4 iso8859-14 iso-ir-144 iso-8859-5 iso_8859-5 iso8859-15 iso-8859-6 iso_8859-6 iso-ir-126 iso-8859-7 iso_8859-7 iso-ir-127 iso-ir-157 iso-8859-8 iso_8859-8 iso-ir-138 iso-ir-148 iso-8859-9 iso_8859-9 iso-ir-109 iso-ir-149 big5-hkscs csshiftjis iso-8859-10 iso-8859-11 csisolatin1 csisolatin2 iso-8859-13 csisolatin3 iso-8859-14 windows-874 csisolatin4 iso-8859-15 iso_8859-15 csisolatin5 iso-8859-16 csisolatin6 windows-949 csisolatin9 csiso88596e csiso88598e unicodefffe unicodefeff csmacintosh csiso88596i csiso88598i windows-31j x-mac-roman iso-2022-cn iso-2022-jp csiso2022jp iso-2022-kr csiso2022kr replacement windows-1250 windows-1251 windows-1252 windows-1253 windows-1254 windows-1255 windows-1256 windows-1257 windows-1258 iso-8859-6-e iso-8859-8-e iso-8859-6-i iso-8859-8-i sun_eu_greek csksc56011987 unicode20utf8 unicode11utf8 ks_c_5601-1987 ansi_x3.4-1968 ks_c_5601-1989 x-mac-cyrillic x-user-defined csiso58gb231280 iso-10646-ucs-2 iso_8859-1:1987 iso_8859-2:1987 iso_8859-6:1987 iso_8859-7:1987 iso_8859-3:1988 iso_8859-4:1988 iso_8859-5:1988 iso_8859-8:1988 x-unicode20utf8 iso_8859-9:1989 csisolatingreek x-mac-ukrainian iso-2022-cn-ext csisolatinarabic csisolatinhebrew unicode-1-1-utf-8 csisolatincyrillic cseucpkdfmtjapanese".split(
    " "
  );

/** Labels mail software writes: aliases, spelling variants, junk. */
const ODD_LABELS = [
  "UTF-8", "Utf8", "utf_8", "UTF_8", "utf-16le", "utf_16be", "utf16le", "utf16", "utf_16", "UTF-16", "utf-7",
  "utf-32", "utf32le", "utf-88", "utf8x", "cp949", "uhc", "UHC", "windows-949", "x-windows-949", "shift-jis",
  "Shift_JIS", "euc-cn", "EUC-CN", "x-gbk", "big5-hkscs", "win1252", "win-1252", "windows_1252", "windows1251",
  "WINDOWS-1251", "winxp", "win", "latin-2", "latin_9", "Latin1", "latin99", "iso8859_15", "iso_8859-15",
  "iso885915", "iso-88591", "ISO-8859-1", "iso_8859_1", "iso8859", "l-9", "l_2", "l2x", "L9", "l0", "",
  "x", "binary", "8bit", "7bit", "7BIT", "US-ASCII", "us_ascii", "ASCII", "Us-Ascii", "constructor",
  "__proto__", "toString", "utf-8;", "utf-8 x", "\u212aoi8-r", "İso-8859-1", "unknown-8bit", "x-unknown",
  "default", "none", "utf8mb4", "cp-1252", "dos-866", "GB2312", "KOI8-R", " utf-8 ", "\tutf-8\n",
  "\u00a0utf-8", "\ufeffutf-8\ufeff", "\u0085utf-8", "\u000butf-8", "\u000cutf-8", "utf-8\u0000", " latin1 ",
  "\u3000big5\u3000", "euc_kr", "EUC-KR", "ks_c_5601", "iso-2022-jp-2", "x-mac-greek", "ibm437", "cp850",
  "utf", "8", "utf-", "windows-", "windows-125", "windows-12520", "iso-8859-", "iso-8859-0", "iso-8859-12",
  "iso-8859-17", "latin0", "latin7", "latin10", "😀", "utf-8\u{1F600}"
];

/** One sample text per legacy encoding, as bytes in it. */
const ENCODED_SAMPLES: [string, string][] = [
  ["euc-kr", "c7d1b1db20c5d7bdbac6ae"],
  ["ks_c_5601-1987", "8c63b9e6b0a2c7cf"],
  ["shift_jis", "93fa967b8cea82cc8365834c83588367"],
  ["iso-2022-jp", "1b2442467c4b5c386c1b284220616263201b2442252b253f252b254a1b2842"],
  ["euc-jp", "c6fccbdcb8eca5c6a5ada5b9a5c8"],
  ["gb18030", "d6d0cec4b2e2cad495328236a2e3"],
  ["gbk", "d6d0cec4b2e2cad4"],
  ["gb2312", "bcf2cce5d6d0cec4"],
  ["big5", "a4a4a4e5b4fab8d5"],
  ["windows-1251", "cff0e8e2e5f220ece8f0"],
  ["koi8-r", "f0d2c9d7c5d4"],
  ["koi8-u", "f0d2c9d7a6d4"],
  ["iso-8859-1", "52e973756de920636166e9"],
  ["windows-1252", "9371756f7465739420962080"],
  ["iso-8859-2", "5a61bff3b3e62067eab66cb1"],
  ["iso-8859-5", "bad8e0d8dbdbd8e6d0"],
  ["iso-8859-7", "c5ebebe7ede9eadc"],
  ["iso-8859-15", "bc7576726520e0203520a4"],
  ["windows-1250", "50f8ed6c699a209e6c759d6f75e86bfd"],
  ["windows-1253", "cae1ebe7ecddf1e1"],
  ["windows-1254", "dd7374616e62756c20f0fcfef6e7"],
  ["windows-1255", "f9ece5ed"],
  ["windows-1256", "e3d1cdc8c7"],
  ["windows-1257", "c0fe756f6c6173"]
];

const fromHex = (text: string) => Uint8Array.from(Buffer.from(text, "hex"));
const utf8 = (text: string) => Uint8Array.from(Buffer.from(text, "utf8"));
const bytesOf = (...values: number[]) => Uint8Array.from(values);
const range = (from: number, to: number) => Array.from({ length: to - from + 1 }, (_, i) => from + i);
const utf16 = (text: string, bigEndian: boolean) => {
  const bytes: number[] = [];
  for (let i = 0; i < text.length; i++) {
    const unit = text.charCodeAt(i);
    bytes.push(...(bigEndian ? [unit >> 8, unit & 0xff] : [unit & 0xff, unit >> 8]));
  }
  return Uint8Array.from(bytes);
};
const concat = (...parts: Uint8Array[]) => Uint8Array.from(parts.flatMap((part) => [...part]));

const charsetFixture = () => {
  const rng = random(101);
  const randomBytes = (length: number, pool?: number[]) =>
    Uint8Array.from(Array.from({ length }, () => (pool ? rng.pick(pool) : rng.int(256))));

  // Bytes every label is given.
  const inputs: Uint8Array[] = [
    bytesOf(),
    utf8("plain ascii text"),
    Uint8Array.from(range(0x00, 0x7f)),
    Uint8Array.from(range(0x80, 0xff)),
    utf8("héllo ✓ 😀 한글"),
    concat(bytesOf(0xef, 0xbb, 0xbf), utf8("bom é")),
    concat(utf8("a"), bytesOf(0xef, 0xbb, 0xbf), utf8("b")),
    concat(bytesOf(0xfe, 0xff), utf16("hi é 한", true)),
    concat(bytesOf(0xff, 0xfe), utf16("hi é 한", false)),
    utf16("plain", true),
    utf16("plain", false),
    utf16("한국어", false),
    utf16("한국어", true),
    utf16("a😀b", false),
    bytesOf(0x00, 0x68, 0x00),
    bytesOf(0x68),
    bytesOf(0x00),
    bytesOf(0xff),
    bytesOf(0xfe, 0xff),
    bytesOf(0xff, 0xfe),
    bytesOf(0xff, 0xfe, 0x00),
    bytesOf(0x00, 0xd8, 0x41, 0x00),
    bytesOf(0xd8, 0x00, 0x00, 0x41),
    bytesOf(0x3d, 0xd8, 0x00, 0xde, 0x00, 0xde),
    bytesOf(0xe2, 0x82),
    bytesOf(0xf0, 0x9f, 0x98, 0x41),
    bytesOf(0xed, 0xa0, 0x80, 0xed, 0xb0, 0x80),
    bytesOf(0xc0, 0x80, 0xf4, 0x90, 0x80, 0x80, 0xf8, 0x88, 0x80, 0x80, 0x80),
    bytesOf(0x1b, 0x24, 0x42, 0x46, 0x7c, 0x1b, 0x28, 0x42, 0x41),
    bytesOf(0x1b, 0x24, 0x42, 0x46),
    bytesOf(0x1b, 0x28, 0x4a, 0x5c, 0x7e, 0x1b, 0x28, 0x49, 0x31),
    bytesOf(0x81, 0x30, 0x81, 0x30, 0x84, 0x31, 0xa4, 0x39, 0x90, 0x30, 0x81, 0x30),
    bytesOf(0x80, 0xa1, 0xa1, 0xfe, 0xfe, 0xff, 0x81, 0x40, 0x81, 0x7f),
    bytesOf(0x8e, 0xa1, 0x8f, 0xa1, 0xa1, 0x8f, 0xa1),
    bytesOf(0x87, 0x40, 0x88, 0x62, 0x88, 0x64, 0xa1, 0x40, 0xf9, 0xfe),
    ...ENCODED_SAMPLES.map(([, sample]) => fromHex(sample)),
    ...Array.from({ length: 12 }, () => randomBytes(1 + rng.int(40))),
    ...Array.from({ length: 6 }, () => randomBytes(2 + rng.int(30), [...range(0x81, 0xfe), ...range(0x30, 0x7e)])),
    // Longer than the 100 code units a byte order is voted on.
    concat(utf16("한".repeat(120), true), utf16("ascii ".repeat(40), false)),
    concat(utf16("z".repeat(101), false), utf16("z".repeat(150), true))
  ];

  // Every label decodes a few inputs, which tells which encoding it names;
  // one label of each encoding, and the odd ones, decode them all.
  const labels = [...new Set([...WHATWG_LABELS, ...WHATWG_LABELS.slice(0, 40).map((label) => label.toUpperCase()), ...ODD_LABELS])];
  const few = [1, 3, 4, 5, 7, 8, 9, 10, 11, 28, 31];
  const decodeAll = (label: string, bytes: Uint8Array[]) => [
    label,
    bytes.map((input) => decodeHeaderBytes(input, label)),
    bytes.map((input) => decodeBodyBytes(input, label))
  ];
  const encodingOf = (label: string) => {
    try {
      return new TextDecoder(label).encoding;
    } catch {
      return "";
    }
  };
  const oneLabelEach = [...new Map(WHATWG_LABELS.filter(encodingOf).map((label) => [encodingOf(label), label])).values()];
  const byLabel = labels.map((label) => decodeAll(label, few.map((index) => inputs[index])));
  const byEncoding = [...oneLabelEach, "utf-16", "x-unknown", "us-ascii", "cp949", "latin-2", ""].map((label) => decodeAll(label, inputs));

  // Every byte of every single-byte encoding, and every two-byte sequence of
  // the multi-byte ones, hashed row by row.
  const singleByte = WHATWG_LABELS.filter((label) => {
    try {
      const decoded = new TextDecoder(label).decode(Uint8Array.from(range(0x80, 0xff)));
      return decoded.length === 128 && !["utf-8", "utf-16le", "utf-16be"].includes(new TextDecoder(label).encoding);
    } catch {
      return false;
    }
  });
  const canonicalSingleByte = [...new Map(singleByte.map((label) => [new TextDecoder(label).encoding, label])).values()];
  const grids = ["big5", "gbk", "gb18030", "euc-kr", "shift_jis", "euc-jp"].map((label) => [
    label,
    range(0x80, 0xff).map((lead) => {
      const row = Uint8Array.from(range(0x00, 0xff).flatMap((trail) => [lead, trail]));
      return sha256(decodeBodyBytes(row, label));
    })
  ]);
  // EUC-JP three-byte (JIS X 0212) and GB18030 four-byte sequences.
  const wide = [
    ["euc-jp", range(0xa1, 0xfe).map((second) => sha256(decodeBodyBytes(Uint8Array.from(range(0xa1, 0xfe).flatMap((third) => [0x8f, second, third])), "euc-jp")))],
    ["gb18030", range(0x81, 0xfe).map((first) => sha256(decodeBodyBytes(Uint8Array.from(range(0x30, 0x39).flatMap((second) => range(0x81, 0xfe).flatMap((third) => [0x30, 0x35, 0x39].flatMap((fourth) => [first, second, third, fourth])))), "gb18030")))],
    ["iso-2022-jp", range(0x21, 0x7e).map((lead) => sha256(decodeBodyBytes(Uint8Array.from([0x1b, 0x24, 0x42, ...range(0x21, 0x7e).flatMap((trail) => [lead, trail]), 0x1b, 0x28, 0x42]), "iso-2022-jp")))]
  ];

  // Random bytes read as UTF-8, the fallback of every unknown label.
  const utf8Pool = [0x00, 0x41, 0x7f, 0x80, 0xbf, 0xc0, 0xc2, 0xdf, 0xe0, 0xe1, 0xed, 0xef, 0xf0, 0xf4, 0xf5, 0xff, 0x9f, 0xa0, 0x90, 0x8f, 0xbb];
  const lossy = Array.from({ length: 600 }, () => randomBytes(1 + rng.int(12), utf8Pool)).map((bytes) => [
    hex(bytes),
    decodeBodyBytes(bytes, "utf-8"),
    decodeHeaderBytes(bytes, "unicode-1-1-utf-8")
  ]);

  writeFixture("charset", {
    inputs: inputs.map(hex),
    few,
    labels: byLabel,
    encodings: byEncoding,
    singleByte: canonicalSingleByte.map((label) => [label, decodeHeaderBytes(Uint8Array.from(range(0x00, 0xff)), label)]),
    grids,
    wide,
    utf8: lossy
  });
};

// ---------------------------------------------------------------------------
// header
// ---------------------------------------------------------------------------

const CHARSETS = [
  "utf-8", "UTF-8", "utf8", "iso-8859-1", "ISO-8859-1", "latin1", "us-ascii", "windows-1252", "euc-kr", "EUC-KR",
  "ks_c_5601-1987", "cp949", "shift_jis", "Shift_JIS", "iso-2022-jp", "gb18030", "gb2312", "gbk", "big5",
  "windows-1251", "koi8-r", "utf-16", "utf-16be", "utf-16le", "x-unknown", "utf-8*en", "iso-8859-1*fr-CA",
  "*", "utf_8", "7bit", "ascii", "x-user-defined", "replacement", "win1251", "latin-2", "bad charset", "é"
];

const JOIN_MARK = "__\x00JOIN\x00__";

const headerFixture = () => {
  const rng = random(202);
  const b64 = (bytes: Uint8Array) => Buffer.from(bytes).toString("base64");
  const qEncode = (bytes: Uint8Array) =>
    [...bytes]
      .map((byte) =>
        byte === 0x20 ? "_" : /[A-Za-z0-9!*+\-/]/.test(String.fromCharCode(byte)) && rng.chance(0.8)
          ? String.fromCharCode(byte)
          : "=" + byte.toString(16).toUpperCase().padStart(2, "0")
      )
      .join("");
  const sampleBytes = (): Uint8Array =>
    rng.pick([
      () => utf8(rng.pick(UNICODE_SAMPLES) + rng.pick(WORDS)),
      () => utf8(rng.several(WORDS, 1, 3, " ")),
      () => fromHex(rng.pick(ENCODED_SAMPLES)[1]),
      () => Uint8Array.from(Array.from({ length: 1 + rng.int(9) }, () => rng.int(256))),
      () => utf16(rng.pick(UNICODE_SAMPLES) + "x", rng.chance(0.5)),
      () => bytesOf()
    ])();
  const word = (charset = rng.pick(CHARSETS), bytes = sampleBytes()): string => {
    const mode = rng.pick(["B", "b", "Q", "q", "B", "Q"]);
    const text = mode.toUpperCase() === "B" ? b64(bytes) : qEncode(bytes);
    return `=?${charset}?${mode}?${text}?=`;
  };
  /** One text split over adjacent words, as a long subject is. */
  const splitWords = (): string => {
    const [charset, sample] = rng.chance(0.5) ? rng.pick(ENCODED_SAMPLES) : ["utf-8", hex(utf8(rng.pick(UNICODE_SAMPLES).repeat(3)))];
    const bytes = fromHex(sample);
    const cut = rng.int(bytes.length + 1);
    const second = rng.chance(0.2) ? rng.pick(CHARSETS) : rng.chance(0.3) ? charset.toUpperCase() : charset;
    return word(charset, bytes.subarray(0, cut)) + rng.pick(["", " ", "  ", "\t", "\r\n ", "\u00a0"]) + word(second, bytes.subarray(cut));
  };
  const brokenWord = (): string =>
    rng.pick([
      "=?utf-8?Q?broken=ZZ?=", "=?utf-8?B?!!!?=", "=?utf-8?X?abc?=", "=?utf-8?Q?abc", "=?utf-8?Q?a?b?=", "=??Q?abc?=",
      "=?utf-8??abc?=", "=?utf-8?Q??=", "=?utf-8?B??=", "=?utf-8?B?QQ?=", "=?utf-8?B?Q?=", "=?utf-8?B?QUJD=RA==?=",
      "=?utf-8?B?QU JD?=", "=?utf-8?B?QUJ\u3042D?=", "=?utf-8?B?QUJ\u013dDRA?=", "=?utf-8?b?QUJD_-8?=", "=?utf-8?Q?=\t41?=",
      "=?utf-8?Q?= 4?=", "=?utf-8?Q?a=?=", "=?utf-8?Q?a=4?=", "=?utf-8?Q?\u00e9=C3=A9?=", "=?utf-8?Q?a_b\u3000c?=",
      "=?utf 8?Q?abc?=", "=?utf-8?QQ?abc?=", "=?=?utf-8?Q?abc?=", "=?utf-8?Q?=?utf-8?Q?abc?=?=", "?=", "=?", "=", "?",
      "=?utf-8?Q?abc?", "=?utf-8?q?=41=4a=4A=4g?=", "=?UTF-16?B?/v8AaABp?=", "=?utf-16?B?aABpAA==?=", "=?utf-16?Q?=00h=00i?="
    ]);
  const piece = (): string =>
    rng.pick([
      () => word(),
      () => word(),
      () => splitWords(),
      () => brokenWord(),
      () => rng.pick(WORDS),
      () => rng.pick(UNICODE_SAMPLES),
      () => rng.pick(WHITESPACE),
      () => rng.pick(WHITESPACE),
      () => JOIN_MARK,
      () => "?=" + JOIN_MARK + "=?utf-8?Q?",
      () => rng.pick(["=", "?", "=?", "?=", "<", ">", ",", ";", ":", "\"", "(", ")"])
    ])();

  const decodeWordsInputs = [
    "=?UTF-8?B?7JWI64WV?=", "=?utf-8?q?caf=C3=A9_au_lait?=", "=?EUC-KR?B?x9Gx2w==?=", "=?ISO-2022-JP?B?GyRCRnxLXDhsGyhC?=",
    "=?Shift_JIS?B?k/qWe4zq?=", "=?GB18030?B?1tDOxA==?=", "=?windows-1251?B?7/Do4uXy?=", "=?iso-8859-1?Q?R=E9sum=E9?=",
    "=?UTF-8?Q?=ED=95?= =?UTF-8?Q?=9C?=", "=?UTF-8?B?7ZU=?= =?UTF-8?B?nA==?=", "=?UTF-8?Q?a?=  =?UTF-8?Q?b?= c",
    "=?x-unknown?Q?abc?=", "=?utf-8?Q?broken=ZZ?=", "plain =?not a word", "", "plain", "=?utf-8?B?7ZU=?= =?utf8?B?nA==?=",
    "=?utf-8?B?7ZU=?= =?iso-8859-1?B?nA==?=", "=?utf-8?B?7ZU=?= =?utf-8?Q?=9C?=", "=?utf-8?Q?a?= b =?utf-8?Q?c?=",
    "=?utf-8?Q?a?=\r\n =?utf-8?Q?b?=", "=?utf-8?Q?a?==?utf-8?Q?b?=", "=?utf-8?Q?a?= =?utf-8?Q?b?= =?utf-8?Q?c?= =?utf-8?Q?d?=",
    "=?utf-8?Q?a?= =?utf-8?B?Yg==?= =?utf-8?Q?c?=", `a${JOIN_MARK}b`, `a?=${JOIN_MARK}=?utf-8?Q?b`, `=${JOIN_MARK}?utf-8?Q?abc?=`,
    `${JOIN_MARK}${JOIN_MARK.slice(2)}`, `=?utf-8?Q?a?=${JOIN_MARK}=?utf-8?Q?b?=`, "=?utf-8*ko?B?7JWI?=", "=?*?Q?abc?=",
    "=?utf-8?Q?\u{1F600}?=", "=?utf-8?B?\u{1F600}QUJD?=", "=?utf-8?B?QUJD\u{1F600}RA==?=", "=?us-ascii?Q?=80=9F?=",
    "=?iso-8859-1?Q?=80=9F?=", "=?utf-8?Q?=EF=BB=BFbom?=", "=?unicode-1-1-utf-8?Q?=EF=BB=BFbom?=", "=?utf-16?Q?=FE=FF=00h?=",
    ...Array.from({ length: 900 }, () => Array.from({ length: 1 + rng.int(6) }, piece).join(rng.pick(["", "", " ", "  "])))
  ];

  const names = ["filename", "name", "charset", "boundary", "format", "delsp", "x", "a", "0", "1", "42", "__proto__", "constructor", "toString", "é", ""];
  const starred = (name: string) => name + rng.pick(["", "*", "*0", "*1", "*2", "*0*", "*1*", "*00*", "*10", "**", "*a", "*1*2", "*999999999999999999999", "*" + "9".repeat(400) + "*"]);
  const percent = (bytes: Uint8Array) => [...bytes].map((byte) => (rng.chance(0.2) && byte > 0x2f && byte < 0x7b ? String.fromCharCode(byte) : "%" + byte.toString(16).toUpperCase().padStart(2, "0"))).join("");
  const value = (): string =>
    rng.pick([
      () => rng.pick(WORDS),
      () => `"${rng.several([...WORDS, " ", ";", "=", "\\\"", "\\\\", ","], 1, 4)}"`,
      () => `${rng.pick(["utf-8", "UTF-8", "iso-8859-1", "euc-kr", "", "x-unknown", "us-ascii", "utf-16"])}'${rng.pick(["", "en", "ko"])}'${percent(sampleBytes())}`,
      () => percent(sampleBytes()),
      () => word(),
      () => rng.pick(UNICODE_SAMPLES),
      () => rng.several(["a", "'", "%", "=", "?", "_", " ", "\t", "\u00a0", "\u2028", "%4", "%zz", "%E3%81%82", "\\", "\""], 0, 6),
      () => ""
    ])();
  const parameter = (): string =>
    rng.pick([
      () => `${starred(rng.pick(names))}=${value()}`,
      () => `${starred(rng.pick(names))}=${value()}`,
      () => `${rng.pick(names).toUpperCase()} = ${value()} `,
      () => rng.pick(names),
      () => "",
      () => "=",
      () => `=${value()}`,
      () => `${rng.pick(names)}=${value()}=${value()}`
    ])();
  const continuation = (): string => {
    const name = rng.pick(["filename", "name", "x"]);
    const bytes = utf8(rng.pick(UNICODE_SAMPLES) + rng.several(WORDS, 1, 3, " ") + ".pdf");
    const cuts = [0, ...Array.from({ length: rng.int(3) }, () => rng.int(bytes.length + 1)).sort((a, b) => a - b), bytes.length];
    const parts = cuts.slice(1).map((end, i) => {
      const chunk = bytes.subarray(cuts[i], end);
      const tagged = i === 0 && rng.chance(0.8);
      const encoded = tagged || rng.chance(0.5);
      return `${name}*${i}${encoded ? "*" : ""}=${tagged ? rng.pick(["UTF-8", "utf-8", "iso-8859-1"]) + "'" + rng.pick(["", "en"]) + "'" : ""}${encoded ? percent(chunk) : `"${Buffer.from(chunk).toString("latin1").replace(/["\\]/g, "\\$&")}"`}`;
    });
    if (rng.chance(0.3)) parts.reverse();
    if (rng.chance(0.2)) parts.push(parts[0]);
    return parts.join(rng.pick(["; ", ";", " ;\t"]));
  };
  const headerValue = (): string =>
    [rng.pick(["text/plain", "attachment", "inline", "multipart/mixed", "", " TEXT/HTML ", "a/b", "\"quoted\"", "a\\;b", "é"])]
      .concat(Array.from({ length: rng.int(5) }, () => (rng.chance(0.25) ? continuation() : parameter())))
      .join(rng.pick(["; ", ";", " ; ", ";\t", ";;"])) + rng.pick(["", "", ";", " ", "\"", "\\"]);
  const parseHeaderValueInputs = [
    'text/plain; charset="UTF-8"; format=flowed',
    "attachment; filename*0*=UTF-8''%EB%B3%B4%EA%B3%A0; filename*1*=%EC%84%9C; filename*2=.pdf",
    "attachment; filename*=iso-8859-1''caf%E9.txt", "inline; foo", "", ";", "=", "a=b", "a; b; c=d", "a; b=c; b=d; B=e",
    'x; a="1;2"; b="3\\";4"; c=5', "x; a=\"unterminated; b=c", "x; a='single; b=c", "x; __proto__=1; constructor=2; a=3",
    "x; 2=b; 1=a; z=c; 0=d", "x; a*=utf-8''%41; a=plain", "x; a=plain; a*=utf-8''%41", "x; a*0=one; a*0*=utf-8''two",
    "x; a*1=b; a*0=a; a*2=c; a*10=k; a*9=j", "x; a*=''; b*='; c*='''; d*=x'y'z'w", "x; a*=utf-8'en'%C3%A9 %C3%A9_=?",
    "x; a*=utf-8''line\nbreak", "x; a*=utf-8''\u2028", "x; a*=bad charset''%41", "x; a*=utf-8''%\u00e9", "x; *=utf-8''%41; **=b; *0=c",
    "x; filename==?utf-8?B?7ZWc6riALmRvY3g=?=", "x; a*=utf-16''%FE%FF%00h%00i", "x; a*=iso-2022-jp''%1B%24%42%46%7C%1B%28%42",
    ...Array.from({ length: 800 }, headerValue)
  ];

  const lineParts = ["Subject", "subject", "X-Custom", "To", "", " ", ":", "::", "value", "a: b", "\r\n ", "\r\n\t", "\n ", "\r ", "\r", "\n", "\r\n", "  ", "\t", "\u00a0", "\u2028", "\ufeff", "é", "한글", "À"];
  const splitHeaderLineInputs = [
    "Subject: one\r\n two", "", ":", "a", "a:", ":b", " a : b ", "a:b:c", "A-B:  c  \r\n", "Subject:\r\n folded\r\n\tmore", "\u00a0Key\u00a0: v\u00a0",
    "key: v\u2028w", "KEY\u2028: v", "\ufeffk: v", "k\r: v", "k\n: v", "ÀÉ: Ö",
    ...Array.from({ length: 500 }, () => rng.several(lineParts, 1, 7))
  ];

  const latin1 = (text: string) => Buffer.from(text, "utf8").toString("latin1");
  const rawOctetsInputs = [
    latin1("한글.pdf"), "caf\xe9", "보고서", "", "plain", latin1("é"), latin1("\ufeffbom"), latin1("😀") + "x", latin1("é") + "\xff",
    latin1("한").slice(0, 2), "\x80", "\xc3\xa9\u0100", "\xed\xa0\x80", "\xc0\xaf", latin1("\u0000\u007f\u0080"),
    ...Array.from({ length: 300 }, () => Array.from({ length: 1 + rng.int(4) }, () => rng.pick([() => latin1(rng.pick(UNICODE_SAMPLES)), () => rng.pick(WORDS), () => String.fromCharCode(0x80 + rng.int(128)), () => rng.pick(["\u0100", "한"])])()).join(""))
  ];

  const idParts = ["<a@x>", "<b@y>", "<c@z>", "<a@x>", "< spaced@x >", "<>", "<<nested@x>>", "<un@closed", "bare@x", "(comment <in@comment>)", "<é@한>", "<\u00a0@x>", "<a\u2028b@x>", `<${"l".repeat(510)}>`, `<${"l".repeat(511)}>`, `<${"😀".repeat(255)}>`, `<${"😀".repeat(256)}>`, "<a>b<c>", ">", "<", ",", " ", "\r\n ", "\t"];
  const messageIdListInputs = [
    "<a@x> <b@y>\r\n <c@z>", "(old) <a@x> junk <a@x> <b@y>", "", "no ids here",
    Array.from({ length: 80 }, (_, i) => `<m${i}@x>`).join(" "), `<${"a".repeat(600)}@x> <b@y>`,
    Array.from({ length: 120 }, (_, i) => `<m${i % 60}@x>`).join(""),
    ...Array.from({ length: 300 }, () => rng.several(idParts, 0, 8, rng.pick(["", " ", ", "])))
  ];

  writeFixture("header", {
    decodeWords: run("decodeWords", decodeWordsInputs, decodeWords),
    parseHeaderValue: run("parseHeaderValue", parseHeaderValueInputs, parseHeaderValue),
    splitHeaderLine: run("splitHeaderLine", splitHeaderLineInputs, splitHeaderLine),
    rawOctetsAsUtf8: run("rawOctetsAsUtf8", rawOctetsInputs, rawOctetsAsUtf8),
    parseMessageIdList: run("parseMessageIdList", messageIdListInputs, parseMessageIdList),
    ensureMessageIdFormat: run(
      "ensureMessageIdFormat",
      [
        "abc@x", "<abc@x>", "", "<", ">", "<>", "a>", "<a", " ", "한글",
        `<${"x".repeat(510)}>`, `<${"x".repeat(511)}>`, "x".repeat(510), "x".repeat(511), `<${"😀".repeat(255)}>`, `<${"😀".repeat(256)}>`
      ],
      (id) => ensureMessageIdFormat(id) ?? null
    )
  });
};

// ---------------------------------------------------------------------------
// address
// ---------------------------------------------------------------------------

const PUNYCODE_LABELS = [
  "xn--9n2bp8q", "xn--bcher-kva", "XN--BCHER-KVA", "xn--", "xn--a", "xn--zzzzzz", "xn--80ak6aa92e", "xn--ls8h", "xn--maana-pta",
  "xn--p1ai", "xn--11b4c3d", "xn--bcher-kva/foo", "xn--bcher-kva:80", "xn--bcher-kva?x", "xn--bcher-kva#x", "xn--bcher-kva\\x",
  "xn--bcher-kva%41", "xn--bcher-kva%2e", "xn--bcher-kvaA", "xn--bcher-kva_", "xn--bcher-kva-", "xn--BCHER-kva", "xn--bcher-kvaé",
  "xn--bcher-kva\u3002com", "xn--bcher-kva\uff0exn--p1ai", "xn--bcher--kva", "xn--zca", "xn--ss-", "xn--fa-hia", "xn--1ch", "xn--53h",
  "xn--mgbh0fb", "xn--fiqs8s", "xn--3e0b707e", "xn--nxasmq6b", "xn--deba0ad", "xn--hlcj6aya9esc7a", "xn--0ugb", "xn--1ug", "xn--a-ecp",
  "xn--caf-dma", "xn--CAF-dma", "xn--caf-dma\u00ad", "xn--caf-dma\u200d", "xn--caf-dma\u00a0", "xn--caf-dma[", "xn--caf-dma%", "xn--0",
  "xn--12", "xn--tma", "xn--ls8haa", "xn--i-7iq", "xn--y9a", "xn--gdh", "xn--a" + "a".repeat(60), "xn--bcher-kva" + "a".repeat(80),
  "xn--mañana", "xn--e1afmkfd", "xn--80akhbyknj4f", "xn--kgbechtv", "xn--0zwm56d", "xn--9t4b11yi5a", "xn--zckzah", "xn--jxalpdlp",
  "xn--4ca", "xn--bca", "xn--ea", "xn--wgbh1c", "xn--mgberp4a5d4ar", "xn--90a3ac", "xn--d1acj3b", "xn--j1amh", "xn--strae-oqa"
];

/** Characters an internationalized domain label is made of, valid there or not. */
const IDN_PIECES = [
  ..."abcxyz019", "-", "-", "é", "ü", "ß", "ç", "ñ", "ø", "α", "β", "σ", "ς", "Σ", "п", "р", "и", "ё", "ש", "ל", "ם", "م", "ر", "ب", "٠", "۵",
  "한", "글", "日", "本", "語", "か", "カ", "ー", "中", "😀", "❤", "\u0301", "\u0308", "\u200d", "\u200c", "·", "・", "ſ", "İ", "ı", "ａ", "Ａ", "²", "™", "℃",
  "\u00ad", "\u3002", "\u200e", "\ufe0f", "क", "्", "ष", "ก", "า", "ง", "A", "Z", "\u05f3", "\u0660", "\u06f0", "\u1e9e", "\u0131", "\u1806", "\u2060"
];

/** A label in its punycode form, whatever the label is. */
const punycodeLabel = (rng: Random): string => {
  const label = rng.several(IDN_PIECES, 1, 7);
  try {
    return "xn--" + punycode.encode(label);
  } catch {
    return "xn--" + label;
  }
};

const addressInputs = (rng: Random): string[] => {
  const encodedList = (text: string) => `=?UTF-8?B?${Buffer.from(text).toString("base64")}?=`;
  const locals = ["a", "bob", "john.doe", "user+tag", "\"quoted local\"", "x_y%z", "한", "é", "", "a@b", "\"a@b\""];
  const domains = ["example.com", "x.org", "sub.example.co.kr", "localhost", "[127.0.0.1]", "", "EXAMPLE.COM", "x", "한글.kr", ...PUNYCODE_LABELS.slice(0, 20).map((label) => `${label}.com`)];
  const names = ["Bob", "Doe, John", "\"Doe, John\"", "\"with \\\"escape\\\"\"", "=?UTF-8?B?7ZmN6ri464+Z?=", "=?utf-8?q?caf=C3=A9?= =?utf-8?q?_x?=", "한글 이름", "", "(comment)", "a (comment) b", "\"user@evil.example\"", "user@inner.example", "<", ">", ":", "\"unterminated", "O'Brien", "x;y", "Très Bien", "\u00a0nbsp\u00a0", "tab\tname", "new\nline", "\"\"", "\"a\"b", "a\"b\"c", "@", "@@", "a@", "@b", "re: thing", "=?utf-8?B?QQ==?= =?utf-8?B?Qg==?=", encodedList("A <a@x.org>, b@y.org")];
  const address = () => `${rng.pick(locals)}@${rng.pick(domains)}`;
  const mailbox = (): string =>
    rng.pick([
      () => address(),
      () => `${rng.pick(names)} <${address()}>`,
      () => `${rng.pick(names)}<${address()}>`,
      () => `<${address()}> ${rng.pick(names)}`,
      () => `${address()} (${rng.pick(names)})`,
      () => `${rng.pick(names)} ${address()}`,
      () => `${rng.pick(names)}`,
      () => `<${address()}`,
      () => `${address()}>`,
      () => `< ${rng.pick(names)} <${address()}> >`,
      () => `${rng.pick(names)} <${address()}> <${address()}>`,
      () => `"${address()}" ${address()}`,
      () => `${rng.pick(["Team", "undisclosed-recipients", "\"G: x\"", "", "=?utf-8?B?VGVhbQ==?="])}: ${Array.from({ length: rng.int(4) }, () => (rng.chance(0.15) ? `inner: ${address()};` : mailbox())).join(", ")};`,
      () => encodedList(Array.from({ length: 1 + rng.int(3) }, address).join(rng.pick([", ", "; ", " "])))
    ])();
  return [
    "a@example.com, Bob <bob@example.com>", '"Doe, John" <john@example.org>', "=?UTF-8?B?7ZmN6ri464+Z?= <hong@example.kr>",
    'Team: alice@example.org, "Bob B" <bob@example.org>;', "x@xn--9n2bp8q.com", encodedList("A <a@x.org>, b@y.org"),
    '"user@evil.example" other@example.com', "a@b.c (Alice)", "undisclosed-recipients:;", "a@x.org; b@x.org", "", " ", ",", ";", ":",
    "<>", "<", ">", "\"", "(", ")", "@", "a@", "@b", "a@b@c", "a b c", "a@b c@d", "john john@x.com smith", "<a@b>,<c@d>", "a@b,,c@d",
    "g1: g2: g3: a@b;;;", "g:".repeat(60) + "a@b.c" + ";".repeat(60), "g: a@b, h: c@d; e@f;", "A: ;, B: ;", "a@b (one) (two)",
    "(only comment)", "\"only quoted\"", "\"a\"\"b\" <c@d>", "\"a\" \"b\" <c@d>", "a\"b\"c <d@e>", "a(b)c <d@e>", "<a@b>c", "a <b@c> d <e@f>",
    "a <b@c>d", "\"quoted\"<a@b>", "<a@b>\"quoted\"", "a\\@b <c@d>", "\"a\\\"b\\\\c\" <d@e>", "\"a\\\nb\" <c@d>", "a\r\n b <c@d>", "a\tb <c@d>",
    "\u0000a\u0001b <c\u0002@d>", "\u007f <a@b>", "a < b <c@d>", "x < y@z", "name <not an address>", "name <a b@c d>", "<a@b> <c@d> <e@f>",
    "a@b <c@d>", "Bob <bob@xn--bcher-kva.example>", "bob@XN--BCHER-KVA.example", "a@xn--x@b.com", "a@b.xn--p1ai.", "a@.xn--p1ai",
    "=?utf-8?B?7ZmN?= =?utf-8?B?6ri4?=", "=?utf-8?B?7ZmN?= x", "=?utf-8?Q?a_b?=", "=?utf-8?B?YUBiLmM=?=: x@y;", "=?utf-8?B?Zzogem9l?=",
    encodedList(encodedList("deep@x.org")), encodedList("g: a@b, c@d;"), encodedList(""), encodedList(" "), encodedList("=?"),
    "mailto:a@b", "a@b.c.", ".a@b.c", "a..b@c", "\"a@b\"@c.d", "user@[IPv6:::1]", "a+b@c", "\"\"@x", "@x.com, y@x.com", "a@b;c@d;;e@f",
    "Smith, John <j@s.com>", "John Smith j@s.com (work)", "j@s.com John Smith", "John <j@s.com> Smith", "😀 <e@m.oji>", "a@😀.com",
    "John\u00a0Smith\u00a0<j@s.com>", "\u3000a@b\u3000", "\ufeffa@b", "a@b\u2028c@d", "tel:+123 <a@b>", "weird:colon@x.com", "a@b:c@d",
    Array.from({ length: 300 }, (_, i) => `User ${i} <u${i}@example.com>`).join(", "),
    ...PUNYCODE_LABELS.map((label) => `a@${label}`),
    ...PUNYCODE_LABELS.map((label) => `N <a@x.${label}.y>`),
    ...Array.from({ length: 900 }, () => Array.from({ length: 1 + rng.int(4) }, mailbox).join(rng.pick([", ", ",", "; ", " ", ", , "]))),
    ...Array.from({ length: 500 }, () => `a@${Array.from({ length: 1 + rng.int(3) }, () => (rng.chance(0.8) ? punycodeLabel(rng) : rng.pick(["com", "example", "한글", "x"]))).join(".")}`)
  ];
};

const addressFixture = () => {
  writeFixture("address", { parseAddressHeader: run("parseAddressHeader", addressInputs(random(303)), parseAddressHeader) });
};

// ---------------------------------------------------------------------------
// mime/text
// ---------------------------------------------------------------------------

const TEXT_PIECES = [
  "http://a.example/b", "https://x.org/?a=1&b=2", "git://repo.example/x.git", "mailto:a@b.example", "www.x.org", "www.", "http://",
  "https://", "xhttp://no.example", "a.b+c@d-e.example", "a@nodot", "user_name%x@sub.domain.example", "@", "a@b.c@d.e", "x@y.z.",
  "HTTP://UPPER.example", "WWW.upper.example", "WWW.x@y.example", "ftp://not.linked", "http://a.example/\"quoted\"", "http://a/<b>",
  "(http://a.example/b).", "see http://a.example/b...", "http://[::1]:80/", "http://한글.kr/경로", "é@é.example", "a@b.example,",
  "a.@b.example", ".a@b.example", "a@-b-.example", "a@b..example", "<", ">", "&", "\"", "'", "`", "&amp;", "<b>bold</b>", ".", ",", ";",
  ":", "!", "?", ")", "]", "}", "(", "http://a.example/x?y=(z)", "http://a.example/x.,;:!?')]}>"
];

const mimeTextFixture = () => {
  const rng = random(404);
  const piece = () =>
    rng.pick([
      () => rng.pick(TEXT_PIECES), () => rng.pick(TEXT_PIECES), () => rng.pick(WORDS), () => rng.pick(WORDS), () => rng.pick(WHITESPACE),
      () => rng.pick(WHITESPACE), () => rng.pick(UNICODE_SAMPLES), () => rng.pick(["\n\n", "\n\n\n", " \n", "\t\n", "\n ", " \r\n \r\n", "\r\r", " \u2028"])
    ])();
  const textInputs = [
    "see http://a.example/b... and (www.x.org).", "mail me at a.b+c@d-e.example, ok", "xhttp://no.example a@nodot", "", " ", "\n", "\n\n",
    "a\n\nb", "a\n\n\n\nb", "a \nb", "a\t \n b \t", " a ", "a\r\nb\r\n\r\nc", "a\rb", "line one  \nline two\t\t\n\n  para  ", "&<>\"'`",
    "tab\tinside", "trailing space ", "\u00a0nbsp\u00a0", "a\u2028b", "a \u2028b", "😀 http://e.mo/ji😀 x", "http://a.example/b\nhttp://c.example/d",
    ...TEXT_PIECES,
    ...Array.from({ length: 1000 }, () => Array.from({ length: 1 + rng.int(8) }, piece).join(rng.pick(["", " ", " ", "\n"])))
  ];
  const entityInputs = ["", "plain", "&<>\"'`", "é한😀\u0000\u007f\u0080\ufffd\u{10FFFF}", ...Array.from({ length: 200 }, () => rng.several([...UNICODE_SAMPLES, ...WORDS, "&", "<", ">", "\"", "'", "`", " "], 1, 6))];

  const addressRng = random(405);
  const addressLists = addressInputs(addressRng).filter((_, i) => i % 4 === 0);
  const addressHtml = run("addressesToHtml", addressLists, (input) => addressesToHtml(parseAddressHeader(input).value));

  // Embedded headers are described by the header values they come from.
  interface HeaderSpec {
    from?: string;
    subject?: string;
    date?: number;
    to?: string[];
    cc?: string[];
    bcc?: string[];
  }
  const someAddress = () => rng.pick(addressLists);
  const specs: HeaderSpec[] = [
    {},
    { from: "Inner <inner@x.org>", subject: "Inner subject", date: 1793005200000 },
    { subject: "<b>&amp; é 한글 😀" },
    { subject: "" },
    { date: 0 },
    { date: -1 },
    { date: 8.64e15 },
    { date: -8.64e15 },
    { date: -62198755200000 },
    { date: 253402300800000 },
    { to: ["a@x.org"], cc: ["b@x.org", "c@x.org"], bcc: [] },
    { to: ["a@x.org", "Team: b@x.org, c@x.org;"], from: "" },
    ...Array.from({ length: 150 }, () => {
      const spec: HeaderSpec = {};
      if (rng.chance(0.6)) spec.from = someAddress();
      if (rng.chance(0.6)) spec.subject = rng.several([...WORDS, ...UNICODE_SAMPLES, "<", "&", " "], 0, 5, " ");
      if (rng.chance(0.6)) spec.date = Math.floor((rng.next() - 0.5) * 2 * 8.64e15 * (rng.chance(0.8) ? 0.0002 : 1));
      if (rng.chance(0.5)) spec.to = Array.from({ length: 1 + rng.int(3) }, someAddress);
      if (rng.chance(0.3)) spec.cc = Array.from({ length: 1 + rng.int(2) }, someAddress);
      if (rng.chance(0.2)) spec.bcc = Array.from({ length: 1 + rng.int(2) }, someAddress);
      return spec;
    })
  ];
  const headersOf = (spec: HeaderSpec): EmbeddedHeaders => {
    const list = (values?: string[]) => {
      if (!values?.length) return undefined;
      const parsed = values.map(parseAddressHeader);
      return parsed.length === 1 ? parsed[0] : parsed;
    };
    return {
      from: spec.from === undefined ? undefined : parseAddressHeader(spec.from),
      subject: spec.subject,
      date: spec.date === undefined ? undefined : new Date(spec.date),
      to: list(spec.to),
      cc: list(spec.cc),
      bcc: list(spec.bcc)
    };
  };

  writeFixture("mime-text", {
    textToHtml: run("textToHtml", textInputs, textToHtml),
    encodeEntities: run("encodeEntities", entityInputs, encodeEntities),
    addressesToHtml: addressHtml,
    embeddedHeaders: run("embeddedHeaders", specs, (spec) => [embeddedHeadersAsText(headersOf(spec)), embeddedHeadersAsHtml(headersOf(spec))])
  });
};

// ---------------------------------------------------------------------------
// transfer decoder
// ---------------------------------------------------------------------------

const CHUNK_SIZES = [1, 2, 3, 7, 64, 1000, 65536];

const transferDecoderFixture = () => {
  const rng = random(505);
  const decode = (encoding: string, input: Uint8Array, size: number) => {
    const decoder = createTransferDecoder(encoding);
    const out: Buffer[] = [];
    for (let i = 0; i < input.length; i += size) out.push(Buffer.from(decoder.write(Buffer.from(input.subarray(i, i + size)))));
    out.push(Buffer.from(decoder.end()));
    return Buffer.concat(out);
  };
  const randomBytes = (length: number) => Uint8Array.from(Array.from({ length }, () => rng.int(256)));
  const latin = (text: string) => Uint8Array.from(Buffer.from(text, "latin1"));
  const wrap = (text: string, width: number, eol: string) => text.replace(new RegExp(`.{${width}}`, "g"), `$&${eol}`);
  const qp = (bytes: Uint8Array) => {
    let line = "";
    let out = "";
    for (const byte of bytes) {
      const literal = byte >= 0x21 && byte <= 0x7e && byte !== 0x3d && rng.chance(0.7);
      const next = literal ? String.fromCharCode(byte) : byte === 0x20 && rng.chance(0.5) ? " " : "=" + byte.toString(16).toUpperCase().padStart(2, "0");
      if (line.length + next.length > 72) {
        out += line + "=" + rng.pick(["\r\n", "\n"]);
        line = "";
      }
      line += next;
    }
    return out + line;
  };
  const base64Noise = ["QUJD", "QUJ", "QU", "Q", "=", "==", "===", "\r\n", "\n", " ", "!", "-", "_", "*", "QUI=", "QQ==", "\u00ff", "\u0000", "+/+/", "A", "AA"];
  const qpNoise = ["=41", "=4", "=", "=\r\n", "=\n", "= \r\n", "=ZZ", "=c3=a9", "=C3=A9", " ", "\t", " \r\n", "\t\n", "\r\n", "\n", "\r", "a", "b c", "\u00e9", "\u0000", "==", "=3D", "=\r", "=0D=0A", "x=", "=A"];

  const text = (bytes: Uint8Array) => Buffer.from(bytes).toString("latin1");
  const inputs: [string, Raw][] = [
    ["base64", ["QU\r\nJD !REVG"]], ["base64", ["QQ=="]], ["base64", ["QUI=QUI="]], ["base64", ["QUI"]], ["base64", [""]],
    ["base64", [wrap(Buffer.from(randomBytes(1000)).toString("base64"), 76, "\r\n")]],
    ["base64", [wrap(Buffer.from(randomBytes(4000)).toString("base64"), 60, "\n")]],
    ["base64", [["QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVowMTIzNDU2Nzg5QUJDREVGR0hJSktMTU5PUFFSU1RVVldY\r\n", 5000], "QUI="]],
    ["base64", [["QUJD", 100_000], "Q", ["=", 10], ["QUJDRA", 3]]],
    ["quoted-printable", ["caf=C3=A9 au=\r\n lait\r\n"]], ["quoted-printable", ["a  b   \r\nc \t\nd  "]], ["quoted-printable", ["=ZZ x=4"]],
    ["quoted-printable", ["trailing="]], ["quoted-printable", [text(concat(latin("x "), utf8("한"), latin("=41\r\n")))]],
    ["quoted-printable", [["=41", 30000], "=42"]], ["quoted-printable", [["a", 70000], "="]],
    ["quoted-printable", [["a", 65535], "=41", ["b", 70000], " \t \r\n"]], ["quoted-printable", [[" ", 70000], "x"]],
    ["quoted-printable", ["x", [" ", 70000]]], ["quoted-printable", ["x", [" ", 70000], "\r\ny"]],
    ["quoted-printable", [["x", 65536], "=\r\ny"]], ["quoted-printable", [["x", 65537], "=4"]], ["quoted-printable", [["x", 65535], "=41"]],
    ["quoted-printable", [["x", 65534], "=41"]], ["quoted-printable", [["x", 65536], "\r"]], ["quoted-printable", [["x", 65536], "\r\r\n"]],
    ["quoted-printable", [["=C3=A9 ", 20000], "=\r\n", ["tail ", 3]]], ["quoted-printable", [["line =41 \t\r\n", 10000]]],
    ["quoted-printable", [qp(randomBytes(5000))]], ["quoted-printable", [qp(utf8("한글 테스트 ".repeat(200)))]], ["quoted-printable", [""]],
    ["8bit", ["abc\r\n"]], ["7bit", [text(randomBytes(300))]], ["binary", [text(randomBytes(300))]], ["", ["x"]], ["BASE64", ["QUJD"]], ["x-uuencode", ["begin 644 x\n"]],
    ...Array.from({ length: 350 }, (): [string, Raw] => ["base64", [rng.several(base64Noise, 0, 14)]]),
    ...Array.from({ length: 80 }, (): [string, Raw] => ["base64", [wrap(Buffer.from(randomBytes(rng.int(200))).toString("base64"), 1 + rng.int(80), rng.pick(["\r\n", "\n", " "])) + rng.pick(["", "=", "\r\n", "QQ"])]]),
    ...Array.from({ length: 450 }, (): [string, Raw] => ["quoted-printable", [rng.several(qpNoise, 0, 16)]]),
    ...Array.from({ length: 60 }, (): [string, Raw] => ["quoted-printable", [qp(randomBytes(rng.int(400)))]])
  ];
  const cases = inputs.map(([encoding, raw]) => {
    const input = rawBytes(raw);
    // A long unterminated line fed a byte at a time is copied once per byte.
    const sizes = input.length > 20_000 ? CHUNK_SIZES.filter((size) => size >= 64) : CHUNK_SIZES;
    const outputs = sizes.map((size) => decode(encoding, input, size));
    if (outputs.some((output) => !output.equals(outputs[0]))) throw new Error(`transfer decoder: chunking changes ${encoding} ${hex(input).slice(0, 80)}`);
    const output = outputs[0];
    return [encoding, raw, output.length > 4000 ? { sha256: sha256(output), length: output.length } : hex(output), sizes];
  });
  writeFixture("transfer-decoder", { cases });
};

// ---------------------------------------------------------------------------
// entities, plain text, getText
// ---------------------------------------------------------------------------

const ENTITY_PIECES = [
  "&amp;", "&lt;", "&gt;", "&quot;", "&apos;", "&nbsp;", "&copy;", "&hellip;", "&euro;", "&amp", "&ampx;", "&copy=2", "&lt3", "&frac12x",
  "&#65;", "&#x42;", "&#X43;", "&#68", "&#x1F600;", "&#0;", "&#150;", "&#xD800;", "&#99999999;", "&;", "&", "&#;", "&#x;", "&unknown;",
  "&check;", "&notit;", "&not;", "&notin;", "&notin", "&AMP;", "&Amp;", "&aMp", "&QUOT", "&#128;", "&#159;", "&#x80;", "&#xDFFF;",
  "&#x10FFFF;", "&#x110000;", "&#1114111;", "&#1114112;", "&#x0;", "&#00065;", "&#x00041", "&#xffffffffffffffffffffffff;",
  "&#" + "9".repeat(400) + ";", "&#x" + "f".repeat(300), "&sup1;", "&sup1x", "&there4;", "&there4", "&Aacute", "&aacutE", "&THORNy", "&yen1",
  "&uml;", "&um", "&g;", "&gt", "&gT;", "&GT", "&LT;", "&zwnj;", "&lrm;", "&ensp;", "&thinsp;", "&#9;", "&#10;", "&#13;", "&#32;", "&#160;",
  "&#8203;", "&&amp;&", "&amp;amp;", "&#38;#38;", "&#x26;lt;", "&é;", "&한;", "& amp;", "&a mp;", "&#x 41;", "&# 65;", "&#-1;", "&#+1;", "&#1e3;",
  "&#x1g;", "&frac34", "&frac3", "&ordf1", "&micro;g", "&microg", "&para=1", "&sect", "&section;", "&times;", "&timesb;", "&divide"
];

const HTML_TAGS = ["p", "div", "span", "b", "i", "a", "ul", "ol", "li", "blockquote", "pre", "h1", "h2", "h3", "h4", "h5", "h6", "table", "tr", "td", "th", "tbody", "thead", "tfoot", "br", "hr", "img", "wbr", "script", "style", "title", "head", "body", "html", "form", "input", "select", "option", "optgroup", "button", "textarea", "datalist", "output", "dl", "dt", "dd", "rt", "rp", "ruby", "svg", "math", "mi", "desc", "foreignObject", "annotation-xml", "section", "article", "aside", "header", "footer", "main", "nav", "address", "details", "fieldset", "figure", "figcaption", "link", "meta", "area", "base", "col", "embed", "param", "source", "track", "center", "font", "u", "em", "strong", "code", "sup", "x-custom", "o:p", "constructor", "P", "DIV", "Br", "LI", "SCRIPT", "sTyLe"];

const HTML_ATTRIBUTES = [
  "href=\"https://x.org/a?b=1&amp;c=2\"", "href='mailto:a@b.c'", "href=#top", "href=\"\"", "href", "href=\"x y\"", "href=mailto:", "HREF=\"https://UPPER.example\"",
  "href=\"https://x.org/&copy=1&#38;&notit;\"", "src=\"cid:logo@x\"", "src=y.png", "src='data:image/png;base64,AAAA'", "alt=\"Logo &amp; more\"", "alt=''", "alt=pic",
  "start=\"3\"", "start=0", "start=-2", "start=\"1e2\"", "start=abc", "start=\"\"", "start=\" 7 \"", "start=0x10", "start=3.5", "start=9999", "start=3998", "start=\"-Infinity\"", "start=1e21",
  "type=\"a\"", "type=A", "type=i", "type=\"I\"", "type=1", "type=disc", "class=\"x\"", "style=\"color:red\"", "id=a", "a=b=c", "=", "\"", "'", "/", "x=\"unterminated", "data-x='a\"b'",
  "href=\"a\" href=\"b\"", "ALT=upper", "title=\"t\"", "\n", "\t"
];

const htmlInputs = (rng: Random): string[] => {
  const text = () =>
    rng.pick([
      () => rng.several(WORDS, 1, 12, " "), () => rng.several(WORDS, 1, 4, " "), () => rng.pick(ENTITY_PIECES), () => rng.pick(UNICODE_SAMPLES),
      () => rng.pick(WHITESPACE), () => rng.pick(["\n", "  ", " \n ", "\t"]), () => "x".repeat(1 + rng.int(100)), () => rng.several(["word", "한글단어", "😀", "é"], 1, 30, " "),
      () => rng.pick(["https://example.com/a/very/long/path/that/keeps/going/and/going/past/eighty/columns", "a".repeat(79), "a".repeat(80), "a".repeat(81), "😀".repeat(40), "😀".repeat(41)]),
      () => rng.pick(["Straße", "ǆemal", "ŉ", "ΟΔΟΣ οδός", "ﬁne", "i̇stanbul İ", "ΑΣ ας"])
    ])();
  const open = (tag: string) => `<${tag}${Array.from({ length: rng.int(3) }, () => " " + rng.pick(HTML_ATTRIBUTES)).join("")}${rng.pick(["", "", "", " ", "/", " /"])}>`;
  const node = (depth: number): string => {
    if (depth <= 0 || rng.chance(0.35)) return text();
    const tag = rng.pick(HTML_TAGS);
    const inner = Array.from({ length: rng.int(4) }, () => node(depth - 1)).join(rng.pick(["", "", " ", "\n"]));
    return rng.pick([
      () => `${open(tag)}${inner}</${tag}>`, () => `${open(tag)}${inner}</${tag}>`, () => `${open(tag)}${inner}`, () => `${inner}</${tag}>`,
      () => `${open(tag)}${inner}</${rng.pick(HTML_TAGS)}>`, () => `<!--${inner.replace(/--/g, "-")}-->`, () => `${open(tag)}${inner}</${tag} >`,
      () => `${open(tag)}${inner}</ ${tag}>`
    ])();
  };
  const soupPieces = ["<", ">", "</", "/>", "<!", "<!--", "-->", "--", "<![CDATA[", "]]>", "<?", "?>", "<!DOCTYPE html>", "=", "\"", "'", " ", "\n", "<p", "</p", "<a href=", "<script>", "</script>", "</scr", "</style", "<style>", "<title>", "</title>", "</titlex", "</ti", "</s", "</sx", "</stylex>", "<br", "<li>", "<ul>", "<ol type=I start=3999>", "<blockquote>", "</blockquote>", "<pre>", "</pre>", "<svg/>", "<svg><path/></svg>", "<math><mi/></math>", "</br>", "</p>", "</3", "</ >", "</>", "<>", "< p>", "<1>", "<é>", "<p\u00e9>", "<a\u0000b>"];
  const soup = () => Array.from({ length: 1 + rng.int(12) }, () => (rng.chance(0.45) ? rng.pick(soupPieces) : rng.chance(0.3) ? rng.pick(HTML_TAGS) : text())).join("");
  return [
    "<p>First</p><p>Second<br>line</p>text after", "<h1>Title</h1><h4>Small heading</h4><p>body</p>", "<ul><li>one</li><li>two<ul><li>nested</li></ul></li></ul>",
    "<ol start=\"3\"><li>c</li><li>d</li></ol><ol type=\"I\"><li>x</li><li>y</li><li>z</li><li>w</li></ol><ol type=\"a\"><li>p</li></ol>",
    "<p>On Monday someone wrote:</p><blockquote><p>quoted</p><p>more</p></blockquote>",
    "<p>Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.</p>",
    "<p>short https://example.com/a/very/long/path/that/keeps/going/and/going/past/eighty/columns end</p>", "<pre>  a\n    b  c</pre><p>after</p>",
    "<table><tr><td>Name</td><td>Qty</td></tr><tr><td>Apple</td><td>3</td></tr></table>", "<ul><li>one<li>two</ul><p>a<p>b",
    "<style>p{color:red}</style><p>a<!-- hidden -->b<img src=x alt=pic></p><script>var s = '<p>no</p>';</script>",
    "<html><head><title>Subject</title></head><body><p>Body</p></body></html>", "<title>Subject</title><p>Body</p>", "<p>Visit <a href=\"https://example.com\">our site</a>.</p>",
    "<p>a</p><hr><p>b</p>", "<p>  Too   many \n\t spaces  </p>", "<script>if (a</b) {}</scr ipt></script>after", "</p>x</br>y</div>z",
    "<p>&lt;a&gt; &amp; &quot;q&quot; &#169; &#x263A; caf&eacute; AT&T &copy=1</p>", "<p>Verify: <a href=\"https://bit.ly/3abc\">now</a><img src=\"a.png\" alt=\"A\"></p>",
    "<a href=\"https://x.org\">https://x.org</a>", "<a href=\"mailto:a@b.c\">write</a> <a href=\"mailto:a@b.c\"></a>", "<a href=\"#top\">top</a> <a href=\"\">empty</a> <a>none</a>",
    "<a href=\"https://x.org/?a=1&amp;b=2&copy=3&#38;c\">q</a>", "<img src=\"cid:logo@x\" alt=\"Logo\"> <img src=\"y.png\"> <img alt=\"alt\">", "<h1><a href=\"https://x.org/h\">link</a></h1>",
    "text <", "<p>a<!-- open", "<p>a</p><div class=\"x", "", " ", "\n", "plain text", "<", ">", "</", "<!", "<!-", "<!--", "<!-->", "<!--->", "<![CDATA[x]]>y", "<![cdata[x]]>y", "<![CDAT[x]]>y",
    "<!DOCTYPE html><p>x", "<!DOCTYPE html", "<?xml version=\"1.0\"?>x", "<?php", "</3 x>y", "</3 x", "a</>b", "a< b", "a<1b", "a<<b>c", "a<<<", "<p/>x", "<br/>x", "<svg/>x", "<svg><a/>x</svg>y",
    "<math><mi>x</mi><mo/>y</math>z", "<svg><desc><p/>x</desc><p/>y</svg>", "<svg><title/>x</svg>", "<title/>x<p>y", "<script/>x<p>y", "<style/>x", "<script>a</script >b", "<script>a</script\n>b",
    "<script>a</SCRIPT>b", "<script>a</scriptx>b</script>c", "<script>a</scrip>b</script>c", "<script>a</sx>b</script>c", "<script>a</ script>b</script>c", "<style>a</stylé>b</style>c",
    "<title>a</titl\u00e9>b</title>c", "<title>a</ti😀>b</title>c", "<script>a</s😀cript>b</script>c", "<title>a<b>c</b></title>d", "<TITLE>a</TITLE>b", "<script>x</script", "<script>x</",
    "<script>x<", "<style>x", "<p a=b c='d' e=\"f\" g>x", "<p a = b>x", "<p a= 'b c'>x", "<p a=b/>x", "<p a/b>x", "<p / >x", "<a href=x/>y</a>", "<a href=\"a\"b\">c</a>",
    "<a href='https://x.org/'>a</a><a href=\"https://y.org/\">b</a>", "<a href=\"https://x.org\"><b>bold</b> <i>it</i></a>", "<a href=\"x\"><img src=\"y\" alt=\"z\"></a>",
    "<a href=\"x\"> </a>", "<a href=\"x\">\u200b</a>", "<a href=\"mailto:mailto:x\">y</a>", "<a href=\"MAILTO:x\">y</a>", "<a href=\"&#x23;top\">y</a>", "<img src=\"a b\" alt=\"c  d\">",
    "<img src=\"\" alt=\"\">", "<img>", "<h1>ß straße ǆ ŉ ﬁ ΟΔΟΣ</h1>", "<h2>title <b>bold</b> &amp; more</h2>", "<h3>a</h3><h4>b</h4><h5>c</h5><h6>d</h6>", "<h1><h2>nested</h2></h1>",
    "<h1>a<br>b</h1>", "<h1><pre>pre  in\nheading</pre></h1>", "<pre>a<br>b\n\n<b>c</b>  &amp;  d</pre>", "<pre>\n\nlead</pre>", "<pre><p>x</p><ul><li>y</li></ul></pre>",
    "<blockquote>a<blockquote>b<blockquote>c</blockquote></blockquote></blockquote>", "<blockquote>\n\n</blockquote>", "<blockquote></blockquote>x", "<blockquote><pre>a\n\n\nb</pre></blockquote>",
    "<blockquote>" + "word ".repeat(40) + "</blockquote>", "<ul></ul>", "<ul>text<li>a</li>more<li>b</li><!-- c --><p>p</p></ul>", "<ul><li></li><li> </li><li>x</li></ul>",
    "<ul><li>a<ol><li>b<ul><li>c</li></ul></li></ol></li></ul>", "<ol><li>" + "word ".repeat(30) + "</li><li>b\nc<br>d</li></ol>", "<ol type=i start=3998><li>a<li>b<li>c</ol>",
    "<ol type=A start=25><li>a<li>b<li>c<li>d</ol>", "<ol type=a start=702><li>a<li>b</ol>", "<ol start=99><li>a<li>b</ol>", "<ol reversed start=5><li>a</ol>", "<li>orphan</li>",
    "<ol><ul><li>a</li></ul></ol>", "<ul><li><ul><li><ul><li>deep</li></ul></li></ul></li></ul>", "<ol><li>a</li></ol>text<ol><li>b</li></ol>", "<ul><li>a</li></ul><ul><li>b</li></ul>",
    "<p>a</p>\n<p>b</p>\n", "<div>a</div><div>b</div>", "<div><div><div>x</div></div></div>", "<div>a<p>b</p>c</div>", "a<div>b</div>c", "<p>a<div>b</div>c</p>", "<span>a</span><span>b</span>",
    "<span>a </span><span> b</span>", "a<b>b</b>c d<i> e </i>f", "a<br>b<br><br>c<br><br><br>d", "<br>", "<br><br>x", "x<br>", "<p><br></p>", "<hr>", "<hr><hr>", "a<hr>b",
    "<table><thead><tr><th>h1<th>h2<tbody><tr><td>a<td>b<tfoot><tr><td>f</table>", "<table><tr><td><p>x</p></td><td><ul><li>y</li></ul></td></tr></table>", "<td>a<td>b<tr><td>c",
    "<select><option>a<option>b<optgroup><option>c</select>", "<form><input value=x><button>go</button><textarea>t</textarea></form>", "<dl><dt>t<dd>d<dt>t2<dd>d2</dl>",
    "<ruby>漢<rp>(<rt>kan<rp>)</ruby>", "a<wbr>b", "a".repeat(78) + "<wbr>bcd", "<p>" + "a".repeat(78) + " <wbr>bcd</p>", "a b<wbr>" + "c".repeat(80), "<body>a</body><body>b</body>",
    "<head><body>x", "<html><body><p>a</body></html><p>b", "<body><script>x</script>y<style>z</style></body>", "<div><body>inner</body></div>outer", "<script><body>x</body></script>y",
    "<p><constructor>x", "<constructor>x", "<p><CONSTRUCTOR>x</CONSTRUCTOR>", "<p></constructor>x", "<p><toString>x<valueOf>y<hasOwnProperty>z", "<__proto__>x", "<p><prototype>x",
    "<ol type=\"I\" start=\"9999\"><li>a</li><li>b</li></ol>", "<ol type=\"I\" start=\"3999\"><li>a</li><li>b</li></ol>", "<ol type=\"I\" start=\"4000\"><li>a</li></ol>",
    "<ol type=\"I\" start=\"5000\"><li>a</li></ol>", "<ol type=\"I\" start=\"10000\"><li>a</li></ol>", "<ol type=\"I\" start=\"40000\"><li>a</li></ol>", "<ol type=\"I\" start=\"44444\"><li>a</li></ol>",
    "<ol type=\"i\" start=\"49999\"><li>a</li></ol>", "<ol type=\"I\" start=\"-5\"><li>a</li></ol>", "<ol type=\"I\" start=\"3.5\"><li>a</li></ol>", "<ol type=\"I\" start=\"abc\"><li>a</li></ol>",
    "<ol type=\"I\" start=\"Infinity\"><li>a</li></ol>", "<ol type=\"I\" start=\"1e21\"><li>a</li></ol>", "<ol type=\"I\" start=\"0\"><li>a</li><li>b</li></ol>", "<ol type=\"a\" start=\"0\"><li>a</li><li>b</li></ol>",
    "<ol type=\"a\" start=\"-5\"><li>a</li></ol>", "<ol type=\"A\" start=\"27\"><li>a</li><li>b</li></ol>", "<ol type=\"a\" start=\"3.5\"><li>a</li></ol>", "<ol type=\"a\" start=\"abc\"><li>a</li></ol>",
    "<ol type=\"a\" start=\"-Infinity\"><li>a</li></ol>", "<ol type=\"a\" start=\"1e300\"><li>a</li></ol>", "<ol start=\"1e21\"><li>a</li><li>b</li></ol>", "<ol start=\"1e-7\"><li>a</li><li>b</li></ol>",
    "<ol start=\"-0\"><li>a</li><li>b</li></ol>", "<ol start=\"0x1F\"><li>a</li></ol>", "<ol start=\" 12 \"><li>a</li></ol>", "<ol start=\"1_0\"><li>a</li></ol>", "<ol start=\".5\"><li>a</li></ol>",
    "<ol start=\"5.\"><li>a</li></ol>", "<ol start=\"0b11\"><li>a</li></ol>", "<ol start=\"0o17\"><li>a</li></ol>", "<ol start=\"0xFFFFFFFFFFFFFFFFFFFF\"><li>a</li></ol>",
    "<ol start=\"123456789012345678901234567890\"><li>a</li></ol>", "<ol start=\"0.000001\"><li>a</li></ol>", "<ol start=\"1e400\"><li>a</li></ol>", "<ol start=\"\u00a05\ufeff\"><li>a</li></ol>",
    "<ol start=\"+5\"><li>a</li></ol>", "<ol start=\"-.5e1\"><li>a</li></ol>", "<ol start=\"0X1f\"><li>a</li></ol>", "<ol start=\"+0x1\"><li>a</li></ol>", "<ol start=\"Infinity\"><li>a</li></ol>",
    "<ol start=\"-Infinity\"><li>a</li></ol>", "<ol start=\"infinity\"><li>a</li></ol>", "<ol start=\"NaN\"><li>a</li></ol>", "<ol start=\"1e\"><li>a</li></ol>", "<ol start=\"e1\"><li>a</li></ol>",
    "<ol start=\"1.2.3\"><li>a</li></ol>", "<ol start=\"0.1\"><li>a</li><li>b</li><li>c</li></ol>", "<ol start=\"9007199254740992\"><li>a</li><li>b</li></ol>", "<ol start=\"1e20\"><li>a</li></ol>",
    "<ol start=\"123456.789\"><li>a</li></ol>", "<ol start=\"0o7705173430276017417\" type=\"A\"><li>a</li><li>b</li></ol>", "<ol start=\"0x20000000000001\"><li>a</li></ol>",
    "<ol start=\"0x20000000000003\"><li>a</li></ol>", "<ol start=\"0x67e83FAf5da7CDEb93Ed\" type=\"1\"><li>a</li><li>b</li></ol>", "<ol start=\"0x1fffffffffffff4\"><li>a</li></ol>", "<ol start=\"0x200000000000010000001\"><li>a</li></ol>",
    "<ol start=\"893514022863146.2\"><li>a</li><li>b</li></ol>", "<ol start=\"893514022863146.25\"><li>a</li></ol>", "<ol start=\"1125899906842625.25\"><li>a</li></ol>",
    "<ol start=\"1125899906842625.75\"><li>a</li></ol>", "<ol start=\"562949953421313.75\" type=\"I\"><li>a</li></ol>", "<ol start=\"4.35\"><li>a</li><li>b</li></ol>",
    `<ol start="0b1${"0".repeat(52)}1"><li>a</li></ol>`, `<ol start="0b1${"0".repeat(52)}11"><li>a</li></ol>`, `<ol start="0x${"f".repeat(255)}" type="a"><li>a</li></ol>`, `<ol start="0x${"f".repeat(256)}"><li>a</li></ol>`, "<ol start=\"0.00000012345\"><li>a</li></ol>", "<ol type=\"I\" start=\"1994\"><li>a</li><li>b</li></ol>", "<ol type=\"i\" start=\"8\"><li>a</li><li>b</li></ol>",
    "a&nbsp;b&nbsp;&nbsp;c", "a\u00a0b \u00a0 c", "a\u200bb \u200b c", "a\u3000b", "a\u000bb\u000cc", "\u200b", "<p>\u200b</p>x", "a &amp;amp; b", "<p>&#60;script&#62;</p>",
    ...ENTITY_PIECES.map((piece) => `<p>a ${piece} b${piece}c</p>`),
    ...Array.from({ length: 700 }, () => node(1 + rng.int(5))),
    // Uppercasing where one letter becomes several, and numbers written out as list markers.
    ...Array.from({ length: 150 }, () => `<h${1 + rng.int(6)}>${rng.several(["ß", "ŉ", "ǰ", "ΐ", "ﬁ", "ﬃ", "i\u0307", "ı", "ǆ", "ǅ", "ς", "σ", "ᾳ", "ᾀ", "ա", "և", "ǉ", "a", "é", " ", " ", "😀", "한"], 1, 10)}</h1>`),
    ...Array.from({ length: 300 }, () => {
      const digits = () => rng.several("0123456789".split(""), 1, rng.pick([1, 3, 8, 17, 25]));
      const start = rng.pick([digits, () => `${digits()}.${digits()}`, () => `.${digits()}`, () => `${digits()}e${rng.pick(["", "+", "-"])}${rng.int(330)}`, () => `${rng.pick(["-", "+", " ", ""])}${digits()}${rng.pick(["", " ", ".", "e", "x"])}`,
        // A quarter past a large integer lies exactly between two shortest ways to write it.
        () => `${2 ** 49 + rng.int(2 ** 30) * 2 ** 19 + rng.int(2 ** 19)}.${rng.pick(["25", "75", "2", "3", "7", "8", "125", "5"])}`, () => `0x${rng.several("0123456789abcdefABCDEFg".split(""), 1, rng.pick([6, 14, 20, 40]))}`, () => `0${rng.pick(["b", "B"])}${rng.several(["0", "1", "1", "0", "2"], 1, rng.pick([8, 53, 60, 90]))}`, () => `0${rng.pick(["o", "O"])}${rng.several("012345678".split(""), 1, rng.pick([6, 18, 24, 40]))}`])();
      // A lettered list starting at Infinity never finishes in the TypeScript.
      const types = Number(start) === Infinity ? ["", ' type="i"', ' type="I"'] : ["", ' type="a"', ' type="A"', ' type="i"', ' type="I"', ' type="1"'];
      return `<ol start="${start}"${rng.pick(types)}><li>a</li><li>b</li></ol>`;
    }),
    ...Array.from({ length: 500 }, soup)
  ];
};

const renderOrError = (html: string, urls: boolean) => {
  try {
    return compact(htmlToPlainText(html, { urls }));
  } catch (error) {
    return { error: (error as Error).message };
  }
};

const plainTextFixture = () => {
  const rng = random(606);
  const entityInputs = [
    "&amp; &lt; &gt; &quot; &apos; &nbsp; &copy; &hellip; &euro;", "&amp &ampx; a=1&copy=2 &lt3 &frac12x", "&#65;&#x42;&#X43;&#68 &#x1F600;", "&#0; &#150; &#xD800; &#99999999;",
    "&; & &#; &#x; AT&T &unknown; &check;", "?a=1&amp;b=2&#38;c&#x26;d", "?a=1&copy=2&amp=3&#38x", "&notaname;", "", "no references", ...ENTITY_PIECES,
    ...Array.from({ length: 600 }, () => rng.several([...ENTITY_PIECES, ...WORDS, " ", "&", ";", "#", "x", "é"], 1, 6))
  ];
  writeFixture("entities", {
    decodeEntities: run("decodeEntities", entityInputs, decodeEntities),
    decodeAttributeEntities: run("decodeAttributeEntities", entityInputs, decodeAttributeEntities)
  });

  const html = htmlInputs(rng);
  for (const type of ["a", "A"]) {
    knownDifferences.push({
      function: "htmlToPlainText",
      input: `<ol type="${type}" start="1e400"><li>a</li><li>b</li></ol>`,
      typescript: JSON.stringify("never returns"),
      rust: [" Infinity. a\n Infinity. b", " Infinity. a\n Infinity. b"],
      why:
        "The TypeScript loop that writes a number in letters does not end for Infinity: it appends digits until memory runs out. " +
        "The Rust port writes the number itself."
    });
  }
  // Elements nested past what the TypeScript call stack holds.
  const nested = "<b>".repeat(30_000) + "x";
  let overflow = "";
  try {
    htmlToPlainText(nested);
  } catch (error) {
    overflow = `${(error as Error).name}: ${(error as Error).message}`;
  }
  if (!overflow) throw new Error("plain text: expected the TypeScript renderer to run out of call stack");
  knownDifferences.push({
    function: "htmlToPlainText",
    input: nested,
    typescript: JSON.stringify({ throws: overflow }),
    rust: ["x", "x"],
    why:
      "The TypeScript renderer recurses over the element tree and throws once elements nest some thousands deep " +
      "(how deep depends on the element and on the call stack it starts from). The Rust port keeps its own stack and renders any depth."
  });
  writeFixture("plain-text", {
    htmlToPlainText: run("htmlToPlainText", html, (input) => [renderOrError(input, false), renderOrError(input, true)])
  });

  // getText throws where htmlToPlainText does; those inputs are known differences.
  const getTextCases: [string, unknown][] = [];
  for (const input of [...new Set(html)]) {
    let output: string;
    try {
      output = getText(input);
    } catch (error) {
      knownDifferences.push({
        function: "getText",
        input,
        typescript: JSON.stringify({ throws: (error as Error).message }),
        rust: null,
        why:
          "The TypeScript getText throws here (a TypeError out of htmlToPlainText). The Rust get_text returns a String, " +
          "so it answers the text rendered leniently instead; the Rust html_to_plain_text itself reports the failure."
      });
      continue;
    }
    if (hasLoneSurrogate(output)) continue;
    getTextCases.push([input, compact(output)]);
  }
  const urlTexts = [
    "<p>Check out https://example.com/page for more info</p>", "http://a", "http://ab", "http://a b", "http://a\tb c", "http://a.b/c?d#e!", "http://a.b/c).", "ftp://x.y/z", "https://x", "https://xy!",
    "http:///x", "http://$x", "http://.x", "http://?x", "http://#x", "http://é.com/ü", "http://a\u00a0b", "xhttp://a.b", "HTTP://a.b", "http://a.b/한글 뒤", "http://a.b/😀 x", "ftps://a.b",
    "a http://a.b b https://c.d e ftp://e.f", "http://a.bhttp://c.d", "http://a_", "http://a-", "http://ab-", "http://a.b/---", "http://a.b/_ x", "<pre>a  \n  b \n\n\n c\t\td\r\ne</pre>",
    "<pre> \n \n</pre>", "<pre>a \n b</pre>", "<pre>a\n b</pre>", "<pre>a \nb</pre>", "<pre>\n\na\n\n</pre>", "<pre>a\u00a0\n\u2028b\u3000c</pre>", "<p>a</p><p> </p><p>b</p>"
  ];
  for (const input of urlTexts) getTextCases.push([input, getText(input)]);
  writeFixture("util", {
    getText: getTextCases,
    nestedPath: run("nestedPath", ["mail.envelopeTo.address", "mail.from.value.address", "field", "parent.child", "", ".", "a.", ".a", "a..b", "x", "é", "한글", "a😀", "😀", "a.😀"], nestedPath)
  });
};

// ---------------------------------------------------------------------------
// snippet, text-to-html
// ---------------------------------------------------------------------------

const snippetFixture = () => {
  const rng = random(707);
  const lines = [
    "Hello, this is the body.", "First line", "second   line", "  third line  ", "My reply is here.", "> Quoted original", ">> Nested quote", " > indented quote", "\t>tab quote", "\u00a0> nbsp quote",
    "See below.", "On Tue, Jan 5, 2021 at 3:00 PM, John <john@x.com> wrote:", "On wrote:", "On x wrote:", "Onx wrote:", "On_ wrote:", "On: wrote:", "  On a wrote:  ", "On a wrote: b", "On a\rb wrote:",
    "On a\u2028b wrote:", "On a wrote:\u2028", "on a wrote:", "ON a wrote:", "On a Wrote:", "Le 5 janv. 2021 à 15:00, Jean <jean@x.com> a écrit :", "Le x a écrit:", "Le x a écrit  :  ", "Lex a écrit :",
    "Le a écrit :", "Le x a e\u0301crit :", "le x a écrit :", "Le x a écrit : y", "-----Original Message-----", "-- Original Message", "- Original Message", "--   original message", "--Forwarded Message",
    "---------- Forwarded message ---------", "--\u00a0Forwarded message", "-- Forwarded meſſage", "__", "___________", "_", " __", "Begin forwarded message:", "begin FORWARDED message", " Begin forwarded message",
    "--", "-- ", "---", "", " ", "\t", "word", "한글 문장입니다", "😀😀😀", "é".repeat(30), "x".repeat(160), "x".repeat(161), "word ".repeat(40), "😀".repeat(79) + "ab", "a" + "😀".repeat(80), "ab " + "😀".repeat(100),
    "a".repeat(159) + "😀", "a".repeat(158) + " 😀", " " + "a".repeat(200), "a " + "b".repeat(200), "a\u00a0b\u3000c\u2028d\ufeffe\u0085f\u000bg"
  ];
  const inputs: (string | null)[] = [
    "Hello, this is the body.", "First line\nsecond   line\n\n  third line  ", "My reply is here.\n> Quoted original\n>> Nested quote\n> More quote",
    "See below.\nOn Tue, Jan 5, 2021 at 3:00 PM, John <john@x.com> wrote:\n> old text", "See below.\nLe 5 janv. 2021 à 15:00, Jean <jean@x.com> a écrit :\n> old text",
    "New content here.\n-----Original Message-----\nFrom: old@x.com\nOld body", "Forwarding this along.\nBegin forwarded message:\nFrom: old@x.com",
    "See below.\n---------- Forwarded message ---------\nFrom: old@x.com\nOld body", "---------- Forwarded message ---------\nFrom: old@x.com\nOld body",
    Array.from({ length: 30 }, (_, i) => `word${i}`).join(" "), "x".repeat(160), "", "   \n  ", "> only quotes\n> here", null, "a\r\nb\rc\r\n", "a\r", "\r\n", "a\r\r\nb", "On a wrote:\r", "On a wrote:\r\nb",
    ...lines,
    ...Array.from({ length: 700 }, () => rng.several(lines, 1, 6, rng.pick(["\n", "\n", "\r\n", "\n\n", "\r"])))
  ];
  writeFixture("snippet", { buildSnippet: run("buildSnippet", inputs, (text) => buildSnippet(text) ?? null) });

  const pieces = [
    ...TEXT_PIECES, ...WORDS, ...WORDS, " ", " ", "  ", "\t", ">", "> ", ">>", "> > ", ">\t", ">\u00a0", "> >> >", " >", "&", "<", "\"", "http://a.example/x).", "https://a.example/한글?q=1&r=2,",
    "https://a.example/'quoted'", "http://x", "http://x.", "http://.", "http://..a", "http://a\"b", "xhttps://a.b", "_https://a.b", "-https://a.b", "https://a.b/😀", "httpshttp://a.b", "http://a.b/c>d"
  ];
  const textInputs = [
    "one\ntwo\r\nthree", "a < b && c > d &para;", "see https://example.com/x?with=query&params=1 now", "go to https://example.com/a.", "javascript:alert(1)", "hi\n> one\n>> two\n> three\nbye", "> a", "",
    "\n", "\n\n", ">", ">\n", "> a\n\n> b", "a\n>\nb", ">>> deep\n> shallow\n>> mid", "> a\r\n> b\rc", ">a\n> b\n>  c\n>\td", "a\n\n\n> b\n\n\nc", "> > a\n> b\n", "\r", "\r\n\r\n", "a\rb\r\nc\n\rd",
    ...Array.from({ length: 800 }, () => Array.from({ length: 1 + rng.int(6) }, () => rng.several(pieces, 0, 5)).join(rng.pick(["\n", "\n", "\r\n", "\r"])))
  ];
  const bodies: [string | null, string | null][] = [
    ["a\nb", "a\nb"], ["", "text"], [null, "text"], ["<p>a</p>", "a"], [null, null], ["", ""], ["x", ""], ["x", null], ["", "> q\nhttps://a.b"], ["same", "same"], ["Same", "same"], ["a", "a\n"]
  ];
  writeFixture("text-to-html", {
    plainTextToHtml: run("plainTextToHtml", textInputs, plainTextToHtml),
    bodyHtml: run("bodyHtml", bodies, ([html, text]) => bodyHtml(html, text))
  });
};

// ---------------------------------------------------------------------------
// threading
// ---------------------------------------------------------------------------

const isOwn = (address: string) => address.toLowerCase().endsWith("@inbox.test");

const threadingFixture = () => {
  const rng = random(808);
  const prefixes = ["Re:", "RE:", "re :", "Fwd:", "FW:", "fw:", "Fwd :", "AW:", "WG:", "SV:", "VS:", "Antw:", "TR:", "RV:", "Re[2]:", "Re [3] :", "Re：", "RE[10]：", "Re[]:", "Re[a]:", "Re", "Re;", "R:", "Rep:", "Fwdd:", "Reply:", "re:re:", "\u00a0Re:\u00a0", "Re:\u2028", "RE\u3000:", "Fw:Fwd:", "Ｒｅ:", "re[1]", "Rſ:", "SV[2]:\t"];
  const topics = ["Plan", "Project  plan", "Invoice", "invoice", "hi", "Hallo", "Return policy", "Regarding x", "", " ", "한글 제목", "ÉTÉ", "Σίσυφος ΣΟΦΟΣ", "İstanbul", "a", "ab", "abc", "😀", "😀a", "Weekly digest", "Re", "FW", "x: y", "\u00a0spaced\u00a0out\u3000"];
  const subject = () => Array.from({ length: rng.int(4) }, () => rng.pick(prefixes)).join(rng.pick(["", " ", "  "])) + rng.pick(["", " ", ""]) + rng.pick(topics) + rng.pick(["", " ", ""]);
  // Lowercasing in context: a final sigma depends on the letters and marks around it.
  const sigmaPieces = ["Σ", "Σ", "σ", "ς", "Α", "α", "a", "B", ".", "'", "’", "\u0301", "\u0345", "-", "\u00ad", "1", "ʰ", ":", "İ", "I", "\u200d", "\u0307", "ǅ", "ẞ", "😀", "한"];
  const sigmas = Array.from({ length: 300 }, () => "x" + rng.several(sigmaPieces, 1, 8));
  const subjects = [...new Set(["Re: RE: Fwd: Project  plan", "AW: Re[2]: Hallo", "Return policy", "Re: x", "Fwd: x", "Regarding x", ...prefixes, ...topics, ...Array.from({ length: 500 }, subject), ...sigmas])];

  const ids = ["<a@x>", "a@x", " <a@x> ", "<<a@x>>", "<a@x", "a@x>", "<>", "", " ", "<b@y>", "b@y", "\u00a0<c@z>\u00a0", "<a@x>>", "<한@글>", "<", ">"];
  const addresses = ["alice@x.com", "Alice@X.com", "me@inbox.test", "ME@INBOX.TEST", "bob@y.org", "billing@vendor-a.com", "ar@vendor-b.com", "", "ÉCOLE@x.fr", "İ@x.tr"];
  const addressValue = (): unknown =>
    rng.pick<() => unknown>([
      () => Array.from({ length: 1 + rng.int(3) }, () => ({ address: rng.pick(addresses), name: "n" })),
      () => ({ address: rng.pick(addresses) }),
      () => [{ address: rng.pick(addresses) }, { name: "no address" }, null, "string", 5, { address: 7 }, { address: "" }, [{ address: "nested@x.com" }]],
      () => null, () => undefined, () => "alice@x.com", () => 0, () => false, () => [], () => ({})
    ])();
  const candidate = (id: string): ThreadCandidate => {
    const mail: ThreadCandidate = { mail_id: id, message_id: rng.pick([`<${id}@x>`, `${id}@x`, rng.pick(ids)]), subject: rng.chance(0.7) ? rng.pick(["Plan", "Re: Plan", "RE: plan", "Fwd: Plan", "Re: Invoice", "invoice", "Other"]) : subject() };
    if (rng.chance(0.4)) mail.in_reply_to = rng.pick([`<${rng.pick("abcdef".split(""))}@x>`, rng.pick(ids), null, ""]);
    if (rng.chance(0.4)) mail.reference_ids = rng.pick([[], null, Array.from({ length: 1 + rng.int(3) }, () => `<${rng.pick("abcdef".split(""))}@x>`), [rng.pick(ids), rng.pick(ids)]]);
    if (rng.chance(0.9)) mail.from_address = addressValue();
    if (rng.chance(0.9)) mail.to_address = addressValue();
    if (rng.chance(0.4)) mail.cc_address = addressValue();
    return mail;
  };
  const mails = Array.from({ length: 400 }, (_, i) => candidate("abcdef"[i % 6] + (i < 6 ? "" : String(i))));
  // Pairs are recorded as the positions of their mails.
  const pairs = Array.from({ length: 700 }, (): [number, number] => [rng.int(mails.length), rng.int(mails.length)]);
  const groups = Array.from({ length: 150 }, () => {
    const size = 2 + rng.int(8);
    const candidates = Array.from({ length: size }, (_, i) => candidate("abcdef"[rng.int(6)] + (rng.chance(0.2) ? "" : String(i))));
    return [rng.chance(0.8) ? rng.pick(candidates) : candidate("seed"), candidates] as [ThreadCandidate, ThreadCandidate[]];
  });

  writeFixture("threading", {
    normalizeSubject: run("normalizeSubject", subjects, normalizeSubject),
    isReplySubject: run("isReplySubject", subjects, isReplySubject),
    bareMessageId: run("bareMessageId", ids, bareMessageId),
    storedIdForms: run("storedIdForms", [["<a@x>", "b@y"], [], ...Array.from({ length: 100 }, () => Array.from({ length: rng.int(6) }, () => rng.pick(ids)))], storedIdForms),
    mails: run("mails", mails, (mail) => [hasThreadHeaders(mail), linkedIds(mail), [...otherParticipantsOf(mail, isOwn)].sort()]),
    matchesBySubject: run("matchesBySubject", pairs, ([a, b]) => matchesBySubject(mails[a], mails[b], isOwn)),
    linkedByHeaders: run("linkedByHeaders", pairs, ([a, b]) => linkedByHeaders(mails[a], mails[b])),
    collectThread: run("collectThread", groups, ([seed, candidates]) => collectThread(seed, candidates, isOwn).map((mail) => mail.mail_id))
  });
};

// ---------------------------------------------------------------------------
// dates and file types
// ---------------------------------------------------------------------------

/** `new Date(text)` on a string held the way a decoded header is: 8-bit when ASCII, else 16-bit. */
const parseDate = (text: string): number | null => {
  const time = new Date(Buffer.from(text, "utf8").toString("utf8")).getTime();
  return Number.isNaN(time) ? null : time;
};

const datesFixture = () => {
  const rng = random(909);
  const months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec", "January", "sept", "DEC", "juLy", "Marz", "Janx", "Ma", "Mayo"];
  const days = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun", "Tuesday", "Foo", "T", "Z", "am", "pm", "utc", "X1"];
  const zones = ["GMT", "UTC", "UT", "Z", "EST", "EDT", "CST", "CDT", "MST", "MDT", "PST", "PDT", "JST", "CET", "z", "gmt", "Est", "GMTX", "A", "T", "AM", "PM", "am", "pm"];
  const seps = [" ", " ", " ", ", ", ",", "-", "/", ".", "  ", "\t", ":", "", "T", " (x) ", "\u00a0", "\n", "+", ") ", "(", "\u2028", "\u3000", "_", ";", "\u0000", "\u000b", "\u000c", "\u2029", "\ufeff", "\u0085"];
  const pad = (n: number, width = 2) => String(n).padStart(width, "0");
  const number = () => rng.pick([() => String(rng.int(32)), () => pad(rng.int(100)), () => String(1900 + rng.int(200)), () => String(rng.int(100000)), () => "0".repeat(rng.int(5)) + String(rng.int(1000)), () => String(rng.int(1e9)) + String(rng.int(1000)), () => String(rng.int(13))])();
  const time = () => {
    const h = rng.pick([String(rng.int(25)), pad(rng.int(24)), "24", "12", "0", "00"]);
    const m = rng.pick([pad(rng.int(60)), String(rng.int(61)), "00", "60"]);
    const s = rng.pick([pad(rng.int(60)), String(rng.int(61)), "00", "60"]);
    let text = rng.pick([`${h}:${m}:${s}`, `${h}:${m}:${s}`, `${h}:${m}`, `${h}:`, `${h}::`, `${h}:${m}:${s}:`, `${h}.${m}`]);
    if (rng.chance(0.2)) text += "." + "0".repeat(rng.int(3)) + String(rng.int(rng.pick([10, 1000, 100000, 1e9])));
    if (rng.chance(0.2)) text += rng.pick([" AM", " PM", "am", "pm", " am", " p.m."]);
    return text;
  };
  const zone = () => rng.pick([
    () => rng.pick(["+", "-"]) + rng.pick([pad(rng.int(24)) + pad(rng.int(60)), String(rng.int(15)), String(rng.int(2400)), String(rng.int(100000)), "", pad(rng.int(24)) + ":" + pad(rng.int(100)), String(rng.int(1200000)) + ":" + pad(rng.int(60))]),
    () => rng.pick(zones), () => rng.pick(zones) + rng.pick(["+", "-"]) + pad(rng.int(15), rng.int(3)) + (rng.chance(0.3) ? ":" + String(rng.int(60)) : ""),
    () => rng.pick(["(UTC)", "(KST", "(a(b)c)", ")", "(Coordinated Universal Time)"])
  ])();
  const piece = (): string => rng.pick([number, number, () => rng.pick(months), () => rng.pick(days), time, time, zone, () => rng.pick(["+", "-", "(", ")", ".", ":", ",", "/", "T", "Z", "é", "あ", "😀", "\u0000", "@", "[", "_"]), () => number() + rng.pick(seps) + number()])();
  const iso = () => {
    let text = rng.pick([String(1000 + rng.int(9000)), pad(rng.int(10000), 4), rng.pick(["+", "-"]) + pad(rng.int(300000), 6), String(rng.int(100000)), "-000000"]);
    if (rng.chance(0.9)) {
      text += "-" + rng.pick([pad(1 + rng.int(12)), String(rng.int(14)), "00", "13"]);
      if (rng.chance(0.9)) text += "-" + rng.pick([pad(1 + rng.int(31)), String(rng.int(33)), "00", "32"]);
    }
    if (rng.chance(0.7)) {
      text += rng.pick(["T", "T", "T", "t", " ", "  ", "T "]) + rng.pick([pad(rng.int(25)), String(rng.int(25))]) + ":" + rng.pick([pad(rng.int(60)), String(rng.int(61))]);
      if (rng.chance(0.7)) {
        text += ":" + rng.pick([pad(rng.int(60)), String(rng.int(61))]);
        if (rng.chance(0.5)) text += rng.pick([".", ".", ","]) + pad(rng.int(rng.pick([10, 1000, 1e6])), rng.int(4));
      }
      text += rng.pick(["", "", "Z", "z", " Z", "+09:00", "-0330", "+09", "+0900", "+9:00", "+24:00", "+23:60", "+09:0", " +0900", "GMT", " UTC"]);
    }
    return text + (rng.chance(0.1) ? rng.pick([" ", "x", " GMT", "Z", " 5"]) : "");
  };
  const rfc = () => {
    let text = rng.chance(0.7) ? rng.pick(days) + rng.pick([", ", " ", ","]) : "";
    const d = rng.pick([String(1 + rng.int(31)), pad(rng.int(40))]);
    const mo = rng.pick(months);
    const y = rng.pick([String(1970 + rng.int(100)), pad(rng.int(100)), String(rng.int(300000))]);
    const sep = rng.pick([" ", " ", " ", "-", "/", ", "]);
    const order = rng.int(4);
    text += [`${d}${sep}${mo}${sep}${y}`, `${mo}${sep}${d}${sep}${y}`, `${y}${sep}${mo}${sep}${d}`, `${mo}${sep}${d}`][order];
    if (rng.chance(0.85)) text += rng.pick([" ", " ", "  ", ",", "T", ""]) + time();
    if (order === 3) text += " " + y;
    if (rng.chance(0.8)) text += rng.pick([" ", " ", "", "  "]) + zone();
    return text + (rng.chance(0.15) ? rng.pick([" ", "  ", " x", " (c)", " 2027", "\u00a0", ".", ","]) : "");
  };
  const mutate = (text: string) => {
    if (!text) return text;
    const at = rng.int(text.length + 1);
    return rng.pick([
      () => text.slice(0, at) + rng.pick(seps) + text.slice(at), () => text.slice(0, at) + text.slice(at + 1),
      () => text.slice(0, at) + rng.pick(["0", "1", "9", "a", "T", "Z", "-", "+", ":", ".", "(", ")", " "]) + text.slice(at + 1),
      () => text.toUpperCase(), () => text.toLowerCase(), () => text + rng.pick(seps) + piece()
    ])();
  };
  const curated = [
    "Tue, 27 Oct 2026 10:00:00 +0900", "27 Oct 2026 10:00:00 +0900", "not a date", "", "2026-10-27", "Tue, 27 Oct 2026 10:00:00 GMT (UTC)", "Tue, 27 Oct 2026 10:00:00 PST", "27 Oct 26 10:00 EST",
    "Oct 27 2026", "2026/10/27 10:00", "10/27/2026", "Tue Oct 27 2026 10:00:00 GMT+0900 (KST)", "0", "1", "2", "9", "12", "13", "31", "32", "49", "50", "99", "100", "999", "1000", "2026", "10000", "99999",
    "275760", "275761", "1 2", "1 2 3", "1/2", "1/2/3", "1-2-3", "1.2.3", "2026 10 27", "27 10 2026", "10 27 2026", "Jan", "Jan 1", "Jan 2026", "1 Jan", "January", "Janx 1 2026", "Jan1 2026",
    "Tue, 27 Oct 2026 10", "Tue, 27 Oct 2026 10:", "Tue, 27 Oct 2026 10:00:", "Tue, 27 Oct 2026 10:00:00:", "Tue, 27 Oct 2026 10:00:00.", "Tue, 27 Oct 2026 10:00:00.1", "Tue, 27 Oct 2026 10:00:00.1234",
    "Tue, 27 Oct 2026 10:00:00,123", "Tue, 27 Oct 2026 10:00:00 +9", "Tue, 27 Oct 2026 10:00:00 +090", "Tue, 27 Oct 2026 10:00:00 +09000", "Tue, 27 Oct 2026 10:00:00 +9960", "Tue, 27 Oct 2026 10:00:00 +09:30",
    "Tue, 27 Oct 2026 10:00:00 +09:99", "Tue, 27 Oct 2026 10:00:00 + 0900", "Tue, 27 Oct 2026 10:00:00+0900", "Tue, 27 Oct 2026 10:00:00 GMT+0900", "Tue, 27 Oct 2026 10:00:00 UTC+9", "Tue, 27 Oct 2026 10:00:00 JST",
    "Tue, 27 Oct 2026 10:00:00 Z", "Tue, 27 Oct 2026 10:00:00 A", "Tue, 27 Oct 2026 10:00:00 GMTX", "Tue, 27 Oct 2026 10:00:00 EST 5", "Tue, 27 Oct 2026 10:00:00 (KST", "Tue, 27 Oct 2026 12:00:00 AM",
    "Tue, 27 Oct 2026 12:00:00 PM", "Tue, 27 Oct 2026 13:00:00 PM", "Tue, 27 Oct 2026 0:00:00 PM", "Tue, 27 Oct 2026 10:00:00AM", "Tue, 27 Oct 2026 10 pm", "Tue, 27 Oct 2026 24:00:00", "Tue, 27 Oct 2026 23:60:00",
    "Tue, 27 Oct 2026 23:59:60", "Tue, 27 Oct 2026 -1:00:00", "Tue, 0 Oct 2026", "Tue, 31 Feb 2026", "Tue, 29 Feb 2025", "Tue, -1 Oct 2026", "Oct 27 10:00:00 GMT 2026", "Tue Oct 27 10:00:00 +0900 2026",
    "27 Oct 2026T10:00:00", "2026-10-27T10:00", "2026-10-27t10:00:00z", "2026-10-27 10:00:00 +0900", "2026-10-27T10:00:00 +0900", "2026-10-27T10:00:00+0900", "2026-10-27T10:00:00.123456789Z", "2026-10-27T24:00:00Z",
    "2026-10-27T24:00:01Z", "2026-13-27", "2026-1-2", "+002026-10-27", "-000001-01-01", "-000000-01-01", "20261027", "Tue, 27 Oct 2026 10:00:00 +0900 trailing", "Tue, 27 Oct 2026 10:00:00 +0900 2027",
    "Tue, 27 Oct 2026 10:00:00 +0900\u00a0", "Tue, 27 Oct 2026 10:00:00 +0900 あ", "あ 27 Oct 2026", "Tüe, 27 Oct 2026 10:00:00 +0900", "Mon Jan _2 15:04:05 2006", "Fri, 21 Nov 1997 09 : 55 : 06 -0600",
    "Dec 31 275760", "Sep 13 275760 00:00:00 GMT", "Sep 14 275760 00:00:00 GMT", "Jan 1 -1", "1.5", "-1", "00001", "Jul 15 2026 17:36:00 +300000:00", "Jul 15 2026 17:36:00 +596523:00", "Jul 15 2026 17:36:00 +596524:00",
    "Jul 15 2026 17:36:00 +1193047:00", "2026\u0000garbage", "Jul 15 2026 \u2028 10:00", "Jul\u202815 2026", "Jul 15 2026 10:00:00\u000b", "Jul 15 2026 10:00:00 \u000b", "15 Oct 12:39\u000b May",
    "15\u3000Oct 12:39\u000b May", "é 15 Oct 12:39\u000b May", "Jul 15 2026 10:00:00\u000c", "Jul 15 2026 10:00:00\u0085"
  ];
  const fuzzed = Array.from({ length: 4000 }, () => {
    let text = rng.pick([rfc, rfc, rfc, iso, iso, () => Array.from({ length: 1 + rng.int(6) }, (_, i) => (i ? rng.pick(seps) : "") + piece()).join("")])();
    for (let i = rng.chance(0.4) ? 1 + rng.int(3) : 0; i > 0; i--) text = mutate(text);
    return text;
  }).filter((text) => !hasLoneSurrogate(text));
  const times = [0, -1, 1, 1793005200000, 8.64e15, -8.64e15, -62198755200000, 253402300800000, 253402300799999, -62167219200000, -62167219200001, 951782400000, 4107542400000, -1e14, 1e15, ...Array.from({ length: 300 }, () => Math.floor((rng.next() - 0.5) * 2 * 8.64e15 * (rng.chance(0.7) ? 0.001 : 1)))];
  writeFixture("dates", {
    parse: run("new Date(text)", [...curated, ...fuzzed], parseDate),
    format: run("Date formatting", times, (time) => [new Date(time).toISOString(), new Date(time).toUTCString()])
  });

  // mimeTypeFromFilename is private to the parser; this is its body.
  const mimeTypeFromFilename = (filename: string): string | { error: ReturnType<typeof errorText> } => {
    try {
      const extension = filename.toLowerCase().replace(/\s/g, "").split(".").pop() ?? "";
      if (!extension) return "application/octet-stream";
      const type = Bun.file(`x.${extension}`).type.split(";")[0];
      return type || "application/octet-stream";
    } catch (error) {
      return { error: errorText(error) };
    }
  };
  // Every extension Bun knows a type for: all names of up to four letters and
  // digits that have one, and the longer ones on record.
  const alphabet = "abcdefghijklmnopqrstuvwxyz0123456789".split("");
  const extensions: string[] = [];
  const tryAll = (prefix: string, depth: number) => {
    for (const letter of alphabet) {
      const name = prefix + letter;
      if (Bun.file(`x.${name}`).type !== "application/octet-stream") extensions.push(name);
      if (depth > 1) tryAll(name, depth - 1);
    }
  };
  tryAll("", 4);
  extensions.push(...LONG_EXTENSIONS.filter((name) => mimeTypeFromFilename(`x.${name}`) !== "application/octet-stream"));
  extensions.sort();
  const table = new Set(extensions);
  const names = [
    "photo.JPG", "notes.txt", "archive.tar.gz", "a b.p d f", "pdf", "photo.jpg/", "photo.jpg//", "photo.jpg/x", "photo.jpg/.", "unknown.zzz", "trailing.", "", ".", "..", "...", ".bashrc", "a.b/c.png",
    "a.\u0000", "a.b\u0000c", "a.png\u0000", "\u0000.png", "a." + "x".repeat(4093), "a." + "x".repeat(4094), "a." + "é".repeat(2046), "a." + "é".repeat(2047), "a." + "한".repeat(1364), "a." + "한".repeat(1365),
    "a." + "😀".repeat(1023), "a." + "😀".repeat(1024), "a." + "/".repeat(4094), "a.png" + "/".repeat(4089), "a.png" + "/".repeat(4090), "a.\u0000" + "x".repeat(5000), "x.PnG", "x.p\u00a0n\u3000g", "x.png\n",
    "x.\u212aml", "x.İ", "x.htmlİ", "한글.pdf", "report.final.DOCX", "a\\b.png", "a.png\\", "a.png?x", "a.png#x", "a.png:", "a.png ", " .png", "x.tar.GZ ", "X.MJS", "x.c++", "x.x_b", "x.webmanifest",
    ...extensions.filter((_, i) => i % 9 === 0).map((extension) => `File Name.${extension.toUpperCase()}`),
    ...Array.from({ length: 200 }, () => rng.several([...WORDS, ".", ".", "/", " ", "png", "jpg", "pdf", "TXT", "é", "\t"], 1, 6))
  ];
  writeFixture("mime-types", {
    table: extensions.map((extension) => [extension, mimeTypeFromFilename(`x.${extension}`)]),
    unknown: run("mimeTypeFromFilename (unknown)", Array.from({ length: 300 }, () => `x.${rng.several(alphabet, 1, 6)}`).filter((name) => !table.has(name.slice(2))), mimeTypeFromFilename),
    names: run("mimeTypeFromFilename", names, mimeTypeFromFilename)
  });
};

// ---------------------------------------------------------------------------
// parser
// ---------------------------------------------------------------------------

/** The instant "now" is while messages are parsed: an unreadable Date header reads as it. */
const FROZEN_NOW = 1795000000000;

const freezeNow = () => {
  const RealDate = Date;
  class FrozenDate extends RealDate {
    constructor(...args: unknown[]) {
      if (args.length === 0) super(FROZEN_NOW);
      else super(...(args as [number]));
    }

    static now() {
      return FROZEN_NOW;
    }
  }
  globalThis.Date = FrozenDate as DateConstructor;
  return () => {
    globalThis.Date = RealDate;
  };
};

const PARSER_CHUNK_SIZES = [65536, 1, 7, 1000];

const parseOnce = async (raw: Uint8Array, chunkSize: number) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "golden-parser-"));
  const attachmentDir = path.join(root, "attachments");
  async function* chunks() {
    for (let i = 0; i < raw.length; i += chunkSize) yield Buffer.from(raw.subarray(i, i + chunkSize));
  }
  try {
    const parsed = await parseMessage(chunks(), { attachmentDir });
    const files = fs.existsSync(attachmentDir) ? fs.readdirSync(attachmentDir).sort((a, b) => Number(a) - Number(b)) : [];
    return {
      ok: {
        ...(compactDeep({ ...parsed, date: undefined, attachments: undefined }) as object),
        date: parsed.date?.getTime(),
        attachments: parsed.attachments.map((attachment) => compactDeep({ ...attachment, path: path.relative(attachmentDir, attachment.path) })),
        // Every file in the attachment directory, with what was written to it.
        files: files.map((name) => {
          const content = fs.readFileSync(path.join(attachmentDir, name));
          return content.length > 256 ? { name, sha256: sha256(content), length: content.length } : { name, hex: hex(content) };
        })
      }
    };
  } catch (error) {
    return { error: errorText(error) };
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
};

const parserFixture = async () => {
  const restore = freezeNow();
  const cases: unknown[] = [];
  try {
    const all: ParserCase[] = parserCases(random(1010));
    for (const entry of all) {
      const raw = rawBytes(entry.raw);
      const sizes = raw.length > 200_000 ? [65536, 1000] : raw.length > 20_000 ? [65536, 7, 1000] : PARSER_CHUNK_SIZES;
      const results: string[] = [];
      for (const size of sizes) results.push(JSON.stringify(await parseOnce(raw, size)));
      if (results.some((result) => result !== results[0])) throw new Error(`parser: chunking changes the result of ${entry.name}`);
      const result = JSON.parse(results[0]);
      if (hasLoneSurrogate(result)) {
        knownDifferences.push({ function: "parseMessage", input: entry, typescript: results[0], rust: wellFormed(result), why: LONE_SURROGATE_WHY });
        continue;
      }
      cases.push({ name: entry.name, raw: entry.raw, chunkSizes: sizes, result });
    }
  } finally {
    restore();
  }
  writeFixture("parser", { cases, frozenNow: [FROZEN_NOW] });
};

// ---------------------------------------------------------------------------

/**
 * The TypeScript parser never closes the file of a part whose headers are cut
 * short by the end of the enclosing multipart. Bun reports each such handle
 * as an error when it collects it; they are counted here, not fatal.
 */
let leakedFileHandles = 0;
process.on("uncaughtException", (error) => {
  if (!String(error.message).includes("FileHandle object was closed during garbage collection")) throw error;
  leakedFileHandles++;
});

const main = async () => {
  if (new Date(2026, 0, 1).getTimezoneOffset() !== 0) throw new Error("Run with TZ unset: the fixtures assume local time is UTC.");
  fs.mkdirSync(FIXTURES, { recursive: true });
  charsetFixture();
  headerFixture();
  addressFixture();
  mimeTextFixture();
  transferDecoderFixture();
  plainTextFixture();
  snippetFixture();
  threadingFixture();
  datesFixture();
  await parserFixture();
  fs.writeFileSync(
    path.join(FIXTURES, "known-differences.json"),
    `[\n${knownDifferences.map((entry) => JSON.stringify(entry)).join(",\n")}\n]\n`
  );
  counts["known-differences"] = knownDifferences.length;
  for (const [name, count] of Object.entries(counts)) console.log(String(count).padStart(6), name);
  if (leakedFileHandles) console.log(`${leakedFileHandles} file handles leaked by the TypeScript parser were collected`);
};

await main();
