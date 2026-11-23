/**
 * Golden fixtures for the Rust port of the IMAP parsers and pure helpers.
 *
 * Runs the Bun server's own code over a large, mostly hostile corpus and
 * records what it returned (or threw). `tests/imap_golden.rs` replays every
 * case against the Rust port and requires identical results.
 *
 * Run from the repository root, after `bun install`:
 *
 *   bun src/server-rs/tests/golden/imap/generate.ts
 *
 * The corpus is deterministic (a seeded generator, no clock, no network), so
 * a rerun rewrites the same files unless the TypeScript changed.
 */
import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

// Environment the TypeScript reads at import time, set before any import.
process.env.TZ = "UTC";
process.env.NODE_ENV = "test";
process.env.IMAP_TRACE = "1";
delete process.env.SSL_CERTIFICATE;
delete process.env.SSL_CERTIFICATE_KEY;
delete process.env.EMAIL_DOMAIN;

const here = import.meta.dir;
const imap = path.resolve(here, "../../../../server/lib/imap");

// The `server` barrel first: it fixes the order the module graph loads in.
await import(path.resolve(imap, "../../index.ts"));
const P = await import(`${imap}/parsers/primitive-parsers.ts`);
const { parseCommand } = await import(`${imap}/parsers/command-parser.ts`);
const fetchParsers = await import(`${imap}/parsers/fetch-parsers.ts`);
const searchParsers = await import(`${imap}/parsers/search-parsers.ts`);
const storeParsers = await import(`${imap}/parsers/store-parsers.ts`);
const authParsers = await import(`${imap}/parsers/auth-parsers.ts`);
const appendParser = await import(`${imap}/parsers/append-parser.ts`);
const mailboxParsers = await import(`${imap}/parsers/mailbox-parsers.ts`);
const util = await import(`${imap}/util.ts`);
const seq = await import(`${imap}/sequence-resolver.ts`);
const { SessionBuffer } = await import(`${imap}/session-buffer.ts`);
const sessionUtils = await import(`${imap}/session-utils.ts`);
const fetchHelpers = await import(`${imap}/fetch-helpers.ts`);
const { getCapabilities } = await import(`${imap}/capabilities.ts`);
const sso = await import(path.resolve(imap, "../sso/index.ts"));
const { logger } = await import(path.resolve(imap, "../logger.ts"));

if (new Date(2026, 0, 1).getTimezoneOffset() !== 0) {
  throw new Error("The fixtures must be generated in UTC");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Deterministic pseudo-random numbers (mulberry32). */
const rng = (seed: number) => {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
};
const pick = <T>(random: () => number, items: readonly T[]): T => items[Math.floor(random() * items.length)];
const int = (random: () => number, max: number): number => Math.floor(random() * max);

/** What a call returned as plain JSON, or what it threw. */
const capture = (run: () => unknown): unknown => {
  try {
    const value = run();
    return value === undefined ? null : JSON.parse(JSON.stringify(value));
  } catch (error) {
    return { threw: String(error) };
  }
};

/** One case per line: compact, and a changed case is a one-line diff. */
const lines = (cases: unknown[]): string => `[\n${cases.map((entry) => JSON.stringify(entry)).join(",\n")}\n]`;
const write = (name: string, data: unknown[] | ({ cases: unknown[] } & Record<string, unknown>)): number => {
  if (Array.isArray(data)) {
    fs.writeFileSync(path.join(here, name), lines(data) + "\n");
    return data.length;
  }
  const { cases, ...rest } = data;
  const head = Object.entries(rest).map(([key, value]) => `${JSON.stringify(key)}: ${JSON.stringify(value)},\n`);
  fs.writeFileSync(path.join(here, name), `{\n${head.join("")}"cases": ${lines(cases)}\n}\n`);
  return cases.length;
};

/** Long strings as their digest, so a megabyte of base64 is not a fixture. */
const shrink = (value: unknown): unknown => {
  if (typeof value === "string" && value.length > 1500) {
    return { $sha256: crypto.createHash("sha256").update(value, "utf8").digest("hex"), $length: value.length };
  }
  if (Array.isArray(value)) return value.map(shrink);
  if (value && typeof value === "object") return Object.fromEntries(Object.entries(value).map(([key, inner]) => [key, shrink(inner)]));
  return value;
};

const counts: Record<string, number> = {};
const MESSAGE = (size: number) => ({ path: "/spool/message", size });

// ---------------------------------------------------------------------------
// Command lines
// ---------------------------------------------------------------------------

type CommandCase = { line: string; literals?: unknown[] };
const commands: CommandCase[] = [];
const seen = new Set<string>();
const command = (line: string, literals?: unknown[]) => {
  const key = JSON.stringify([line, literals ?? null]);
  if (seen.has(key)) return;
  seen.add(key);
  commands.push(literals === undefined ? { line } : { line, literals });
};

const SEQUENCE_SETS = [
  "1", "1:5", "1:*", "*", "*:*", "*:1", "5:1", "1,3,5", "1:3,5,7:9", "2:4,6:*", "0", "0:0", "01:005", "1,", ",1", "1,,2",
  "1:", ":5", "1:2:3", "1 , 2", "1, 2", "1 ,2", "4294967295", "4294967296", "9007199254740991", "9007199254740993",
  "99999999999999999999", "9".repeat(400), "1:" + "9".repeat(30), "-1", "1.5", "a", "1:a", "*:a", "1,a", "$", "1:*,*:1",
  Array.from({ length: 200 }, (_, i) => i + 1).join(","),
  Array.from({ length: 100 }, (_, i) => `${i * 3 + 1}:${i * 3 + 2}`).join(","),
];

const MAILBOXES = [
  "INBOX", "inbox", "Inbox", '"INBOX"', "Drafts", '"Sent Messages"', '"Sent Messages/accounts/alice"', "INBOX/accounts/bob",
  '"with \\"quote\\""', '"back\\\\slash"', '""', "Archive/2025", "caf\u00e9", '"caf\u00e9 \u{1F600}"', "&AOk-", "a*b", "a%b", "(x)",
  '"unterminated', "{5}", "{5+}", "~x", "x]y", "[x", "a\\b", '"tab\there"', '"\\x"', '"trailing\\"',
];

const FETCH_ITEMS = [
  "ENVELOPE", "FLAGS", "INTERNALDATE", "RFC822", "RFC822.HEADER", "RFC822.SIZE", "RFC822.TEXT", "UID", "BODYSTRUCTURE", "MODSEQ",
  "BODY", "BODY.PEEK", "BODY[]", "BODY.PEEK[]", "BODY[HEADER]", "BODY[TEXT]", "BODY.PEEK[HEADER]", "BODY.PEEK[TEXT]",
  "BODY[1]", "BODY[1.2]", "BODY[1.2.3]", "BODY[1.HEADER]", "BODY[1.TEXT]", "BODY[2.MIME]", "BODY.PEEK[1.2.MIME]", "BODY[0]",
  "BODY[HEADER.FIELDS (FROM TO)]", "BODY.PEEK[HEADER.FIELDS (From To Subject Date Message-ID)]", "BODY[HEADER.FIELDS.NOT (BCC)]",
  "BODY.PEEK[HEADER.FIELDS.NOT (X-A X-B)]", "BODY[HEADER.FIELDS ()]", "BODY[HEADER.FIELDS]", "BODY[HEADER.FIELDS FROM]",
  "BODY[HEADER.FIELDS (FROM)", "BODY[HEADER.FIELDS (a  b\tc)]", "BODY[HEADER.FIELDS ((nested) x)]", "BODY[HEADER.FIELDS (X.PEEK)]",
  "BODY[]<0.10>", "BODY.PEEK[]<0.2048>", "BODY[TEXT]<100.50>", "BODY[1]<5.5>", "BODY[HEADER]<0.0>", "BODY[]<0>", "BODY[]<.5>",
  "BODY[]<5.>", "BODY[]<a.b>", "BODY[]<1.2", "BODY[]<1.2>x", "BODY[]<1.2><3.4>", "BODY[]<99999999999999999999.1>", "BODY[]< 1.2>",
  "BODY[HEADER.FIELDS (FROM)]<0.5>", "BODY[", "BODY]", "BODY[]]", "BODY[[]", "BODY[1.]", "BODY[.1]", "BODY[1..2]", "BODY[1.X]",
  "BODY[TEXT.1]", "BODY[HEADER.TEXT]", "BODY[MIME]", "BODY[1.mime]", "body[header]", "Body.Peek[Text]", "BODY.PEEK.PEEK[]",
  "BODYX", "BODY[]junk", "BODY.peek[]", "BODYSTRUCTURE[]", "RFC822.PEEK", "RFC822[]", "FOO", "ALL", "FAST", "FULL", "all", "Fast",
  "BODY[\u00e9]", "BODY[HEADER.FIELDS (\u00dcber Stra\u00dfe)]", "BODY[HEADER.\ufb01ELDS (X)]", "BODY[1.TE\u017fT]",
];

const DATES = [
  "1-Jan-2026", "01-Jan-2026", "31-Dec-1999", "29-Feb-2024", "29-Feb-2023", "31-Apr-2026", "0-Jan-2026", "32-Jan-2026", "1-Foo-2026",
  "1-jan-2026", "1-JANUARY-2026", "1-Jan-26", "1-Jan-70", "1-Jan-0", "1-Jan-100", "1-Jan-10000", "1-Jan-275760", "1-Jan-275761",
  "2026-01-01", "2026-1-1", "2026-13-01", "2026-01-32", "2026", "20260101", "12345", "1/2/2026", "13/2/2026", "2026/01/02",
  "Jan-1-2026", "Jan", "1-Jan", "1", "0", "99", "100", "2026-01-01T00:00:00Z", "2026-01-01T10:00:00+05:30", "2026-01-01T25:00:00Z",
  "1-Jan-2026:10:00", "10:00", "1.2.2026", "Jan\u202f1\u202f2026", "1-Jan-2026\u00a0", "1-Jan-2026,", "today", "-1-Jan-2026",
  "+1-Jan-2026", "1--Jan--2026", "1-Jan-2026-", "1-Jan-2026Z", "1-Jan-2026UTC", "1-Jan-2026GMT+1", "1-Jan-2026;", "\u0661-Jan-2026",
  '"1-Jan-2026"', "(1-Jan-2026)", "{10}", "",
];

const ASTRINGS = [
  "alice", "a@b.example", '"two words"', '""', '"quote \\" inside"', '"back\\\\slash"', "{5}", "{5+}", "caf\u00e9", '"\u{1F600}"',
  "NIL", "1", "1:2", "*", "%", "(", ")", '"unterminated', "x)", "\\Seen", "a\x01b", "a\x7fb", "",
];

const tags = ["a1", "A001", "1", "tag.with-dots", "x{1}", "\u00e9", "*", "+", "a\\b", "(", '"a"', "a b", ""];

// --- Commands without arguments, in every casing, with trailing junk ---
for (const name of ["CAPABILITY", "NOOP", "LOGOUT", "DONE", "CHECK", "CLOSE", "EXPUNGE", "IDLE", "STARTTLS", "NAMESPACE", "UNSELECT", "ID"]) {
  for (const spelling of [name, name.toLowerCase(), name[0] + name.slice(1).toLowerCase()]) {
    command(`a1 ${spelling}`);
    command(`a1 ${spelling} extra arguments (ignored)`);
    command(`a1 UID ${spelling}`);
  }
}
command('a1 ID ("name" "client" "version" "1.0")');
command("a1 ID NIL");
for (const tag of tags) {
  command(`${tag} NOOP`);
  command(`${tag}  NOOP`);
}
for (const line of ["", " ", "a1", "a1 ", "  a1 NOOP", "\ta1 NOOP", "\r\na1 NOOP", "\u00a0a1 NOOP", "\u2028a1 NOOP", "\ufeffa1 NOOP", "\u0085a1 NOOP",
  "a1\tNOOP", "a1 NOOP\r\n", "a1 NOOP ", "a1 FOO", "a1 foo bar", "a1 NOOPX", "a1 \u00e9", "a1 UID", "a1 UID ", "a1 UID FOO", "a1 UID UID NOOP",
  "a1 UID UID UID FETCH 1 FLAGS", "a1 uid fetch 1 flags", "a1 Uid Search ALL", "a1 UID (", "a1 (", "a1 \"NOOP\"", "a1 {4}", "a1 *", "a1 %"]) {
  command(line);
}

// --- LOGIN and AUTHENTICATE ---
for (const user of ASTRINGS) {
  for (const password of ["secret", '"p w"', '"p\\"q"', "{6}", "", "p)", "\u00e9\u00e8"]) {
    command(`a1 LOGIN ${user} ${password}`);
  }
}
command("a1 LOGIN {5} {8}", ["admin", "password"]);
command("a1 LOGIN {5+} {8+}", ["admin", "password"]);
command("a1 LOGIN {5+} {8+}", ["admin"]);
command("a1 LOGIN {5+} {8+}", []);
command("a1 LOGIN {5+} {8+}");
command("a1 LOGIN {5+} {9+}", ["admin", "password "]);
command("a1 LOGIN {5+} {9+}", ["admin", "p\u00e4ssword"]);
command('a1 LOGIN {5+} "p@ss word"', ["admin"]);
command("a1 LOGIN {5} {8}", [MESSAGE(5), "password"]);
command("a1 LOGIN {5} {8}", ["admin", MESSAGE(8)]);
command("a1 LOGIN {7+}", ["admin\r\n"]);
command("a1 LOGIN {x} y", ["admin"]);
command("a1 LOGIN {5 y", ["admin"]);
command("a1 LOGIN {} y", ["admin"]);
command("a1 LOGIN {+} y", ["admin"]);
command("a1 LOGIN {5++} y", ["admin"]);
command("a1 LOGIN {-5} y", ["admin"]);
command("a1 LOGIN { 5} y", ["admin"]);
command("a1 LOGIN {5}{3}", ["admin", "abc"]);
command("a1 login admin secret");
command("a1 LOGIN  admin   secret  ");
command("a1 LOGIN admin secret extra");
const plain = (text: string) => Buffer.from(text, "utf8").toString("base64");
for (const mechanism of ["PLAIN", "plain", "LOGIN", "XOAUTH2", '"PLAIN"', "{5}", "CRAM-MD5", "", "(", "\u00e9"]) {
  for (const response of [undefined, "", plain("\0admin\0secret"), plain("admin\0admin\0secret"), plain("\0\0"), plain("no nulls"), "=", "====",
    "!!!not-base64!!!", '"quoted"', "{4}", "a b", "(", plain("\0adm\u00edn\0p\u00e4ss"), "A".repeat(1000)]) {
    command(response === undefined ? `a1 AUTHENTICATE ${mechanism}` : `a1 AUTHENTICATE ${mechanism} ${response}`);
  }
}
command("a1 AUTHENTICATE PLAIN {4}", ["abcd"]);
command("a1 AUTHENTICATE {5} {4}", ["PLAIN", "abcd"]);
command("a1 AUTHENTICATE PLAIN {4}", [MESSAGE(4)]);

// --- Mailbox commands ---
for (const name of ["SELECT", "EXAMINE", "CREATE", "DELETE", "SUBSCRIBE", "UNSUBSCRIBE", "GETQUOTAROOT"]) {
  for (const mailbox of MAILBOXES) {
    command(`a1 ${name} ${mailbox}`);
    command(`a1 ${name.toLowerCase()}  ${mailbox} trailing`);
  }
  command(`a1 ${name}`);
  command(`a1 ${name} `);
  command(`a1 ${name} {5}`, ["INBOX"]);
  command(`a1 ${name} {5+}`, ["caf\u00e9"]);
  command(`a1 ${name} {5}`, [MESSAGE(5)]);
  command(`a1 ${name} {5}`);
}
for (const oldName of MAILBOXES.slice(0, 12)) {
  for (const newName of ["New", '"New Name"', "", "{3}", '"unterminated', "INBOX"]) {
    command(`a1 RENAME ${oldName} ${newName}`, newName === "{3}" ? ["new"] : undefined);
  }
}
command("a1 RENAME {3} {3}", ["old", "new"]);
command("a1 RENAME {7+}", ["Oldie\r\n"]);
for (const name of ["LIST", "LSUB", "list", "Lsub"]) {
  for (const reference of ['""', "INBOX", '"INBOX/"', "{0}", "", "(", '"unterminated', "~", "caf\u00e9"]) {
    for (const pattern of ["*", "%", '"*"', "INBOX", '"INBOX/%"', '"%/accounts"', "INBOX*", "*%*", "a\\b", "", "(SPECIAL-USE)", "{1}", '"un', "\u00e9*", "a b"]) {
      const literals = [reference, pattern].filter((part) => part.startsWith("{")).map(() => "x");
      command(`a1 ${name} ${reference} ${pattern}`, literals.length ? literals : undefined);
    }
  }
}
command('a1 LIST (SPECIAL-USE) "" "*"');
command('a1 LIST "" "*" RETURN (SPECIAL-USE)');
const STATUS_ITEMS = ["MESSAGES", "RECENT", "UIDNEXT", "UIDVALIDITY", "UNSEEN", "HIGHESTMODSEQ"];
for (const mailbox of MAILBOXES.slice(0, 10)) {
  command(`a1 STATUS ${mailbox} (${STATUS_ITEMS.join(" ")})`);
  command(`a1 STATUS ${mailbox} (messages unseen)`);
}
for (const items of ["(MESSAGES)", "()", "( MESSAGES  UNSEEN )", "(MESSAGES", "MESSAGES", "(MESSAGES INVALID)", "(INVALID)", "", "(MESSAGES))", "((MESSAGES))",
  "(MESSAGES UNSEEN) extra", "(MESSAGES,UNSEEN)", "(\"MESSAGES\")", "(MESSAGES \\Seen)", "(*)", "(SIZE)", "(DELETED)", "(me\u017fsages)", "({8})"]) {
  command(`a1 STATUS INBOX ${items}`);
}
command("a1 STATUS {5+}", ["INBOX"]);
command("a1 STATUS {5+} (MESSAGES)", ["INBOX"]);

// --- ENABLE ---
for (const capabilities of ["CONDSTORE", "condstore qresync", "CONDSTORE UTF8=ACCEPT", "", "  A  B  ", "(CONDSTORE)", "A (B) C", '"CONDSTORE"', "\u00e9 stra\u00dfe", "A*B"]) {
  command(`a1 ENABLE ${capabilities}`);
}

// --- FETCH ---
for (const set of SEQUENCE_SETS) {
  command(`a1 FETCH ${set} FLAGS`);
  command(`a1 UID FETCH ${set} (FLAGS UID)`);
}
for (const item of FETCH_ITEMS) {
  command(`a1 FETCH 1 ${item}`);
  command(`a1 FETCH 1:* (${item})`);
  command(`a1 FETCH 1 (UID ${item} FLAGS)`);
  command(`a1 UID FETCH 1 ${item}`);
  command(`a1 FETCH 1 ${item.toLowerCase()}`);
  command(`a1 FETCH 1 ${item} (CHANGEDSINCE 5)`);
}
for (const items of ["()", "(", ")", "(FLAGS", "(FLAGS))", "((FLAGS))", "(FLAGS  UID)", "( FLAGS )", "(FLAGS,UID)", "FLAGS UID", "(FLAGS) UID", "", " ",
  "(ALL)", "(FAST)", "ALL FLAGS", "(FLAGS ALL)", "(UID RFC822.SIZE INTERNALDATE FLAGS)", "(UID FLAGS ENVELOPE BODYSTRUCTURE)", "(UID BODY.PEEK[])",
  "(FLAGS BODY.PEEK[HEADER.FIELDS (DATE FROM)] BODY[TEXT]<0.100>)", "(BODY[HEADER.FIELDS (A B)] BODY[HEADER.FIELDS.NOT (C)])", "(FLAGS \"UID\")",
  "(FLAGS {3})", "(FLAGS\tUID)", "(FLAGS\r\nUID)", "(BODY[HEADER.FIELDS (A\r\nB)])", "(BODY[HEADER.FIELDS (A\u2028B)])", "(BODY[\n])", "BODY[]\n"]) {
  command(`a1 FETCH 1 ${items}`);
}
for (const modifier of ["(CHANGEDSINCE 12345)", "(changedsince 1)", "( CHANGEDSINCE  7 )", "(CHANGEDSINCE)", "(CHANGEDSINCE x)", "(CHANGEDSINCE 1", "()", "(",
  "(UNCHANGEDSINCE 1)", "(CHANGEDSINCE 1 CHANGEDSINCE 2)", "(CHANGEDSINCE 1 VANISHED)", "(CHANGEDSINCE 99999999999999999999)", "(CHANGEDSINCE -1)",
  "(CHANGEDSINCE 1) trailing", "CHANGEDSINCE 1", "(\"CHANGEDSINCE\" 1)", "(CHANGEDSINCE 1)(CHANGEDSINCE 2)", "(CHANGEDSINCE 0)", "(\u00e9 1)"]) {
  command(`a1 FETCH 1:* (FLAGS) ${modifier}`);
  command(`a1 UID FETCH 1:* FLAGS ${modifier}`);
  command(`a1 FETCH 1 ALL ${modifier}`);
}
command("a1 FETCH");
command("a1 FETCH ");
command("a1 FETCH 1");
command("a1 FETCH FLAGS");
command("a1 FETCH (FLAGS)");

// --- STORE ---
const STORE_OPERATIONS = ["FLAGS", "FLAGS.SILENT", "+FLAGS", "+FLAGS.SILENT", "-FLAGS", "-FLAGS.SILENT"];
const FLAG_LISTS = ["(\\Seen)", "(\\Seen \\Flagged)", "()", "\\Deleted", "(\\Seen", "\\Seen \\Flagged", "(custom $Forwarded)", "(\\Seen\\Flagged)", "( \\Seen )",
  "", "({)", "(\\)", "\\", "(\"quoted\")", "(*)", "(a%b)", "(\u00e9)", "(\\Seen) extra", "NIL", "(a\tb)"];
for (const operation of STORE_OPERATIONS) {
  for (const flags of FLAG_LISTS) {
    command(`a1 STORE 1 ${operation} ${flags}`);
    command(`a1 UID STORE 1:5 ${operation.toLowerCase()} ${flags}`);
  }
  for (const modifier of ["(UNCHANGEDSINCE 12345)", "(unchangedsince 0)", "(UNCHANGEDSINCE)", "(UNCHANGEDSINCE x)", "(CHANGEDSINCE 1)", "()", "(UNCHANGEDSINCE 1",
    "( UNCHANGEDSINCE 5 )", "(UNCHANGEDSINCE 1 UNCHANGEDSINCE 2)", "(UNCHANGEDSINCE 18446744073709551616)"]) {
    command(`a1 STORE 1:3 ${modifier} ${operation} (\\Seen)`);
    command(`a1 UID STORE 7 ${modifier} ${operation} \\Deleted`);
    command(`a1 STORE 1:3${modifier}${operation} (\\Seen)`);
  }
}
for (const operation of ["BADOP", "FLAGS.silent", "+flags", "++FLAGS", "FLAGS.SILENT.SILENT", "+", "-", "", "(FLAGS)", "\"FLAGS\"", "X-FLAGS", "+FLAGS.", "FLAG\u017f"]) {
  command(`a1 STORE 1 ${operation} (\\Seen)`);
}
for (const set of SEQUENCE_SETS.slice(0, 24)) {
  command(`a1 STORE ${set} +FLAGS (\\Seen)`);
  command(`a1 COPY ${set} Archive`);
  command(`a1 UID MOVE ${set} "Sent Messages"`);
}
command("a1 STORE");
command("a1 STORE 1");
command("a1 STORE 1 ");

// --- COPY and MOVE ---
for (const name of ["COPY", "MOVE", "copy", "UID COPY", "UID MOVE", "uid move"]) {
  for (const mailbox of MAILBOXES) {
    command(`a1 ${name} 1:3 ${mailbox}`);
  }
  command(`a1 ${name}`);
  command(`a1 ${name} 1:3`);
  command(`a1 ${name} 1:3 `);
  command(`a1 ${name} INBOX`);
  command(`a1 ${name} 1:3 {5}`, ["INBOX"]);
  command(`a1 ${name} 1:3 {5}`, [MESSAGE(5)]);
  command(`a1 ${name} 1:3 INBOX extra`);
}

// --- SEARCH ---
const FLAG_KEYS = ["ALL", "ANSWERED", "DELETED", "FLAGGED", "NEW", "OLD", "RECENT", "SEEN", "UNANSWERED", "UNDELETED", "UNFLAGGED", "UNSEEN", "DRAFT", "UNDRAFT"];
const DATE_KEYS = ["BEFORE", "ON", "SINCE", "SENTBEFORE", "SENTON", "SENTSINCE"];
const STRING_KEYS = ["FROM", "TO", "CC", "BCC", "SUBJECT", "BODY", "TEXT"];
for (const key of FLAG_KEYS) {
  command(`a1 SEARCH ${key}`);
  command(`a1 SEARCH ${key.toLowerCase()}`);
  command(`a1 UID SEARCH ${key} ${key}`);
  command(`a1 SEARCH NOT ${key}`);
  command(`a1 SEARCH OR ${key} SEEN`);
}
for (const key of DATE_KEYS) {
  for (const date of DATES) {
    command(`a1 SEARCH ${key} ${date}`);
  }
  command(`a1 SEARCH ${key}`);
  command(`a1 SEARCH ${key.toLowerCase()} 1-jan-2026 SEEN`);
  command(`a1 SEARCH NOT ${key} 1-Jan-2026`);
  command(`a1 SEARCH ${key} {10}`, ["1-Jan-2026"]);
}
for (const key of STRING_KEYS) {
  for (const value of ASTRINGS) {
    const literals = value.startsWith("{") ? ["hello"] : undefined;
    command(`a1 SEARCH ${key} ${value}`, literals);
    command(`a1 UID SEARCH ${key.toLowerCase()} ${value} UNSEEN`, literals);
  }
  command(`a1 SEARCH ${key}`);
  command(`a1 SEARCH ${key} {5}`, [MESSAGE(5)]);
  command(`a1 SEARCH ${key} {5}`);
  command(`a1 SEARCH ${key} {3+} ${key} {3+}`, ["abc", "d f"]);
}
for (const field of ["Message-Id", '"X-Custom"', "{4}", "", "(", "x:y"]) {
  for (const value of ["value", '"<abc@host>"', '""', "{5}", "", ")"]) {
    const literals = [field, value].filter((part) => part.startsWith("{")).map((_, index) => `lit${index}`);
    command(`a1 SEARCH HEADER ${field} ${value}`, literals.length ? literals : undefined);
  }
}
for (const key of ["KEYWORD", "UNKEYWORD"]) {
  for (const flag of ["$Forwarded", "custom", "\\Seen", '"quoted"', "", "(", "{3}", "a*b", "\u00e9"]) {
    command(`a1 SEARCH ${key} ${flag}`);
  }
}
for (const key of ["LARGER", "SMALLER"]) {
  for (const size of ["0", "1000", "007", "99999999999999999999", "9".repeat(350), "-1", "1.5", "1k", "abc", "", '"5"', "(5)"]) {
    command(`a1 SEARCH ${key} ${size}`);
  }
}
for (const set of SEQUENCE_SETS) {
  command(`a1 SEARCH ${set}`);
  command(`a1 SEARCH UID ${set}`);
  command(`a1 UID SEARCH UID ${set} SEEN`);
  command(`a1 SEARCH SEEN ${set} FLAGGED`);
}
for (const criteria of [
  "", " ", "FOO", "FOO SEEN", "SEEN FOO", "FOO BAR BAZ", "NOT", "NOT NOT", "NOT FOO", "NOT NOT SEEN", "NOT NOT NOT NOT SEEN", "OR", "OR SEEN", "OR SEEN FOO",
  "OR FOO SEEN", "OR SEEN ANSWERED", "OR NOT SEEN FLAGGED", "OR OR SEEN FLAGGED DELETED", "OR SEEN OR FLAGGED DELETED", "OR OR OR A B C D",
  "OR OR OR SEEN FLAGGED DELETED DRAFT", "NOT OR SEEN FLAGGED", "OR NOT OR SEEN FLAGGED NOT DRAFT", "NOT SEEN SINCE 1-Jan-2026", "SINCE 1-Jan-2026 NOT SEEN",
  "OR FROM alice TO bob SUBJECT \"x y\"", "NOT FROM", "OR FROM a", "OR FROM a TO", "NOT 1:5", "OR 1 2", "NOT UID 1:*", "OR UID 1 UID", "UID", "UID SEEN",
  "(SEEN)", "(", ")", "SEEN (FLAGGED)", "SEEN)", "\"SEEN\"", "\\Seen", "{4}", "SEEN {4}", "*", "* SEEN", "%", "SEEN %", "1:* SEEN", "1,2,3", "1, SEEN", "1:",
  "1:a", "SEEN\tFLAGGED", "SEEN\r\nFLAGGED", "SEEN  FLAGGED  ", "CHARSET UTF-8 SEEN", "CHARSET UTF-8 SUBJECT \"x\"", "RETURN (MIN MAX) SEEN", "MODSEQ 5",
  "HEADER", "HEADER X", "HEADER X Y Z", "LARGER 5 SMALLER 10", "BEFORE 1-Jan-2026 SINCE 1-Jan-2025", "ON 1-Jan-2026 ON", "s\u00e9en", "\u017feen", "SEEN\u00a0FLAGGED",
  "NOT NOT", "OR SEEN SEEN SEEN", "OR (SEEN) FLAGGED", "NOT \"x\"", "OR \"a\" \"b\"", "SUBJECT \"a\" (", "TEXT {3} NOT", "KEYWORD a UNKEYWORD b NOT KEYWORD c",
]) {
  command(`a1 SEARCH ${criteria}`, criteria.includes("{") ? ["abc"] : undefined);
  command(`a1 UID SEARCH ${criteria}`, criteria.includes("{") ? ["abc"] : undefined);
}
command("a1 SEARCH");
command("a1 SEARCH SUBJECT {3+}", ["abc"]);
command("a1 SEARCH SUBJECT {3+} FROM {3+}", ["abc", "def"]);
command("a1 SEARCH SUBJECT {3+} FROM {3+}", ["abc"]);

// --- APPEND ---
for (const mailbox of MAILBOXES.slice(0, 14)) {
  command(`a1 APPEND ${mailbox} {10}`, [MESSAGE(10)]);
}
for (const flags of ["", "()", "(\\Seen)", "(\\Seen \\Flagged)", "( \\Seen )", "(\\Seen", "(custom)", "(\\Seen))", "((\\Seen))", "(\\Seen {5})", "({)", "(\"x\")",
  "\\Seen", "(a  b)", "(*)", "(\u00e9)"]) {
  for (const date of ["", '"25-Feb-2026 10:30:00 -0800"', '" 5-Feb-2026 10:30:00 +0000"', '""', '"not a date"', '"unterminated', "25-Feb-2026", '"a" "b"']) {
    for (const literal of ["{10}", "{10+}", "{0}", "", "{x}", "{10", "10", "{10} trailing"]) {
      command(["a1 APPEND INBOX", flags, date, literal].filter(Boolean).join(" "), [MESSAGE(10)]);
    }
  }
}
command("a1 APPEND INBOX {5}", ["hello"]);
command("a1 APPEND INBOX {5}", []);
command("a1 APPEND INBOX {5}");
command("a1 APPEND {5} {5}", [MESSAGE(5), MESSAGE(5)]);
command("a1 APPEND {5} {5}", ["INBOX", MESSAGE(5)]);
command("a1 APPEND {5} (\\Seen) {5}", ["INBOX", MESSAGE(5)]);
command("a1 APPEND {5} {5}", ["INBOX", "hello"]);
command("a1 APPEND", [MESSAGE(5)]);
command("a1 APPEND ", [MESSAGE(5)]);
command("a1 APPEND INBOX", [MESSAGE(5)]);
command("a1 APPEND INBOX {abc}\r\ndata");
command("a1 APPEND INBOX (\\Seen) \"25-Feb-2026 10:30:00 -0800\" {13}", [MESSAGE(13), "extra"]);
command("a1 append inbox (\\seen) {1}", [MESSAGE(1)]);
command("a1 UID APPEND INBOX {1}", [MESSAGE(1)]);

// --- Mutations of valid commands ---
const SPECIALS = [" ", "  ", '"', "(", ")", "{", "}", "[", "]", "<", ">", "\\", "*", "%", ":", ",", ".", "+", "-", "\r", "\n", "\r\n", "\t", "\0", "\x7f",
  "\u00e9", "\u{1F600}", "\u00a0", "\u2028", "{3}", "{3+}", "NIL", "0", "9", "a", "Z"];
const valid = commands.filter((entry) => parseCommand(entry.line, entry.literals as never).success);
const mutate = (random: () => number, line: string): string => {
  let out = line;
  for (let round = 1 + int(random, 3); round > 0; round--) {
    const at = int(random, out.length + 1);
    switch (int(random, 7)) {
      case 0:
        out = out.slice(0, at) + out.slice(at + 1);
        break;
      case 1:
        out = out.slice(0, at) + pick(random, SPECIALS) + out.slice(at);
        break;
      case 2:
        out = out.slice(0, at);
        break;
      case 3: {
        const end = Math.min(out.length, at + 1 + int(random, 6));
        out = out.slice(0, end) + out.slice(at, end) + out.slice(end);
        break;
      }
      case 4:
        out = out.slice(0, at) + out.slice(at, at + 1).toUpperCase() + out.slice(at + 1);
        break;
      case 5:
        out = out.slice(0, at) + out.slice(at, at + 1).toLowerCase() + out.slice(at + 1);
        break;
      default:
        out = out.slice(0, at) + out.slice(at + 1, at + 2) + out.slice(at, at + 1) + out.slice(at + 2);
    }
  }
  // A lone surrogate cannot come off the wire (the line is decoded UTF-8)
  // and cannot round-trip through a JSON fixture.
  // Typed by hand: the project's `lib` predates these two String methods.
  const text = out as unknown as { isWellFormed(): boolean; toWellFormed(): string };
  return text.isWellFormed() ? out : text.toWellFormed();
};
{
  const random = rng(20261121);
  for (let i = 0; i < 2500; i++) {
    const base = pick(random, valid);
    const literals = int(random, 4) === 0 ? [pick(random, ["x", "two words", "caf\u00e9", ""]), MESSAGE(3), "third"].slice(0, 1 + int(random, 3)) : base.literals;
    command(mutate(random, base.line), literals);
  }
}

// --- Token soup ---
{
  const random = rng(4242);
  const TOKENS = ["a1", "UID", "FETCH", "STORE", "SEARCH", "COPY", "MOVE", "APPEND", "LOGIN", "SELECT", "STATUS", "LIST", "1", "1:*", "*", "2,4", "(", ")", "()",
    "FLAGS", "(FLAGS)", "+FLAGS", "-FLAGS.SILENT", "BODY[]", "BODY.PEEK[HEADER.FIELDS", "(FROM", "TO)]", "<0.10>", "NOT", "OR", "SEEN", "FROM", "SINCE",
    "1-Jan-2026", "HEADER", "LARGER", "100", '"q s"', '"', "{3}", "{3+}", "INBOX", "\\Seen", "(CHANGEDSINCE", "(UNCHANGEDSINCE", "5)", "ALL", "FULL", "%", "",
    "KEYWORD", "UID", "BODY", "TEXT", "\u00e9", "RFC822.SIZE", "ENVELOPE", "(MESSAGES", "UNSEEN)", "ON", "BEFORE", "x"];
  for (let i = 0; i < 1500; i++) {
    const words = Array.from({ length: 2 + int(random, 7) }, () => pick(random, TOKENS));
    if (int(random, 3) > 0) words[0] = "a1";
    const line = words.join(int(random, 10) === 0 ? "  " : " ");
    command(line, line.includes("{3") ? (int(random, 3) === 0 ? [MESSAGE(3)] : ["abc", "def"]) : undefined);
  }
}

counts["commands.json"] = write(
  "commands.json",
  commands.map((entry) => ({ ...entry, expected: capture(() => parseCommand(entry.line, entry.literals as never)) }))
);

// --- Nesting as deep as a command line allows, and deeper ---
const sha256 = (text: string) => crypto.createHash("sha256").update(text, "utf8").digest("hex");
const NOT = "NOT ";
const UID = "UID ";
// Each line is its parts repeated and joined: `[text, times]`.
const deep = (
  [
    [["a1 SEARCH ", 1], [NOT, 2000], ["SEEN", 1]],
    [["a1 SEARCH ", 1], [NOT, 16000], ["SEEN", 1]],
    [["a1 SEARCH ", 1], ["OR ", 8000], ["SEEN", 1], [" FLAGGED", 8000]],
    [["a1 SEARCH ", 1], ["OR SEEN ", 7000], ["FLAGGED", 1]],
    [["a1 SEARCH ", 1], ["OR NOT SEEN NOT ", 3000], ["FLAGGED", 1]],
    [["a1 UID SEARCH ", 1], [NOT, 9000], ['FROM "a b"', 1]],
    [["a1 ", 1], [UID, 16000], ["NOOP", 1]],
    [["a1 ", 1], ["uid ", 5000], ["FETCH 1:* (FLAGS)", 1]],
    [["a1 ", 1], [UID, 4000], ["SEARCH ", 1], [NOT, 4000], ["SEEN", 1]],
    [["a1 ", 1], [UID, 3000], ["BOGUS", 1]],
    [["a1 ", 1], [UID, 3000]],
    [["a1 SEARCH ", 1], [NOT, 3000]],
    [["a1 SEARCH ", 1], [NOT, 3000], ["FOO", 1]],
    [["a1 SEARCH ", 1], ["OR ", 3000], ["SEEN", 1]],
    [["a1 SEARCH ", 1], ["OR ", 3000], ["SEEN", 1], [" (", 1]],
    // Past the JavaScript stack: the limit itself moves with the JIT, so these
    // sit far beyond it.
    [["a1 SEARCH ", 1], [NOT, 150000], ["SEEN", 1]],
    [["a1 SEARCH ", 1], ["OR ", 150000], ["SEEN", 1]],
    [["a1 ", 1], [UID, 150000], ["NOOP", 1]],
  ] as [string, number][][]
).map((parts) => {
  const result = parseCommand(parts.map(([text, times]) => text.repeat(times)).join(""));
  const json = JSON.stringify(result);
  return { parts, success: result.success, length: json.length, sha256: sha256(json), error: result.error ?? null };
});
counts["commands_deep.json"] = write("commands_deep.json", deep);

// ---------------------------------------------------------------------------
// Parsers called directly
// ---------------------------------------------------------------------------

type ParserCase = { fn: string; input: string; position?: number; args?: unknown[]; literals?: unknown[] };
const parserCases: ParserCase[] = [];
// The parsers take differently shaped contexts and arguments.
// eslint-disable-next-line @typescript-eslint/no-explicit-any
const direct: Record<string, (context: any, ...args: any[]) => unknown> = {
  parseAtom: P.parseAtom,
  parseDate: P.parseDate,
  parseFlag: P.parseFlag,
  parseLiteral: P.parseLiteral,
  parseMessageLiteral: P.parseMessageLiteral,
  parseString: P.parseString,
  parseListMailbox: P.parseListMailbox,
  parseQuotedString: P.parseQuotedString,
  parseSequenceSet: P.parseSequenceSet,
  parseNumber: P.parseNumber,
  parseModifierGroup: P.parseModifierGroup,
  skipWhitespace: P.skipWhitespace,
  // The code unit, not the character: half of a surrogate pair is no JSON string.
  peek: (context) => {
    const unit = P.peek(context);
    return unit === "" ? null : unit.charCodeAt(0);
  },
  consume: P.consume,
  parseFetch: fetchParsers.parseFetch,
  parseFetchDataItems: fetchParsers.parseFetchDataItems,
  parseFetchDataItem: fetchParsers.parseFetchDataItem,
  parseSearch: searchParsers.parseSearch,
  parseSearchCriteria: searchParsers.parseSearchCriteria,
  parseStore: storeParsers.parseStore,
  parseCopy: storeParsers.parseCopy,
  parseMove: storeParsers.parseMove,
  parseLogin: authParsers.parseLogin,
  parseAuthenticate: authParsers.parseAuthenticate,
  parseAppend: appendParser.parseAppend,
  parseList: (context, name) => mailboxParsers.parseList(name, context),
  parseSelect: (context, readOnly) => mailboxParsers.parseSelect(readOnly, context),
  parseStatus: mailboxParsers.parseStatus,
  parseCreate: mailboxParsers.parseCreate,
  parseDelete: mailboxParsers.parseDelete,
  parseRename: mailboxParsers.parseRename,
  parseSubscribe: mailboxParsers.parseSubscribe,
  parseUnsubscribe: mailboxParsers.parseUnsubscribe,
};
const parser = (fn: string, input: string, extra: Omit<ParserCase, "fn" | "input"> = {}) => parserCases.push({ fn, input, ...extra });

const PRIMITIVE_INPUTS = [
  "", " ", "INBOX", "INBOX rest", "FLAGS)", "(atom", 'atom"rest', "atom\\flag", "atom*rest", "atom%rest", "atom\x01rest", "atom\x7frest", "atom{5}", "a]b[c",
  "\\Seen", "\\Seen \\Flagged", "\\", "\\\\Seen", "\\Seen\\Flagged", "Answered", "$Forwarded", "\\*", '"hello"', '""', '"hello\\"world"', '"hello\\\\world"',
  '"hello', '"a\\', '"a\\"', '"\\x\\y"', '"caf\u00e9 \u{1F600}" rest', '"line\r\nbreak"', '"tab\t"', "hello", "{5}", "{5} rest", "{5+}", "{5+}rest", "{5x}",
  "{5", "{}", "{+}", "{5++}", "{05}", "{ 5}", "{5 }", "{5}\r\n", "{99999999999999999999}", "{-1}", "{5}}", "{{5}", "x{5}", "5", "12345", "42abc", "abc", "007",
  "9".repeat(25), "9".repeat(400), "1:5", "1:*", "*", "*:*", "1,3,5", "1:3,5,7:9", "1:abc", "1,", "1, 2 ,3 rest", " 1", "1 FLAGS", "*x", "1:*:2", "1::2", ":",
  ",", "01-Jan-2024", "not-a-date", "1-Jan-2026 rest", "2026-01-01T00:00:00Z", "   abc", "\tabc", "\u00a0abc", "abc", "caf\u00e9", "\u{1F600}x", "%", "*", "INBOX/%",
  "%/accounts", "INBOX*", "*%*", '"INBOX/%"', "a b", "a(b", "a\\b", "(CHANGEDSINCE 5)", "(CHANGEDSINCE 5) rest", "(changedsince 5)", "( CHANGEDSINCE  5 )",
  "(CHANGEDSINCE)", "(CHANGEDSINCE x)", "(UNCHANGEDSINCE 5)", "(CHANGEDSINCE 5", "()", "(", "  (CHANGEDSINCE 7)", "(CHANGEDSINCE 1 CHANGEDSINCE 2)",
  "(CHANGEDSINCE 5 5)", "(\"CHANGEDSINCE\" 5)", "(CHANGEDSINCE 99999999999999999999)", "FLAGS", "CD",
];
for (const input of PRIMITIVE_INPUTS) {
  for (const fn of ["parseAtom", "parseDate", "parseFlag", "parseString", "parseListMailbox", "parseQuotedString", "parseSequenceSet", "parseNumber", "skipWhitespace", "peek"]) {
    parser(fn, input);
  }
  for (const literals of [undefined, [], ["admin"], ["admin", "password"], [MESSAGE(5)], [MESSAGE(5), "text"], ["caf\u00e9", MESSAGE(1)]]) {
    parser("parseLiteral", input, { literals });
    parser("parseMessageLiteral", input, { literals });
    if (input.startsWith("{")) parser("parseString", input, { literals });
  }
  parser("parseModifierGroup", input, { args: ["FETCH", "CHANGEDSINCE"] });
  parser("parseModifierGroup", input, { args: ["STORE", "UNCHANGEDSINCE"] });
  for (const expected of ["FLAGS", "CD", "(", '"', "", "\u00e9", "INBOX rest and more"]) {
    parser("consume", input, { args: [expected] });
  }
}
for (const [input, position] of [["a", 1], ["ABCDEF", 2], ["ab cd", 2], ["ab cd", 3], ["x 1:5", 2], ['x "q"', 2], ["x {3}", 2], ["abc", 3], ["abc", 7]] as const) {
  for (const fn of ["parseAtom", "parseFlag", "parseString", "parseQuotedString", "parseSequenceSet", "parseNumber", "skipWhitespace", "peek", "parseListMailbox"]) {
    parser(fn, input, { position });
  }
  parser("consume", input, { position, args: ["CD"] });
  parser("parseLiteral", input, { position, literals: ["abc"] });
}
for (const item of FETCH_ITEMS) {
  parser("parseFetchDataItem", item);
  parser("parseFetchDataItem", `${item} FLAGS`);
  parser("parseFetchDataItems", item);
  parser("parseFetchDataItems", `(${item} UID)`);
  parser("parseFetchDataItems", `  (  ${item}  `);
  parser("parseFetch", `1:* ${item}`);
  parser("parseFetch", `1 (${item}) (CHANGEDSINCE 9)`);
}
for (const set of SEQUENCE_SETS) {
  parser("parseSequenceSet", set);
  parser("parseFetch", `${set} FLAGS`);
  parser("parseStore", `${set} FLAGS (\\Seen)`);
  parser("parseCopy", `${set} INBOX`);
  parser("parseMove", `${set} "Sent Messages"`);
  parser("parseSearchCriteria", set);
  parser("parseSearchCriteria", `UID ${set}`);
}
for (const criteria of ["", "ALL", "FOO", "FOO SEEN", "NOT", "NOT SEEN", "OR SEEN", "OR SEEN FLAGGED", "(", "\"x\"", "SEEN (", "*", "%", "{3}", "SUBJECT {3}",
  "FROM alice SUBJECT hello", "BEFORE 01-Jan-2024", "ON", "SINCE x", "HEADER X-Custom value123", "HEADER \"Message-Id\" \"<abc@host>\"", "LARGER 1000", "SMALLER",
  "UID 1:*", "UID", "1:3", "SEEN 1:3", "NOT NOT SEEN", "OR NOT SEEN FLAGGED", "KEYWORD", "KEYWORD x", "  SEEN  ", "SEEN\\", "\\"]) {
  parser("parseSearchCriteria", criteria, { literals: criteria.includes("{") ? ["abc"] : undefined });
  parser("parseSearch", criteria, { literals: criteria.includes("{") ? ["abc"] : undefined });
}
for (const operation of STORE_OPERATIONS) {
  for (const flags of FLAG_LISTS) parser("parseStore", `1:5 ${operation} ${flags}`);
  parser("parseStore", `1 (UNCHANGEDSINCE 7) ${operation} (\\Seen)`);
}
for (const input of ["", "1 ", "abc FLAGS (\\Seen)", "1 BADOP (\\Seen)", "1 FLAGS ({)", "1 FLAGS", "1 (UNCHANGEDSINCE x) FLAGS (\\Seen)", "1 INBOX", "1:10 Trash",
  '1 "My Mailbox"', "abc INBOX", "1 {5}"]) {
  for (const fn of ["parseStore", "parseCopy", "parseMove"]) parser(fn, input, { literals: input.includes("{") ? ["INBOX"] : undefined });
}
for (const input of ["", "user", "user password", '"user@example.com" password', 'user "my password"', 'user "p@ss\\"w0rd!"', 'user "pass\\\\word"',
  '"unterminated password', "  user   password  ", "{5} {8}", "{5} password", "user {8}", "PLAIN", "PLAIN AGFsaWNlAHNlY3JldA==", '"PLAIN"', "PLAIN {4}", "PLAIN (",
  "plain  x  y"]) {
  for (const fn of ["parseLogin", "parseAuthenticate"]) {
    parser(fn, input);
    if (input.includes("{")) parser(fn, input, { literals: ["admin", "password"] });
  }
}
for (const input of ["", "INBOX", "INBOX {4}", '"Sent Items" {12}', "INBOX (MyLabel Important) {4}", "INBOX () {4}", 'INBOX "25-Feb-2026 12:34:56 +0000" {4}',
  'INBOX (Important) "25-Feb-2026 12:34:56 +0000" {4}', "INBOX {abc}\r\ntest", "INBOX {100", "INBOX (\\Seen {4}", "INBOX (\\Seen) {4} trailing", " INBOX {4}",
  "{5} {4}", "INBOX {4}{4}", "INBOX (a)(b) {4}", 'INBOX "d1" "d2" {4}', 'INBOX "unterminated {4}']) {
  for (const literals of [undefined, [], [MESSAGE(4)], ["text"], ["INBOX", MESSAGE(4)], [MESSAGE(4), MESSAGE(4)]]) {
    parser("parseAppend", input, { literals });
  }
}
for (const mailbox of MAILBOXES) {
  for (const fn of ["parseCreate", "parseDelete", "parseSubscribe", "parseUnsubscribe", "parseStatus"]) parser(fn, mailbox);
  parser("parseSelect", mailbox, { args: [false] });
  parser("parseSelect", ` ${mailbox}`, { args: [true] });
  parser("parseRename", `${mailbox} NewName`);
  parser("parseRename", `Old ${mailbox}`);
  parser("parseList", `"" ${mailbox}`, { args: ["LIST"] });
  parser("parseList", `${mailbox} *`, { args: ["LSUB"] });
  parser("parseStatus", `${mailbox} (MESSAGES UNSEEN)`);
}
for (const input of ["INBOX (MESSAGES)", "INBOX (MESSAGES RECENT UIDNEXT UIDVALIDITY UNSEEN HIGHESTMODSEQ)", "INBOX (messages)", "INBOX MESSAGES", "INBOX (INVALID)",
  "INBOX (", "INBOX ()", "INBOX", "", "INBOX (MESSAGES", "INBOX ( MESSAGES ) x"]) {
  parser("parseStatus", input);
}
for (const input of ["", '"" *', '"" %', '"INBOX" *', "INBOX", '"" ', "a b c"]) {
  for (const name of ["LIST", "LSUB", "XLIST"]) parser("parseList", input, { args: [name] });
}

const context = (entry: ParserCase) => ({
  input: entry.input,
  position: entry.position ?? 0,
  length: entry.input.length,
  literals: entry.literals && [...entry.literals],
});
counts["parsers.json"] = write(
  "parsers.json",
  parserCases.map((entry) => {
    const ctx = context(entry);
    const expected = capture(() => direct[entry.fn](ctx, ...(entry.args ?? [])));
    return { ...entry, expected, position_after: ctx.position, literals_after: ctx.literals ? ctx.literals.length : null };
  })
);

// Body expressions and sections, which take a string rather than a context.
const BODY_EXPRESSIONS = [...new Set([...FETCH_ITEMS, ...FETCH_ITEMS.map((item) => item.toUpperCase()), "BODY[] ", "BODY []", "BODY.PEEK[HEADER]<1.2>", "BODY[A]B]",
  "BODY[A]]<1.2>", "BODY[]<1.2>\n", "BODY[\r]", "BODY[\u2029]", "BODY[x]\u2028", "BODY.PEEK", "BODY.PEEK[", "", "BODY[1.HEADER.FIELDS (X)]"])];
const SECTIONS = ["", "HEADER", "header", "TEXT", "Text", "MIME", "1", "1.2", "1.2.3", "0", "01", "1.", ".1", "1..2", "1.HEADER", "1.header", "1.TEXT", "2.MIME", "1.2.Mime",
  "1.FOO", "1.HEADER.FIELDS (X)", "HEADER.FIELDS (FROM TO)", "header.fields (from to)", "HEADER.FIELDS.NOT (BCC)", "HEADER.FIELDS (FROM", "HEADER.FIELDS FROM TO)",
  "HEADER.FIELDS ()", "HEADER.FIELDS ", "HEADER.FIELDS", "HEADER.FIELDS.NOT ", "HEADER.FIELDS.NOT", "HEADER.FIELDS  (a\t b\u00a0c\u2028d)", "HEADER.FIELDS (\u00fcber stra\u00dfe \ufb01)",
  "HEADER.\ufb01ELDS (X Y)", "HEADER.FIELD\u017f (X)", "HEADER.FIELDS.NOT((A)(B))", "HEADER.FIELDS (A) (B)", " HEADER", "HEADER ", "1 ", "\u0661", "1.TE\u017fT", "1.MIM\u0045", "1.\u212aEY"];
counts["body.json"] = write("body.json", [
  ...BODY_EXPRESSIONS.map((input) => ({ fn: "parseBodyFetch", input, expected: capture(() => fetchParsers.parseBodyFetch(input, context({ fn: "", input: "" }))) })),
  ...SECTIONS.map((input) => ({ fn: "parseBodySection", input, expected: capture(() => fetchParsers.parseBodySection(input)) })),
]);

// ---------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------

const dateInputs = new Set<string>(DATES);
{
  const days = ["1", "01", "5", "15", "28", "29", "30", "31", "32", "0", "00", "001"];
  const months = ["Jan", "feb", "MAR", "April", "may", "Jun", "Jul", "Aug", "Sept", "Oct", "Nov", "Dec", "Foo", "Ja", "Janvier", "1", "01", "12", "13", "0"];
  const years = ["2026", "26", "70", "49", "50", "99", "100", "1970", "1969", "0", "00", "1", "9999", "10000", "275760", "275761", "-1", "+2026"];
  const separators = ["-", " ", "/", ".", ",", ", ", "  ", ""];
  for (const day of days) for (const month of months) for (const separator of ["-", " ", "/"]) dateInputs.add(`${day}${separator}${month}${separator}2026`);
  for (const year of years) for (const separator of separators) {
    dateInputs.add(`5${separator}Jan${separator}${year}`);
    dateInputs.add(`Jan${separator}5${separator}${year}`);
    dateInputs.add(`${year}${separator}Jan${separator}5`);
    dateInputs.add(`${year}${separator}01${separator}05`);
    dateInputs.add(`01${separator}05${separator}${year}`);
  }
  const times = ["10:00", "10:00:00", "1:2:3", "23:59:59", "24:00", "24:00:00", "24:00:01", "25:00", "10:60", "10:00:60", "10:00:00.5", "10:00:00.123", "10:00:00.123456",
    "10:00:00.", "10::30", "10:", ":10", "10", "10.30", "12:00 AM", "12:00 PM", "0:00 am", "1:00pm", "13:00 PM", "12 PM", "10:00:00:00", "1:2:3:4:5"];
  const zones = ["", " Z", "Z", " GMT", " UTC", " UT", " EST", " EDT", " CST", " CDT", " MST", " MDT", " PST", " PDT", " CET", " +0100", "+0100", " -0800", " +01:00", " +1", " +12345",
    " GMT+1", " GMT+0100", " GMT-08:00", " UTC+5:30", " +", " -", " GMT+", " (PST)", " +0100 (CET)", " z", " gmt+2", " +99", " +9999", " +400000:00", " +700000:00", " +1200000:30"];
  for (const time of times) for (const zone of zones) {
    dateInputs.add(`Jan 5 2026 ${time}${zone}`);
    dateInputs.add(`5 Jan 2026 ${time}${zone}`);
  }
  for (const time of times) {
    dateInputs.add(`2026-01-05T${time}`);
    dateInputs.add(`2026-01-05 ${time}`);
    dateInputs.add(`${time} Jan 5 2026`);
    dateInputs.add(`${time}`);
  }
  const iso = ["2026", "2026-01", "2026-01-05", "2026-1-5", "2026-13-05", "2026-00-05", "2026-01-00", "2026-01-32", "2026-02-29", "2024-02-29", "2026-02-30", "2026-04-31",
    "+002026-01-05", "-002026-01-05", "-000000-01-05", "+000000-01-05", "+2026-01-05", "02026-01-05", "202-01-05", "20260105", "2026-01-05T", "2026-01-05T10",
    "2026-01-05T10:00", "2026-01-05T10:00:00", "2026-01-05T10:00:00Z", "2026-01-05T10:00:00z", "2026-01-05t10:00:00Z", "2026-01-05T10:00:00.1Z", "2026-01-05T10:00:00.12Z",
    "2026-01-05T10:00:00.123Z", "2026-01-05T10:00:00.1234Z", "2026-01-05T10:00:00.999999999999Z", "2026-01-05T10:00:00,123Z", "2026-01-05T10:00:00+05:30",
    "2026-01-05T10:00:00-0800", "2026-01-05T10:00:00+05", "2026-01-05T10:00:00+5:30", "2026-01-05T10:00:00+24:00", "2026-01-05T10:00:00+23:59", "2026-01-05T10:00:00+23:60",
    "2026-01-05T10:00:00 Z", "2026-01-05T10:00:00Z ", " 2026-01-05T10:00:00Z", "2026-01-05T24:00:00Z", "2026-01-05T24:00:00.000Z", "2026-01-05T24:00:00.001Z", "2026-01-05T24:01:00Z",
    "2026-01-05T1:00:00Z", "2026-01-05T10:0:00Z", "2026-01-05T10:00:0Z", "2026-01-05T10:00:00ZZ", "2026-01-05T10:00:00UTC", "2026-01-05T10:00:00 GMT", "2026-01-05T10:00:00 +0100",
    "2026-01-05 10:00:00Z", "2026-01-05 10:00:00 +0100", "2026-01-05 10:00:00.5", "2026-W01-1", "2026-005", "1970-01-01T00:00:00.000Z", "1969-12-31T23:59:59.999Z",
    "0000-01-01T00:00:00Z", "0001-01-01", "9999-12-31T23:59:59.999Z", "+275760-09-13T00:00:00.000Z", "+275760-09-13T00:00:00.001Z", "-271821-04-20T00:00:00.000Z",
    "-271821-04-19T23:59:59.999Z", "+010000-01-01T00:00:00Z", "10000-01-01", "2026-01-05T10:00:00.9999Z", "1969-12-31T23:59:59.9999Z"];
  for (const value of iso) dateInputs.add(value);
  const misc = ["", " ", "x", "T", "Z", "0", "5", "32", "999999999", "1000000000", "123456789012", "Sat, 21 Nov 2026 05:53:08 GMT", "Sat, 21 Nov 2026 05:53:08 +0100",
    "Sat Nov 21 2026 05:53:08 GMT+0100 (Central European Standard Time)", "Saturday, November 21, 2026", "November 21, 2026", "21 November 2026", "Nov 21", "21 Nov",
    "Nov 2026", "2026 Nov", "Monday, January 5th 2026", "(comment) Jan 5 2026 (x(y)z)", "Jan 5 2026 )", "Jan 5 (2026", "Jan 5 2026 (unclosed", "1/2/3", "1/2", "13/2/2026",
    "2026/13/2", "31/12/2026", "12-31-2026", "2026.01.05", "5.1.2026", "Jan-5-2026", "Jan 5 6 7", "5 6", "5 6 7", "5 6 7 8", "\u00e9 Jan 5 2026", "caf\u00e9 5 Jan 2026",
    "Jan 5 2026 \u00e9", "Jan\u00a05\u00a02026", "Jan\u202f5\u202f2026", "Jan\u20285\u20282026", "Jan\u000b5\u000b2026", "Jan\t5\n2026\r", "Jan\u30005 2026", "Jan\ufeff5 2026",
    "Jan 5 2026\u0000 garbage", "\u0000Jan 5 2026", "January5 2026", "5January2026", "Jan5", "5Jan", "5jan2026", "jan52026", "Mon Jan 05 2026", "Mon, Jan 05 2026", "Mon,Jan,05,2026",
    "mon jan 05 2026", "tue 5 2026", "5 tue 2026", "1e3", "0x10", "Infinity", "NaN", "null", "now", "yesterday", "2026-01-05junk", "junk2026-01-05", "junk 2026-01-05",
    "2026-01-05 junk", "Jan 5 2026 10:00 junk", "Jan 5 2026 10:00 AM PM", "Jan 5 2026 AM", "AM Jan 5 2026", "pm 5 Jan 2026", "Jan 5 2026 10:00 A", "Jan 5 2026 10:00 P.M.",
    "t 5 Jan 2026", "5 Jan 2026 t", "utc 5 Jan 2026", "5 Jan 2026 utc+", "5 Jan 2026 +0100", "5 Jan 2026 10:00 +0100 +0200", "5 Jan 2026 10:00 -0100-", "5 Jan 2026 10:00:00+",
    "5 Jan 2026 10:00:00-08", "5 Jan 2026 10:00:00.5+01:30", "5 Jan 2026 10:00 +1:2", "5 Jan 2026 10:00 +1:", "5 Jan 2026 10:00 +:30", "5 Jan 2026 10:00 +01:99", "5 Jan 2026 10:00 +01:30:15",
    "5-1-2026", "5/1/2026 10:00", "1/5/26", "1/5/70", "0/5/2026", "1/0/2026", "1/32/2026", "12/31/1969 23:59:59", "Dec 31 1969 23:59:59 UTC", "Jan 1 1970 UTC", "1 1 1970 GMT",
    "@", "#5 Jan 2026", "5 Jan 2026 #", "5 @ Jan @ 2026", "5;Jan;2026", "[5 Jan 2026]", "5_Jan_2026", "5~Jan~2026", "5'Jan'2026", "5\"Jan\"2026", "<5 Jan 2026>"];
  for (const value of misc) dateInputs.add(value);
  // Every character that might count as white space, in each place white space matters.
  for (const code of [0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x1c, 0x1f, 0x20, 0x7f, 0x85, 0xa0, 0x1680, 0x180e, 0x2000, 0x2001, 0x2005, 0x200a, 0x200b, 0x2028, 0x2029, 0x202f,
    0x205f, 0x2060, 0x3000, 0xfeff, 0x40, 0x5b, 0x5f, 0x60, 0x7b, 0xe9, 0x1f600]) {
    const ch = String.fromCodePoint(code);
    for (const value of [`Jan${ch}5${ch}2026`, `Jan 5 2026 10:00:00${ch}PM`, `Jan${ch}uary 5 2026`, `${ch}2026-01-05`, `2026-01-05${ch}`, `1${ch}2${ch}2026`,
      `5 Jan 2026 10:00${ch}+0100`, `2026-01-05T10:00:00${ch}Z`, `5${ch}Jan${ch}2026${ch}10:00${ch}GMT`]) {
      dateInputs.add(value);
    }
  }
  const random = rng(77);
  const SOUP = ["5", "05", "31", "2026", "26", "1970", "0", "12", "13", "99", "Jan", "feb", "DEC", "Mon", "Sat,", "10:00", "10:30:15", "23:59:59.999", "1:2", "AM", "pm",
    "Z", "GMT", "UTC", "EST", "PDT", "+0100", "-08:00", "+1", "GMT+2", "T", "-", "/", ".", ",", ":", "(x)", "(", ")", "+", "junk", "\u00e9", "2026-01-05", "T10:00:00Z", "1/2/2026"];
  for (let i = 0; i < 1500; i++) {
    const parts = Array.from({ length: 1 + int(random, 6) }, () => pick(random, SOUP));
    dateInputs.add(parts.join(pick(random, [" ", " ", " ", "-", "", "/", ", "])));
  }
}
counts["dates.json"] = write(
  "dates.json",
  [...dateInputs].map((input) => {
    const date = new Date(input);
    const ms = date.getTime();
    return {
      input,
      ms: Number.isNaN(ms) ? null : ms,
      iso: Number.isNaN(ms) ? null : date.toISOString(),
      utc: date.toUTCString(),
      internal: util.formatInternalDate(date),
    };
  })
);
counts["dates_ms.json"] = write(
  "dates_ms.json",
  [0, 1, -1, 999, 1000, 86399999, 86400000, -86400000, 951782400000, 1795240388123, -62135596800000, -62167219200000, -62198755200000, 253402300799999, 253402300800000,
    8.64e15, -8.64e15, 8.64e15 + 1, -8.64e15 - 1, 4102444800000, 1709164800000, 1677628800000, -2208988800000, 32503680000000, -30610224000000, 1e12, 1e13, 1e14, 1e15].map((ms) => {
    const date = new Date(ms);
    const valid = !Number.isNaN(date.getTime());
    return { ms, iso: valid ? date.toISOString() : null, utc: date.toUTCString(), internal: util.formatInternalDate(date) };
  })
);

// ---------------------------------------------------------------------------
// util.ts
// ---------------------------------------------------------------------------

const utilCases: { fn: string; args: unknown[]; expected: unknown }[] = [];
const utilCall = (fn: string, ...args: unknown[]) => utilCases.push({ fn, args, expected: capture(() => util[fn](...args)) });

const TEXTS = ["", "Hello", "Hello there", "a\r\nb\nc\rd\0e", "a\u2028b\u2029c", 'Re: "quoted" \\ subject', "\\", '"', 'a"b', "a\\b", "ends\\", "one\r\n\r\ntwo", "\r\n", "\0",
  "\u65e5\u672c\u8a9e", "caf\u00e9 \u{1F600}", "tab\there", "  padded  ", 'evil.txt"; filename="wanted.pdf', "report\\", "x".repeat(300), "Hello\r\nFrom: ceo@bank.example\r\nX-Evil: 1"];
for (const text of TEXTS) {
  for (const fn of ["encodeText", "headerFieldValue", "headerQuotedParam", "boundaryToken", "quoteString", "formatNString"]) utilCall(fn, text);
}
utilCall("formatNString", null);
for (const id of ["test-doc.id_1", '<a@b>\r\n"', "caf\u00e9", "a\u{1F600}b", "<x@y>", "a b", "UPPER_lower-123.x", "@#$%^&*()"]) utilCall("boundaryToken", id);
const CONTENT_TYPES = [undefined, "", "application/pdf", "application", "/pdf", "application/", "/", "text/plain", "TEXT/PLAIN", "Text/Html", "a/b/c", 'ap"p/pd\\f',
  "text/plain; charset=utf-8", "x\r\ny/z", "image/jpeg", "multipart/mixed", " text / plain "];
for (const contentType of CONTENT_TYPES) {
  utilCall("mediaTypeParts", contentType ?? null);
  for (const filename of [undefined, "", "a.bin", 'in"voice\\', "na\r\nme.txt", "\u65e5\u672c.txt"]) {
    utilCall("attachmentPartHeaderFields", { ...(contentType === undefined ? {} : { contentType }), ...(filename === undefined ? {} : { filename }) });
  }
}
const ADDRESS_LISTS: unknown[] = [
  null, [], [{ name: "John Doe", address: "john@example.com" }], [{ name: "", address: "john@example.com" }], [{ address: "john@example.com" }],
  [{ name: " ", address: "john@example.com" }], [{ name: 'John "Johnny" Doe', address: "john@example.com" }], [{ name: "Invalid", address: "nodomain" }],
  [{ name: "Empty", address: "" }], [{ name: "NoAddress" }], [{}], [{ name: "Bob\\", address: "bob@example.com" }], [{ name: "A\nB", address: "a@example.com" }],
  [{ name: "X", address: 'we"ird\\local@example.com' }], [{ name: "X", address: 'user@ex"ample.com' }], [{ name: "X", address: "a@b@c" }], [{ name: "X", address: "@domain" }],
  [{ name: "X", address: "local@" }], [{ name: "\u65e5\u672c", address: "\u00fc@b\u00fccher.example" }],
  [{ name: "John Doe", address: "john@example.com" }, { name: "Jane Smith", address: "jane@example.com" }],
  [{ name: "", address: "john@example.com" }, { name: "Bad", address: "bad" }, { name: "Jane Smith", address: "jane@example.com" }],
];
for (const list of ADDRESS_LISTS) utilCall("formatAddressList", list);

const address = (text: string, value: unknown = []) => ({ text, value });
const ATTACHMENT = (overrides: Record<string, unknown> = {}) => ({ content: { data: "att-missing" }, filename: "test.pdf", size: 1024, contentType: "application/pdf", ...overrides });
const MAILS: Record<string, unknown>[] = [
  {},
  { text: "Hello, World!" },
  { html: "<p>Hello, World!</p>" },
  { text: "Hello", html: "<p>Hello</p>" },
  { text: "   ", html: "\n\t" },
  { text: "", html: "" },
  { text: "\u00a0\u2028", html: "<p>x</p>" },
  { text: "hi", text_octets: 0, html_octets: 0 },
  { text_octets: 30, html_octets: 0 },
  { text_octets: 0, html_octets: 30 },
  { text_octets: 27060, html_octets: 11138020 },
  { text_octets: 0, html_octets: 0 },
  { text_octets: 30, html_octets: 40, mail_id: "m1", user_id: "u1" },
  { text_octets: 0, html_octets: 0, mail_id: "m1", user_id: "u1" },
  { text_octets: 31, html_octets: 0, mail_id: "m1", user_id: "u1", attachments: [ATTACHMENT()] },
  { text_octets: 30, mail_id: "m1", user_id: "u1" },
  { text_octets: 30, html_octets: 40, mail_id: "m1", user_id: "u1", text: "loaded anyway" },
  { subject: "Test Subject", messageId: "<test@example.com>", date: "2024-01-15T10:30:00Z" },
  { subject: 'Test "Quoted" Subject', messageId: '<"a\\"b"@example.com>' },
  { subject: "Re: C:\\Users\\", messageId: "<id@example>", from: address("", [{ name: "Bob\\", address: "bob@example.com" }]) },
  { subject: "one\r\n\r\ntwo", messageId: "<a@b>\r\nX-Evil: 1\"", text: "hi", html: "<p>hi</p>" },
  { subject: "Hello\r\nFrom: ceo@bank.example\r\nX-Evil: 1", from: address("real@sender.example"), text: "body" },
  { subject: 'sale boundary="--"', text: "t", html: "h" },
  { subject: "x Content-Type: text/evil", text: "t", html: "h" },
  { subject: "a\u2028Content-Type: text/evil", text: "t", html: "h", attachments: [ATTACHMENT()] },
  { subject: "\u65e5\u672c\u8a9e \u{1F600}", text: "\u65e5\u672c\u8a9e\u306e\u672c\u6587 \u{1F600}", from: address("\u5c71\u7530 <yamada@example.jp>", [{ name: "\u5c71\u7530", address: "yamada@example.jp" }]) },
  {
    messageId: "<full@example.com>", date: "2026-11-21T05:53:08.123Z", subject: "Full",
    from: address("John Doe <john@example.com>", [{ name: "John Doe", address: "john@example.com" }]),
    to: address("Jane <jane@example.com>, bob@example.com", [{ name: "Jane", address: "jane@example.com" }, { name: "", address: "bob@example.com" }]),
    cc: address("cc@example.com", [{ address: "cc@example.com" }]), bcc: address("bcc@example.com", [{ name: "B", address: "bcc@example.com" }]),
    replyTo: address("reply@example.com", [{ name: "", address: "reply@example.com" }]),
    text: "Plain body\r\nsecond line", html: "<p>HTML body</p>", read: true, saved: true, answered: true, uid: { domain: 12, account: 7 }, modseq: 99,
  },
  { from: address("x"), to: address(""), cc: { value: [{ name: "only value", address: "v@example.com" }] }, bcc: { text: "only text" }, replyTo: address("", []) },
  { date: "not a date" }, { date: "" }, { date: "Sat, 21 Nov 2026 05:53:08 +0100" }, { date: "2026-11-21" }, { date: "1-Jan-70" }, { date: "+275760-09-13T00:00:00.000Z" },
  { date: "-000001-06-15T12:00:00Z" }, { date: "0099-01-01T00:00:00Z" }, { date: "12345" },
  { read: true }, { saved: true }, { deleted: true }, { draft: true }, { answered: true }, { read: true, saved: true, deleted: true, draft: true, answered: true }, { read: false, saved: false },
  { text: "Hello", attachments: [ATTACHMENT()] },
  { html: "<b>x</b>", attachments: [ATTACHMENT({ filename: "document.pdf" })] },
  { text: "Hello", html: "<p>Hello</p>", attachments: [ATTACHMENT({ content: { data: "att-small" } }), ATTACHMENT({ content: { data: "att-3k" }, filename: "1000025344.jpg", contentType: "image/jpeg", size: 81495 })] },
  { attachments: [ATTACHMENT({ content: { data: "att-empty" } })] },
  { attachments: [ATTACHMENT({ content: { data: "att-small" }, filename: "a.txt", contentType: "text/plain", size: 10 }), ATTACHMENT({ content: { data: "att-odd" }, filename: "b.bin", contentType: "application/octet-stream", size: 4 })] },
  { text: "body", attachments: [{ content: { data: "att-small" }, size: 12 }] },
  { text: "body", attachments: [{ content: { data: "att-small" }, filename: 'in"voice\\', contentType: 'ap"p/pd\\f', size: 12 }] },
  { text: "body", attachments: [ATTACHMENT({ contentType: "application", size: 0 }), ATTACHMENT({ contentType: "/pdf", size: 1 }), ATTACHMENT({ contentType: "application/", size: 2 }), ATTACHMENT({ contentType: "TEXT/PLAIN", size: 100 }), ATTACHMENT({ contentType: "text/plain", size: 0 })] },
  { text: "body", attachments: [ATTACHMENT({ filename: 'evil.txt"; filename="wanted.pdf', contentType: "text/plain\r\nX-Evil: 1" })] },
  { text: "body", attachments: [] },
  { text: "x".repeat(70000), html: "<p>" + "\u65e5".repeat(30000) + "</p>" },
  { text: Array.from({ length: 200 }, (_, i) => `line ${i}`).join("\r\n") },
  { messageId: "<only-id@example.com>", text: "a", html: "b" },
  { text: "a", html: "b" },
];
for (const mail of MAILS) {
  // The megabyte-sized bodies are exercised through fetch.json, by reference.
  if (JSON.stringify(mail).length > 20000) continue;
  utilCall("formatEnvelope", mail);
  utilCall("formatFlags", mail);
  for (const docId of [undefined, "doc1", "", '<d@x>\r\n"']) utilCall("formatHeaders", mail, ...(docId === undefined ? [] : [docId]));
  for (const extensible of [true, false]) utilCall("formatBodyStructure", mail, extensible);
  utilCall("formatBodyStructure", mail);
}
const BOXES = ["INBOX", "inbox", "Inbox", "\u0131nbox", "INBOX ", "Sent Messages", "sent messages", "Sent Messages/accounts/foo", "Sent Messages/accounts", "Sent Messages/accounts/",
  "INBOX/accounts", "INBOX/accounts/foo", "INBOX/accounts/a/b", "Archive", "", "Drafts", "drafts", "DRAFTS", "Junk", "junk", "Starred", "STARRED", "Trash", "trash", "Trash ",
  "Dr\u00e4fts", "support", "user@example.com", "first.last@example.com", "user+tag@example.com", "noat", "@x", "a@b@c", "\u00dcSER@x"];
for (const box of BOXES) {
  for (const fn of ["utilityFolder", "isUtilityFolder", "isMappedUtilityFolder", "utilityPlacement", "canonicalMailbox", "accountToBox", "accountToSentBox", "isSentBox",
    "isInbox", "isDomainScoped", "isAccountsFolder", "isSentMessagesAccountsFolder"]) {
    utilCall(fn, box);
  }
  utilCall("boxToAccount", "admin", box);
  utilCall("boxToAccount", "testuser", box);
}
for (const [source, dest] of [["src-msg-id", "INBOX/accounts/foo"], ["src-msg-id-B", "INBOX/accounts/foo"], ["src", "dest"], ["srcdest", ""], ["", ""], ["<\u00e9@x>", "Archiv\u00e9"]]) {
  utilCall("deriveCopyMessageId", source, dest);
}
utilCases.push({ fn: "constants", args: [], expected: JSON.parse(JSON.stringify({
  ACCOUNTS_FOLDER: util.ACCOUNTS_FOLDER, SENT_MESSAGES_FOLDER: util.SENT_MESSAGES_FOLDER, SENT_MESSAGES_ACCOUNTS_FOLDER: util.SENT_MESSAGES_ACCOUNTS_FOLDER,
  UTILITY_FOLDERS: util.UTILITY_FOLDERS,
})) });
counts["util.json"] = write("util.json", utilCases);

// ---------------------------------------------------------------------------
// sequence-resolver.ts
// ---------------------------------------------------------------------------

const MAX = Number.MAX_SAFE_INTEGER;
const sequenceCases: { fn: string; args: unknown[]; expected: unknown }[] = [];
const seqCall = (fn: string, ...args: unknown[]) => sequenceCases.push({ fn, args, expected: capture(() => seq[fn](...args)) });
const UID_LISTS = [[], [1], [10, 20, 30], [1, 2, 3, 4, 5], [5, 15], Array.from({ length: 50 }, (_, i) => 10001 + i * 7), Array.from({ length: 20 }, (_, i) => i + 1),
  [3, 3, 3], [30, 20, 10], [2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22]];
const NUMBERS = [0, 1, 2, 3, 5, 10, 20, 21, 30, 31, 10001, 10344, 10345, MAX, MAX - 1, 1e20, -1];
const SETS = [
  [{ start: 1 }], [{ start: 1, end: 3 }], [{ start: 3, end: 1 }], [{ start: 1, end: MAX }], [{ start: MAX, end: 1 }], [{ start: MAX }], [{ start: MAX, end: MAX }],
  [{ start: 0 }], [{ start: 0, end: 0 }], [{ start: 100, end: 200 }], [{ start: 1 }, { start: 3 }, { start: 5 }], [{ start: 1, end: 2 }, { start: 5, end: 8 }, { start: 20, end: MAX }],
  [{ start: 10001, end: 10002 }, { start: 10005, end: 10100 }], [{ start: 10051, end: MAX }], [{ start: MAX, end: 10051 }], [{ start: 19950, end: 10051 }],
  [{ start: 2, end: 2 }, { start: 2, end: 2 }], [{ start: 1e20 }], [{ start: 1, end: 1e20 }], [{ start: 5, end: 15 }, { start: 10, end: 30 }], [],
];
for (const uids of UID_LISTS) {
  for (const number of NUMBERS) {
    seqCall("seqToUidNumber", uids, number);
    seqCall("uidToSeqNumber", uids, [...uids.entries()].map(([index, uid]) => [uid, index + 1]), number);
  }
  for (const start of NUMBERS) for (const end of [1, 3, MAX, 0, 1e20]) {
    seqCall("resolveSeqRangeToUids", uids, start, end);
    seqCall("resolveUidRangeSentinel", uids, start, end);
  }
  for (const ranges of SETS) {
    const set = { type: "sequence", ranges };
    for (const isUid of [false, true]) {
      seqCall("countSequenceSetMessages", uids, set, isUid);
      for (const limit of [0, 1, 4, 50, -1]) seqCall("clampSequenceSetToFirst", uids, set, limit, isUid);
    }
  }
  for (const live of UID_LISTS.slice(0, 6)) seqCall("departedSequenceNumbers", uids, live);
}
// `uidToSeqNumber` takes a Map: rebuild it from the entries recorded as the argument.
for (const entry of sequenceCases) {
  if (entry.fn === "uidToSeqNumber") {
    const [uids, entries, number] = entry.args as [number[], [number, number][], number];
    entry.expected = capture(() => seq.uidToSeqNumber(uids, new Map(entries), number));
  }
}
counts["sequence.json"] = write("sequence.json", sequenceCases);

// ---------------------------------------------------------------------------
// session-buffer.ts
// ---------------------------------------------------------------------------

const bufferScripts: { ops: unknown[][]; results: unknown[] }[] = [];
{
  const random = rng(99);
  const PIECES = ["a1 NOOP\r\n", "a2 LOGIN {5+}\r\n", "admin", " secret\r\n", "\r", "\n", "\r\n", "x", "", "caf\u00e9", "\u{1F600}", "tail without terminator", "\r\r\n", "\n\r",
    "A".repeat(100), "B".repeat(700) + "\r\n"];
  const RAW = [[0xff], [0xc3], [0xa9], [0xe2, 0x82], [0xac], [0xf0, 0x9f], [0x98, 0x80], [0xef, 0xbb, 0xbf], [0xed, 0xa0, 0x80], [0xc0, 0x80], [0x00], [0x0d], [0x0a], [0x80, 0x80],
    [0xf4, 0x90, 0x80, 0x80], [0xe0, 0x80, 0x80], [0xf8, 0x88, 0x80, 0x80, 0x80], [0x41, 0xc2]];
  for (let script = 0; script < 160; script++) {
    const buffer = new SessionBuffer();
    const ops: unknown[][] = [];
    const results: unknown[] = [];
    const steps = 5 + int(random, 60);
    for (let step = 0; step < steps; step++) {
      const length = buffer.length;
      const choice = int(random, 100);
      if (choice < 40) {
        const bytes = int(random, 4) === 0 ? Buffer.from(pick(random, RAW)) : Buffer.from(pick(random, PIECES), "utf8");
        // A whole piece, or a piece cut where the wire would cut it.
        const cut = int(random, 3) === 0 && bytes.length > 1 ? bytes.subarray(0, 1 + int(random, bytes.length - 1)) : bytes;
        ops.push(["push", cut.toString("hex")]);
        buffer.push(Buffer.from(cut));
        results.push(null);
      } else if (choice < 60) {
        ops.push(["indexOfCrlf"]);
        results.push(buffer.indexOfCrlf());
      } else if (choice < 72) {
        const start = int(random, length + 2);
        const end = start + int(random, length + 3);
        ops.push(["decode", start, end]);
        results.push(buffer.decode(start, end));
      } else if (choice < 82) {
        const max = int(random, 3) === 0 ? 1 + int(random, 10) : 100000;
        ops.push(["head", max]);
        results.push(Buffer.from(buffer.head(max)).toString("hex"));
      } else if (choice < 96) {
        const count = int(random, 8) === 0 ? length + int(random, 5) : int(random, length + 1);
        ops.push(["consume", count]);
        buffer.consume(count);
        results.push(null);
      } else if (choice < 98) {
        ops.push(["clear"]);
        buffer.clear();
        results.push(null);
      } else {
        // The command loop: take lines while there are any.
        ops.push(["drainLines"]);
        const lines: string[] = [];
        for (let at = buffer.indexOfCrlf(); at !== -1; at = buffer.indexOfCrlf()) {
          lines.push(buffer.decode(0, at));
          buffer.consume(at + 2);
        }
        results.push(lines);
      }
      ops.push(["length"]);
      results.push(buffer.length);
    }
    bufferScripts.push({ ops, results });
  }
  // More segments than the buffer holds unmerged: a command dribbled an octet at a time.
  for (const [count, terminator] of [[4096, false], [4097, true], [9000, true], [12289, false]] as const) {
    const buffer = new SessionBuffer();
    const ops: unknown[][] = [];
    const results: unknown[] = [];
    // `pushEach` pushes every octet of its argument as a segment of its own.
    for (let done = 0; done < count; done += 997) {
      const run = Buffer.from(Array.from({ length: Math.min(997, count - done) }, (_, i) => 0x61 + ((done + i) % 26)));
      ops.push(["pushEach", run.toString("hex")]);
      for (const byte of run) buffer.push(Buffer.from([byte]));
      results.push(null);
      ops.push(["indexOfCrlf"]);
      results.push(buffer.indexOfCrlf());
    }
    if (terminator) {
      for (const hex of ["0d", "0a"]) {
        ops.push(["push", hex]);
        buffer.push(Buffer.from(hex, "hex"));
        results.push(null);
      }
    }
    ops.push(["indexOfCrlf"]);
    results.push(buffer.indexOfCrlf());
    ops.push(["length"]);
    results.push(buffer.length);
    ops.push(["head", 3]);
    results.push(Buffer.from(buffer.head(3)).toString("hex"));
    ops.push(["consume", 5]);
    buffer.consume(5);
    results.push(null);
    ops.push(["decode", 0, 30]);
    results.push(buffer.decode(0, 30));
    bufferScripts.push({ ops, results });
  }
}
counts["session_buffer.json"] = write("session_buffer.json", bufferScripts);

// ---------------------------------------------------------------------------
// trace.ts
// ---------------------------------------------------------------------------

const trace = await import(`${imap}/trace.ts`);
const traceCases: { fn: string; args: unknown[]; expected: unknown }[] = [];
const TRACE_LINES = [
  "A1 NOOP", "A2 CAPABILITY", 'A1 LOGIN "admin" "hunter2"', "A1 LOGIN admin hunter2", "a1 login admin hunter2", "LOGIN admin hunter2", "  A1 LOGIN admin hunter2",
  "\r\n LOGIN admin hunter2", "A1 LOGIN", "A1 LOGIN ", "A1 LOGINX admin hunter2", "A1 XLOGIN admin hunter2", "LOGIN LOGIN x", "LOGIN LOGIN LOGIN x", "A1 LOGIN {5+}",
  "admin {8+}", "password", "A1  LOGIN\tadmin\thunter2", "A1 LOGIN admin hunter2\r\nA2 NOOP", "A1 \u0131OGIN a b", "A1 LOG\u0131N a b", "A1 LOGIN\u00a0admin\u00a0hunter2",
  "A2 AUTHENTICATE PLAIN AGFkbWluAGh1bnRlcjI=", "A2 AUTHENTICATE PLAIN", "A2 AUTHENTICATE PLAIN ", "A2 authenticate plain AGFkbWlu", "AUTHENTICATE PLAIN AGFkbWlu",
  "  A1 AUTHENTICATE PLAIN AGFkbWluAGh1bnRlcjI=", "AUTHENTICATE AUTHENTICATE PLAIN", "AUTHENTICATE AUTHENTICATE PLAIN x", "A2 AUTHENTICATE", "A2 AUTHENTICATE  PLAIN  x  y",
  "A2 AUTHENTICATE PLAIN\r\nAGFkbWlu", "A1 LOGIN a b AUTHENTICATE PLAIN c", "A1 AUTHENTICATE LOGIN a b", "A1 SELECT INBOX", "a1 select inbox", "A1 select INBOX", "A1 SELECTx",
  "A1 SELECT_X", "A1 SELECT9", "A1 SELECT-X", "A1\tSELECT", "A1", "A1 ", " A1 NOOP", "A.1 NOOP", "A1 N", "\u00e91 NOOP", "A1 \u00c9", "* OK IMAP4rev1 Service Ready",
  "* CAPABILITY IMAP4rev1 LITERAL+", "* 12 EXISTS", "* 0 RECENT", "* 3 EXPUNGE", "* 3 EXPUNGE ", "* 3 EXPUNGE\t\r", "* 3 EXPUNGED", "* 3 FETCH (FLAGS (\\Seen))", "* x EXISTS",
  "*  12 EXISTS", "* 12  EXISTS", "* BYE Server logging out", "* BAD x", "* NO x", "* ENABLED CONDSTORE", "* PREAUTH", "* OKAY", "* OK", "* NOOP", "* SEARCH 1 2 3", "* LIST () \"/\" INBOX",
  "* FLAGS (\\Seen)", "+ go ahead", "+", "+idling", "A1 OK LOGIN completed", "A2 NO Not authenticated.", "A3 BAD Invalid syntax", "A3 BAD", "A3 OK", "A3  OK x", "A3 ok x", "a.3 OK x",
  "Subject: Attorney client privileged", 'ENVELOPE ("Wed" "hello" ((NIL NIL "alice" "ex.com")))', ")", "", `A1 SELECT ${"X".repeat(600)}`, `A1 SELECT ${"\u00e9".repeat(300)}`,
  `A1 SELECT ${"\u{1F600}".repeat(200)}`, `A1 SELECT ${"x".repeat(501)}\u{1F600}`, "x".repeat(512), "x".repeat(513), "\ufffd".repeat(171), "\ufffd".repeat(170) + "ab", `A1 LOGIN admin ${"p".repeat(700)}`,
];
for (const line of TRACE_LINES) {
  traceCases.push({ fn: "redactCredentials", args: [line], expected: trace.redactCredentials(line) });
  traceCases.push({ fn: "clip", args: [line], expected: trace.clip(line) });
}
{
  const original = logger.info;
  const traced = (direction: string, data: string): string[] => {
    const lines: string[] = [];
    logger.info = (_message: string, fields: { line: string }) => void lines.push(fields.line);
    try {
      trace.imapTrace(direction, "session", data);
    } finally {
      logger.info = original;
    }
    return lines;
  };
  for (const direction of ["in", "out"]) {
    for (const line of TRACE_LINES) traceCases.push({ fn: "trace", args: [direction, line], expected: traced(direction, line) });
    const joined = TRACE_LINES.slice(0, 40).join("\r\n") + "\r\n";
    traceCases.push({ fn: "trace", args: [direction, joined], expected: traced(direction, joined) });
    traceCases.push({ fn: "trace", args: [direction, "A1 NOOP\nA2 NOOP\r\n\r\nA3 NOOP\r"], expected: traced(direction, "A1 NOOP\nA2 NOOP\r\n\r\nA3 NOOP\r") });
  }
  if (traceCases.filter((entry) => entry.fn === "trace").every((entry) => (entry.expected as string[]).length === 0)) {
    throw new Error("The wire trace recorded nothing: IMAP_TRACE was not on when trace.ts loaded");
  }
}
counts["trace.json"] = write("trace.json", traceCases);

// ---------------------------------------------------------------------------
// capabilities.ts
// ---------------------------------------------------------------------------

const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "imap-golden-"));
const capabilityCases: unknown[] = [];
{
  const cert = path.join(scratch, "cert.pem");
  const key = path.join(scratch, "key.pem");
  fs.writeFileSync(cert, "cert");
  fs.writeFileSync(key, "key");
  for (const ssoOn of [false, true]) {
    if (ssoOn) {
      sso.initializeSso({ AUTH_ISSUER: "https://auth.example.test", AUTH_CLIENT_ID: "mail", AUTH_CLIENT_SECRET: "test_key_not_real_do_not_use", APP_HOSTNAME: "mail.example.test" });
    }
    for (const tls of [false, true]) {
      if (tls) {
        process.env.SSL_CERTIFICATE = cert;
        process.env.SSL_CERTIFICATE_KEY = key;
      } else {
        delete process.env.SSL_CERTIFICATE;
        delete process.env.SSL_CERTIFICATE_KEY;
      }
      for (const isTls of [false, true]) {
        capabilityCases.push({ isTls, sso: ssoOn, tlsAvailable: tls, expected: getCapabilities(isTls) });
      }
    }
    sso.resetSso();
  }
  delete process.env.SSL_CERTIFICATE;
  delete process.env.SSL_CERTIFICATE_KEY;
}
counts["capabilities.json"] = write("capabilities.json", capabilityCases);

// ---------------------------------------------------------------------------
// fetch-helpers.ts and session-utils.ts
// ---------------------------------------------------------------------------

// Attachment files live under `./attachments` of the working directory.
process.chdir(scratch);
fs.mkdirSync("attachments");
const ATTACHMENT_FILES: Record<string, Buffer> = {
  "att-small": Buffer.from("0123456789"),
  "att-empty": Buffer.alloc(0),
  "att-odd": Buffer.from([0xff, 0x00, 0x80, 0x7f, 0x0a]),
  "att-3k": Buffer.from(Array.from({ length: 3001 }, (_, i) => (i * 31 + 7) % 256)),
};
for (const [id, bytes] of Object.entries(ATTACHMENT_FILES)) fs.writeFileSync(path.join("attachments", id), bytes as unknown as Uint8Array);

const SECTION_VALUES: unknown[] = [
  { type: "FULL" }, { type: "HEADER" }, { type: "TEXT" }, { type: "MIME_PART", partNumber: "1" }, { type: "MIME_PART", partNumber: "2" }, { type: "MIME_PART", partNumber: "3" },
  { type: "MIME_PART", partNumber: "1.1" }, { type: "MIME_PART", partNumber: "1.2" }, { type: "MIME_PART", partNumber: "1.3" }, { type: "MIME_PART", partNumber: "99" },
  { type: "MIME_PART", partNumber: "1", subSection: "HEADER" }, { type: "MIME_PART", partNumber: "1", subSection: "MIME" }, { type: "MIME_PART", partNumber: "1", subSection: "TEXT" },
  { type: "MIME_PART", partNumber: "2", subSection: "MIME" }, { type: "MIME_PART", partNumber: "1.2", subSection: "TEXT" }, { type: "MIME_PART", partNumber: "1.2", subSection: "MIME" },
  { type: "MIME_PART", partNumber: "3", subSection: "HEADER" }, { type: "MIME_PART", partNumber: "0" }, { type: "MIME_PART", partNumber: "1.0" },
  { type: "HEADER_FIELDS", not: false, fields: ["FROM", "TO", "SUBJECT", "DATE"] }, { type: "HEADER_FIELDS", not: true, fields: ["FROM", "TO"] },
  { type: "HEADER_FIELDS", not: false, fields: ["from"] }, { type: "HEADER_FIELDS", not: false, fields: [] }, { type: "HEADER_FIELDS", not: true, fields: [] },
  { type: "HEADER_FIELDS", not: false, fields: ["X-NONE"] }, { type: "HEADER_FIELDS", not: false, fields: ["MESSAGE-ID", "CONTENT-TYPE", "MIME-VERSION"] },
  { type: "HEADER_FIELDS", not: true, fields: ["CONTENT-TRANSFER-ENCODING", "REPLY-TO", "CC", "BCC"] },
];
const PARTIALS = [undefined, { start: 0, length: 10 }, { start: 0, length: 2048 }, { start: 5, length: 5 }, { start: 3, length: 0 }, { start: 100, length: 50 }, { start: 0, length: 1000000 },
  { start: 1000000, length: 10 }, { start: 1, length: 2 }, { start: 2, length: 3 }, { start: 7, length: 64 }, { start: 150, length: 131 }];
const DATA_ITEMS: unknown[] = [
  { type: "ENVELOPE" }, { type: "FLAGS" }, { type: "BODYSTRUCTURE", extensible: true }, { type: "BODYSTRUCTURE", extensible: false }, { type: "UID" }, { type: "INTERNALDATE" },
  { type: "RFC822" }, { type: "RFC822.HEADER" }, { type: "RFC822.SIZE" }, { type: "RFC822.TEXT" }, { type: "MODSEQ" },
  ...SECTION_VALUES.flatMap((section) => [{ type: "BODY", peek: false, section }, { type: "BODY", peek: true, section, partial: { start: 0, length: 20 } }]),
];

const fetchCases: Record<string, unknown>[] = [];
for (const items of [[], ...DATA_ITEMS.map((item) => [item]), DATA_ITEMS, [{ type: "FLAGS" }, { type: "UID" }], [{ type: "ENVELOPE" }, { type: "BODY", peek: true, section: { type: "HEADER_FIELDS", not: false, fields: ["FROM", "X-A", "reply-to"] } }]]) {
  fetchCases.push({ fn: "getRequestedFields", items, expected: [...fetchHelpers.getRequestedFields(items)] });
  fetchCases.push({ fn: "shouldMarkAsRead", items, expected: sessionUtils.shouldMarkAsRead(items) });
}
for (const section of SECTION_VALUES) {
  fetchCases.push({ fn: "getBodySectionKey", section, expected: sessionUtils.getBodySectionKey(section) });
  const fields = new Set<string>();
  fetchHelpers.addBodyFields({ type: "BODY", peek: true, section }, fields);
  fetchCases.push({ fn: "addBodyFields", section, expected: [...fields] });
}
for (const ranges of SETS) {
  fetchCases.push({ fn: "convertSequenceSet", ranges, expected: capture(() => fetchHelpers.convertSequenceSet({ type: "sequence", ranges })) });
}
for (const content of ["", "Hello, World!", "Hello \u{1F600} World", "\u65e5\u672c\u8a9e", "a\r\nb\r\n\r\n", "\u00e9".repeat(10)]) {
  for (const partial of PARTIALS.slice(1)) {
    fetchCases.push({ fn: "applyPartialFetch", content, partial, expected: sessionUtils.applyPartialFetch(content, partial) });
  }
}

/** The octets a stream emits, as their count and digest. */
const drain = async (stream: AsyncIterable<Buffer>): Promise<{ emitted: number; sha256: string }> => {
  const hash = crypto.createHash("sha256");
  let emitted = 0;
  for await (const chunk of stream) {
    hash.update(chunk as unknown as Uint8Array);
    emitted += chunk.length;
  }
  return { emitted, sha256: hash.digest("hex") };
};
/** A response part as JSON; a stream is drained, unless it would read the database. */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
const part = async (value: any, lazy: boolean): Promise<unknown> => {
  if (value === null || value === undefined) return null;
  if (value.type === "stream") {
    return { type: "stream", header: value.header, length: value.length, ...(lazy ? {} : await drain(value.stream)) };
  }
  return JSON.parse(JSON.stringify(value));
};
const attempt = async (run: () => Promise<unknown>): Promise<unknown> => {
  try {
    return await run();
  } catch (error) {
    return { threw: String(error) };
  }
};

for (const [index, mail] of MAILS.entries()) {
  const lazy = typeof mail.text_octets === "number" && typeof mail.html_octets === "number" && typeof mail.mail_id === "string" && typeof mail.user_id === "string"
    && mail.text === undefined && mail.html === undefined;
  for (const docId of index % 4 === 0 ? ["doc1", undefined] : ["doc1"]) {
    fetchCases.push({ fn: "buildMessageSegments", mail: index, docId: docId ?? null, expected: capture(() => {
      const segments = sessionUtils.buildMessageSegments(mail, docId);
      return {
        segments,
        fullSize: sessionUtils.sumSegmentBytes(segments),
        computed: sessionUtils.computeFullMessageSize(mail, docId),
        bodyBytes: sessionUtils.sumBodyBytes(segments),
        partBytes: Object.fromEntries(["1", "2", "3", "1.1", "1.2"].map((p) => [p, sessionUtils.sumPartBodyBytes(segments, p)])),
      };
    }) });
    fetchCases.push({ fn: "buildFullMessage", mail: index, docId: docId ?? null, expected: capture(() => sessionUtils.buildFullMessage(mail, docId)) });
  }
  for (const partNum of ["1", "2", "3", "4", "1.1", "1.2", "1.3", "0", "x", "", "2.1", "1.", "01"]) {
    fetchCases.push({ fn: "getBodyPartHeaders", mail: index, partNum, expected: capture(() => sessionUtils.getBodyPartHeaders(mail, partNum)) });
    if (!lazy) fetchCases.push({ fn: "getBodyPart", mail: index, partNum, expected: capture(() => sessionUtils.getBodyPart(mail, partNum)) });
  }
  for (const section of SECTION_VALUES) {
    fetchCases.push({ fn: "getBodyContent", mail: index, section, docId: "doc1", expected: capture(() => fetchHelpers.getBodyContent(mail, section, "doc1")) });
    for (const partial of index % 3 === 0 ? PARTIALS : PARTIALS.slice(0, 3)) {
      const bodyFetch = { type: "BODY", peek: true, section, ...(partial ? { partial } : {}) };
      fetchCases.push({
        fn: "buildBodyResponsePart", mail: index, bodyFetch, docId: "doc1",
        expected: await attempt(async () => part(await fetchHelpers.buildBodyResponsePart(mail, bodyFetch, "doc1", "INBOX"), lazy)),
      });
    }
  }
  for (const mailbox of ["INBOX", "INBOX/accounts/foo"]) {
    for (const item of DATA_ITEMS.slice(0, 11)) {
      // INTERNALDATE falls back to the clock when the mail has no date.
      if ((item as { type: string }).type === "INTERNALDATE" && !mail.date) continue;
      if ((item as { type: string }).type === "UID" && !mail.uid) continue;
      fetchCases.push({
        fn: "buildFetchResponsePart", mail: index, item, docId: "doc1", mailbox,
        expected: await attempt(async () => part(await fetchHelpers.buildFetchResponsePart(mail, item, "doc1", mailbox), lazy)),
      });
    }
  }
  const responseItems = [{ type: "FLAGS" }, { type: "UID" }, { type: "MODSEQ" }, { type: "RFC822.SIZE" }, { type: "ENVELOPE" }, { type: "BODYSTRUCTURE", extensible: true },
    { type: "BODY", peek: true, section: { type: "HEADER_FIELDS", not: false, fields: ["FROM", "SUBJECT"] } }, { type: "BODY", peek: false, section: { type: "MIME_PART", partNumber: "2" } },
    { type: "BODY", peek: true, section: { type: "TEXT" }, partial: { start: 0, length: 64 } }];
  if (mail.uid) {
    for (const [isUidFetch, condstore] of [[true, false], [false, true], [true, true], [false, false]] as const) {
      fetchCases.push({
        fn: "buildFetchResponse", mail: index, items: responseItems, docId: "doc1", uid: 4242, isUidFetch, mailbox: "INBOX", condstore,
        expected: await attempt(async () => Promise.all((await fetchHelpers.buildFetchResponse(mail, responseItems, "doc1", 4242, isUidFetch, "INBOX", condstore)).map((p: unknown) => part(p, lazy)))),
      });
    }
  }
}
counts["fetch.json"] = write("fetch.json", {
  attachments: Object.fromEntries(Object.entries(ATTACHMENT_FILES).map(([id, bytes]) => [id, bytes.toString("base64")])),
  mails: MAILS,
  cases: fetchCases.map((entry) => ({ ...entry, expected: shrink(entry.expected) })),
});

fs.rmSync(scratch, { recursive: true, force: true });
for (const [name, count] of Object.entries(counts)) console.log(`${name}: ${count} cases`);
console.log(`total: ${Object.values(counts).reduce((sum, count) => sum + count, 0)} cases`);
process.exit(0);
