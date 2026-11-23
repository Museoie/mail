/**
 * Golden fixtures for the Rust port of the IMAP data layer
 * (`repositories/mails/imap-query.ts`, `repositories/mails/imap.ts` and
 * `imap/store.ts`), generated from the real TypeScript:
 *
 *   bun src/server-rs/tests/golden/imapdb/generate.ts
 *
 * - `builders.json`: what the pure functions return: every statement builder
 *   with the values it binds, the criterion reductions, the flag clauses and
 *   the search keys the parser produces, simplified.
 * - `traces.json`: for each repository function and store method, the
 *   statements it issues in order, the values it binds and what it returns.
 *   The database driver is replaced by a script that answers each statement
 *   from a list, so no database is needed and every branch (an empty
 *   mailbox, a row that fails validation, a dropped connection) can be
 *   reached on demand.
 *
 * Bound values are recorded as the pool sends them: every one as text, the
 * way the real `Pool` converts it. The Rust tests in
 * `src/postgres/repositories/mails/imap_query.rs`, `imap.rs` and
 * `src/imap/store.rs` replay both files. Re-run this script whenever the
 * TypeScript changes; the output is deterministic.
 */
import fs from "fs";
import path from "path";

// Environment the TypeScript reads at import time, set before any import.
process.env.TZ = "UTC";
process.env.NODE_ENV = "test";
delete process.env.EMAIL_DOMAIN;

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
type Row = { [key: string]: Json };
type Response = { rows: Row[] } | { error: { code: string; message: string } };
interface Statement {
  sql: string;
  values: Json[];
}

// The `server` barrel first: it fixes the order the module graph loads in.
await import("../../../../server/index");
const { pool } = await import("../../../../server/lib/postgres/client");
const { Mail, SignedUser } = await import("../../../../common");
const query = await import("../../../../server/lib/postgres/repositories/mails/imap-query");
const httpQuery = await import("../../../../server/lib/postgres/repositories/mails/http-query");
const repository = await import("../../../../server/lib/postgres/repositories/mails/imap");
const { Store, simplifyCriterion } = await import("../../../../server/lib/imap/store");
const { parseCommand } = await import("../../../../server/lib/imap/parsers/command-parser");
const { getRequestedFields } = await import("../../../../server/lib/imap/fetch-helpers");

if (new Date(2026, 0, 1).getTimezoneOffset() !== 0) {
  throw new Error("The fixtures must be generated in UTC");
}

// The code under test logs what it absorbs; the fixtures record it.
console.error = () => undefined;
console.warn = () => undefined;
console.log = () => undefined;
console.info = () => undefined;
console.debug = () => undefined;

// ---------------------------------------------------------------------------
// Values as the fixtures spell them
// ---------------------------------------------------------------------------

const USER = "11111111-1111-4111-8111-111111111111";
const MAIL = "22222222-2222-4222-8222-222222222222";
const OTHER_MAIL = "33333333-3333-4333-8333-333333333333";
const THIRD_MAIL = "44444444-4444-4444-8444-444444444444";
const FIXED_IDS = [USER, MAIL, OTHER_MAIL, THIRD_MAIL];
const DATE = "2024-01-02T03:04:05.000Z";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/**
 * A value as JSON: a number JSON cannot carry as `{ $n }`, a date as
 * `{ $date: <ms or null> }`, bytes as `{ $hex }`, a `Map` as its entries.
 */
const enc = (value: unknown): Json => {
  if (value === undefined || value === null) return null;
  if (value instanceof Date) return { $date: Number.isNaN(value.getTime()) ? null : value.getTime() };
  if (Buffer.isBuffer(value)) return { $hex: value.toString("hex") };
  if (typeof value === "number") return Number.isFinite(value) ? value : { $n: String(value) };
  if (value instanceof Map) return [...value.entries()].map(([key, entry]) => [enc(key), enc(entry)]);
  if (value instanceof Set) return [...value].map(enc);
  if (Array.isArray(value)) return value.map(enc);
  if (typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value as Record<string, unknown>)
        .filter(([, entry]) => entry !== undefined)
        .map(([key, entry]) => [key, enc(entry)])
    );
  }
  return value as Json;
};

/** The reverse of `enc`, for arguments and scripted rows. */
const dec = (value: Json): unknown => {
  if (value === null || typeof value !== "object") return value;
  if (Array.isArray(value)) return value.map(dec);
  if ("$date" in value) return new Date(value.$date === null ? NaN : (value.$date as number));
  if ("$n" in value) return Number(value.$n);
  if ("$hex" in value) return Buffer.from(value.$hex as string, "hex");
  return Object.fromEntries(Object.entries(value).map(([key, entry]) => [key, dec(entry)]));
};

const number = (value: Json): number => dec(value) as number;
const optionalNumber = (value: Json | undefined): number | undefined =>
  value === undefined || value === null ? undefined : number(value);

// ---------------------------------------------------------------------------
// The scripted driver
// ---------------------------------------------------------------------------

let responses: Response[] = [];
let trace: Statement[] = [];
let starved = false;

// The pool keeps its conversions: parameters reach the driver as the text
// the real one would be sent, and scripted rows go back through the pool's
// own result shaping.
const driver = {
  unsafe(text: string, params: unknown[] = []) {
    trace.push({
      sql: text,
      values: params.map((param) => (param === null ? null : Buffer.isBuffer(param) ? enc(param) : String(param))),
    });
    const response = responses.shift();
    const settled = (async () => {
      if (!response) {
        starved = true;
        throw new Error(`no scripted response for statement ${trace.length}: ${text}`);
      }
      if ("error" in response) {
        throw Object.assign(new Error(response.error.message), { errno: response.error.code });
      }
      const rows = response.rows.map((row) => dec(row) as Record<string, unknown>);
      const columns = Object.keys(rows[0] ?? {}).map((name) => ({ name, type: 0 }));
      return Object.assign(rows, { command: "SELECT", count: rows.length, columns });
    })();
    return Object.assign(settled, { cancel: () => undefined });
  },
};
(pool as unknown as { sql: unknown }).sql = driver;

const rows = (...list: Row[]): Response => ({ rows: list });
const fault = (code: string): Response => ({ error: { code, message: `scripted fault ${code}` } });

/** Generated ids differ per run; everything else is exact. */
const normalize = (value: Json): Json =>
  typeof value === "string" && UUID.test(value) && !FIXED_IDS.includes(value) ? "<uuid>" : value;

/** The values as the pool binds them, or the fact that it could not. */
const bind = async (values: unknown[]): Promise<{ values: Json[] } | { threw: true }> => {
  responses = [rows()];
  trace = [];
  try {
    await pool.query("SELECT 1", values);
  } catch {
    return { threw: true };
  } finally {
    responses = [];
  }
  return { values: trace[0].values };
};

// ---------------------------------------------------------------------------
// builders.json
// ---------------------------------------------------------------------------

const builders: Record<string, Json[]> = {};
const record = (group: string, entry: Json) => {
  (builders[group] ??= []).push(entry);
};

const MAILBOXES: (string | null)[] = [
  null,
  "Drafts",
  "Junk",
  "junk",
  "INBOX/accounts/a",
  "Sent Messages/accounts/a",
  "Archive",
  "Starred",
  "Trash",
  "Projects/Wörk \"2024\"",
];
const SENT = [false, true];

for (const mailbox of MAILBOXES) {
  for (const sent of SENT) {
    record("countMessages", {
      args: { mailbox, sent },
      ...enc(query.buildCountMessagesQuery(USER, mailbox, sent)) as { [key: string]: Json },
    });
    for (const [group, build] of [
      ["allUids", query.buildAllUidsQuery],
      ["firstUnseenUid", query.buildFirstUnseenUidQuery],
    ] as const) {
      const built = build(USER, mailbox, sent);
      record(group, { args: { mailbox, sent }, sql: built.sql, ...(await bind(built.values)) });
    }
    record("expungeDeletedFilters", {
      args: { mailbox, sent },
      filters: Object.entries(query.buildExpungeDeletedFilters(USER, mailbox, sent)).map(([column, value]) => [
        column,
        String(value),
      ]),
    });
    const deleted = query.buildExpungeDeletedSelectQuery(USER, mailbox, sent);
    record("expungeDeletedSelect", { args: { mailbox, sent }, sql: deleted.sql, ...(await bind(deleted.values)) });
    const inbox = query.buildExpungeDeletedInboxQuery(USER, mailbox, sent);
    record("expungeDeletedInbox", { args: { mailbox, sent }, sql: inbox.sql, ...(await bind(inbox.values)) });
    for (const uids of [[], [4], [4, 9, 11], [2147483648, 1]]) {
      record("expungeUidsFilters", {
        args: { mailbox, sent, uids },
        filters: Object.entries(query.buildExpungeUidsFilters(USER, mailbox, sent, uids)).map(([column, value]) => [
          column,
          typeof value === "object" && value !== null
            ? { op: (value as { op: string }).op, value: (value as { value: number[] }).value.map(String) }
            : String(value),
        ]),
      });
      const selected = query.buildExpungeUidsSelectQuery(USER, mailbox, sent, uids);
      record("expungeUidsSelect", { args: { mailbox, sent, uids }, sql: selected.sql, ...(await bind(selected.values)) });
    }
    for (const mail_ids of [[MAIL], [MAIL, OTHER_MAIL], ['a"b', "c\\d", "e,f", "{g}", "h i", "", "ünï"]]) {
      const built = query.buildMailFlagUpdatesQuery(USER, mailbox, sent, mail_ids);
      record("mailFlagUpdates", { args: { mailbox, sent, mail_ids }, sql: built.sql, ...(await bind(built.values)) });
    }
  }
}
// The count builder binds booleans; record them as the pool sends them.
for (const entry of builders.countMessages) {
  const built = entry as { values: unknown[] };
  Object.assign(built, await bind(built.values));
}

// --- range reads ---------------------------------------------------------------------

const MAX = Number.MAX_SAFE_INTEGER;

/** The projections FETCH asks for, from the items a client names. */
const FETCH_ITEMS = [
  "FLAGS",
  "(UID FLAGS)",
  "(FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)",
  "(UID RFC822.SIZE FLAGS BODY.PEEK[HEADER.FIELDS (From To Cc Bcc Subject Date Message-ID Priority X-Priority References Newsgroups In-Reply-To Content-Type Reply-To)])",
  "BODY[]",
  "BODY.PEEK[]<0.2048>",
  "(BODY[TEXT] BODYSTRUCTURE)",
  "BODY[1]",
  "BODY[HEADER]",
  "BODY[HEADER.FIELDS.NOT (Received)]",
  "(RFC822 RFC822.HEADER RFC822.TEXT)",
  "(UID MODSEQ)",
  "ALL",
  "FULL",
  "FAST",
];
const fetchProjections: string[][] = [];
for (const items of FETCH_ITEMS) {
  const parsed = parseCommand(`a FETCH 1 ${items}`);
  if (!parsed.success || parsed.value?.request.type !== "FETCH") throw new Error(`FETCH 1 ${items} does not parse`);
  const fields = Array.from(getRequestedFields(parsed.value.request.data.dataItems)) as string[];
  fetchProjections.push(fields, [...fields, "modseq"]);
}

/** The columns `Store.getMessages` reads for a field of the mail. */
const COLUMNS_OF: Record<string, string[]> = {
  messageId: ["message_id"],
  uid: ["uid_domain", "uid_mailbox"],
  from: ["from_address", "from_text"],
  to: ["to_address", "to_text"],
  cc: ["cc_address", "cc_text"],
  bcc: ["bcc_address", "bcc_text"],
  replyTo: ["reply_to_address", "reply_to_text"],
};
const columnsOf = (fields: string[]) => fields.flatMap((field) => COLUMNS_OF[field] ?? [field]);

const RANGE_FIELDS: string[][] = [
  ["*"],
  [],
  ["mail_id"],
  ["subject"],
  ["mail_id", "uid_mailbox", "text_octets", "html_octets"],
  ["uid_mailbox"],
  ["html_octets"],
  ["uid_domain", "uid_mailbox", "read"],
  ["bogus_field", "read"],
  ["*", "subject"],
  ["read", "read", "mail_id"],
  ["text", "html", "search_vector", "updated"],
  ...fetchProjections.map(columnsOf),
];
const RANGES: [number, number][] = [
  [3, 9],
  [1, MAX],
  [7, 7],
  [-1, -1],
  [5, 1e21],
  [1e21, Infinity],
  [NaN, NaN],
  [1000000000, 2000000000],
];

const rangeCase = async (
  mailbox: string | null,
  sent: boolean,
  [start, end]: [number, number],
  useUid: boolean,
  fields: string[],
  changedSince?: number
) => {
  const built = query.buildMailsByRangeQuery(USER, mailbox, sent, start, end, useUid, fields, changedSince);
  record("mailsByRange", {
    args: enc({ mailbox, sent, start, end, useUid, fields, changedSince }),
    sql: built.sql,
    selectedFields: built.selectedFields,
    ...(await bind(built.values)),
  });
};
for (const mailbox of MAILBOXES) {
  for (const useUid of [true, false]) {
    for (const fields of RANGE_FIELDS) {
      await rangeCase(mailbox, false, RANGES[0], useUid, fields);
    }
    for (const range of RANGES) {
      for (const changedSince of [undefined, 0, 42, 1e21]) {
        await rangeCase(mailbox, mailbox !== null && mailbox.startsWith("Sent"), range, useUid, ["mail_id", "modseq"], changedSince);
      }
    }
    await rangeCase(mailbox, true, [1, 5], useUid, ["*"], 7);
  }
}

// --- SEARCH --------------------------------------------------------------------------

type Simplified = { type: string; value?: unknown };

const DAY = new Date("2024-03-04T00:00:00.000Z");
const INVALID = new Date(NaN);
const LAST_DAY = new Date(8.64e15);
const FAR = new Date("9999-12-31T12:00:00.000Z");

const flagTypes = [
  "ALL", "UNSEEN", "SEEN", "FLAGGED", "UNFLAGGED", "ANSWERED", "UNANSWERED", "DELETED", "UNDELETED", "DRAFT",
  "UNDRAFT", "NEW", "OLD", "RECENT", "KEYWORD", "UNKEYWORD", "seen", "Unseen", "SOMETHING-UNSUPPORTED", "SEQ",
  "LARGER", "",
];
const textTypes = ["SUBJECT", "FROM", "TO", "CC", "BCC", "BODY", "TEXT", "SUBJECT_TEXT"];
const dateTypes = ["BEFORE", "ON", "SINCE", "SENTBEFORE", "SENTON", "SENTSINCE"];

const leaves: Simplified[] = [
  ...flagTypes.map((type) => ({ type })),
  ...textTypes.flatMap((type) =>
    ["needle", "", "100% _real_", "it's \"quoted\" \\", "ünï 😀"].map((value) => ({ type, value }))
  ),
  ...["Subject", "FROM", "to", "Message-ID", "message-id", "X-Mailer", "Cc", "", "ſubject"].map((field) => ({
    type: "HEADER",
    value: { field, text: "v%" },
  })),
  ...dateTypes.flatMap((type) => [DAY, INVALID, LAST_DAY, FAR, new Date(0), new Date(-1)].map((value) => ({ type, value }))),
  { type: "LARGER", value: 999999999 },
  { type: "SMALLER", value: 1 },
  { type: "UID_SET", value: [] },
  { type: "UID_SET", value: [{ start: 5 }] },
  { type: "UID_SET", value: [{ start: 1 }, { start: 3 }] },
  { type: "UID_SET", value: [{ start: 2, end: 3 }, { start: 5, end: 7 }] },
  { type: "UID_SET", value: [{ start: 1, end: 3 }] },
  { type: "UID_SET", value: [{ start: 1, end: MAX }, { start: -1, end: -1 }, { start: 1e21 }, { start: Infinity, end: NaN }] },
  { type: "uid_set", value: [{ start: 9 }] },
];

/** The operands the reductions tell apart: all, none, a column test, one and two parameters, and a date that cannot be bound. */
const operands: Simplified[] = [
  { type: "ALL" },
  { type: "KEYWORD" },
  { type: "SEEN" },
  { type: "SUBJECT", value: "x" },
  { type: "ON", value: DAY },
  { type: "UID_SET", value: [{ start: 2, end: 4 }, { start: 9 }] },
  { type: "BEFORE", value: INVALID },
];
const not = (value: Simplified): Simplified => ({ type: "NOT", value });
const or = (left: Simplified, right: Simplified): Simplified => ({ type: "OR", value: { left, right } });
const combined: Simplified[] = [];
for (const a of operands) {
  combined.push(not(a), not(not(a)), not(not(not(a))));
  for (const b of operands) {
    combined.push(or(a, b), not(or(a, b)), or(not(a), b), or(a, not(b)));
    for (const c of [operands[0], operands[1], operands[3], operands[6]]) {
      combined.push(or(or(a, b), c), or(a, or(b, c)), or(not(or(a, b)), not(c)));
    }
  }
}
combined.push({ type: "not", value: { type: "seen" } });

for (const criterion of [...leaves, ...combined]) {
  for (const [uidField, seed] of [
    ["uid_domain", []],
    ["x.uid", [USER, false, "Archive"]],
  ] as [string, unknown[]][]) {
    const values = [...seed];
    const clause = query.buildCriterionClause(criterion, uidField, values as never);
    record("criterionClause", { criterion: enc(criterion), uidField, seed: enc(seed), clause, ...(await bind(values)) });
  }
}

const searches: Simplified[][] = [
  [],
  [{ type: "SEEN" }],
  [{ type: "ALL" }],
  [{ type: "SEEN" }, { type: "KEYWORD" }],
  [{ type: "BODY", value: "needle" }],
  [{ type: "UNSEEN" }, { type: "FROM", value: "a" }, { type: "SINCE", value: DAY }],
  [or({ type: "SUBJECT", value: "x" }, { type: "ALL" }), { type: "TO", value: "kept" }],
  [not({ type: "KEYWORD" }), { type: "UID_SET", value: [{ start: 1, end: 3 }, { start: 7 }] }],
  [{ type: "ON", value: DAY }, not(or({ type: "FROM", value: "a" }, { type: "CC", value: "b" }))],
  [{ type: "BEFORE", value: INVALID }],
  [or({ type: "BEFORE", value: INVALID }, { type: "ALL" }), { type: "SEEN" }],
  [{ type: "ON", value: LAST_DAY }],
  [{ type: "UID_SET", value: [{ start: -1, end: -1 }] }],
];
for (const mailbox of MAILBOXES) {
  for (const criteria of searches) {
    const sent = mailbox !== null && mailbox.startsWith("Sent");
    const built = query.buildSearchMailsByUidQuery(USER, mailbox, sent, criteria);
    record("searchMailsByUid", { args: { mailbox, sent, criteria: enc(criteria) }, sql: built.sql, ...(await bind(built.values)) });
  }
}

// --- STORE ---------------------------------------------------------------------------

const FLAGS = ["\\Seen", "\\Flagged", "\\Deleted", "\\Draft", "\\Answered"];
const flagLists: string[][] = [];
for (let mask = 0; mask < 32; mask++) flagLists.push(FLAGS.filter((_, bit) => mask & (1 << bit)));
flagLists.push(["\\Answered", "\\Seen"], ["\\seen"], ["\\SEEN", "Seen", "$Forwarded"], ["\\Seen", "\\Seen"], ["NonJunk", "\\Deleted"]);
for (const operation of ["FLAGS", "+FLAGS", "-FLAGS"] as const) {
  for (const flags of flagLists) {
    record("flagSetClause", { operation, flags, clause: repository.buildFlagSetClause(operation, flags) });
  }
}

// --- the search keys the parser produces ---------------------------------------------

const SEARCH_KEYS = [
  "ALL", "ANSWERED", "DELETED", "FLAGGED", "NEW", "OLD", "RECENT", "SEEN", "UNANSWERED", "UNDELETED", "UNFLAGGED",
  "UNSEEN", "DRAFT", "UNDRAFT", "KEYWORD $Forwarded", "UNKEYWORD $Forwarded",
  "BEFORE 1-Jan-2024", "ON 4-Mar-2024", "SINCE 1-Feb-2024", "SENTBEFORE 31-Dec-2023", "SENTON 4-Mar-2024",
  "SENTSINCE 29-Feb-2024", "BEFORE 31-Feb-2024", "SINCE 1-Jan-275760", "ON 13-Sep-275760",
  "FROM alice", 'TO "bob smith"', "CC carol", "BCC dave", 'SUBJECT "100% _real_"', "BODY needle",
  'TEXT "quoted \\"text\\""', "SUBJECT ünï",
  "HEADER Subject hi", "HEADER FROM a", "HEADER to b", 'HEADER Message-ID "<id@x>"', "HEADER X-Mailer z",
  "UID 1:5", "UID 1,3:5,9", "UID 5:*", "UID *", "UID 9:2", "UID 99999999999999999999999",
  "1:3", "2,4", "*", "5:*",
  "LARGER 100", "SMALLER 5",
  "NOT SEEN", "NOT FROM x", "NOT UID 3", "NOT 2:3", "NOT ALL", "NOT KEYWORD k", "NOT NOT SEEN",
  "NOT OR SEEN FLAGGED", "NOT BEFORE 1-Jan-2024", "NOT HEADER X-Spam yes",
  "OR SEEN FLAGGED", "OR FROM a TO b", "OR ALL SEEN", "OR KEYWORD k SEEN", "OR KEYWORD a KEYWORD b",
  "OR SUBJECT x ALL", "OR UID 1:2 UID 5", "OR NOT SEEN NOT FLAGGED", "OR OR SEEN FLAGGED DELETED",
  "OR SEEN OR FLAGGED DELETED", "OR 1:2 SUBJECT s", "OR LARGER 5 SMALLER 9",
  "SEEN FLAGGED", "UNSEEN FROM a SINCE 1-Jan-2024", "NOT DELETED SUBJECT x OR FROM a FROM b UID 1:100",
  "MODSEQ 5", "FOO", "CHARSET UTF-8 SUBJECT x",
];
let unparsed = 0;
for (const keys of SEARCH_KEYS) {
  for (const line of [`a SEARCH ${keys}`, `a UID SEARCH ${keys}`]) {
    const parsed = parseCommand(line);
    if (!parsed.success || !parsed.value) {
      unparsed++;
      continue;
    }
    const request = parsed.value.request;
    const search = request.type === "UID" ? request.data.request : request;
    if (search.type !== "SEARCH") throw new Error(`${line} is not a SEARCH`);
    record("simplifyCriterion", { line, simplified: enc(search.data.criteria.map(simplifyCriterion)) });
  }
}

// --- the account list ----------------------------------------------------------------

for (const sent of SENT) {
  for (const domain of ["alice.mydomain", "mydomain", "", undefined]) {
    const built = httpQuery.buildAccountStatsQuery(USER, sent, domain ? [domain] : undefined, false);
    record("accountStats", { args: enc({ sent, domain }), sql: built.sql, ...(await bind(built.values)) });
  }
}

// ---------------------------------------------------------------------------
// traces.json
// ---------------------------------------------------------------------------

interface Trace {
  fn: string;
  args: Json;
  script: Response[];
  statements: Statement[];
  returned?: Json;
  threw?: Json;
}
const traces: Record<string, Trace[]> = { repository: [], store: [] };

const traced = async (group: "repository" | "store", fn: string, args: Json, script: Response[], run: () => unknown) => {
  responses = structuredClone(script);
  trace = [];
  starved = false;
  let outcome: { returned: Json } | { threw: Json };
  try {
    outcome = { returned: enc(await run()) };
  } catch (error) {
    outcome = { threw: { code: (error as { code?: string }).code ?? null } };
  }
  if (starved) throw new Error(`${fn} ${JSON.stringify(args)}: the script ran out`);
  if (responses.length > 0) throw new Error(`${fn} ${JSON.stringify(args)}: ${responses.length} scripted responses were not used`);
  const statements = trace.map(({ sql, values }) => ({ sql, values: values.map(normalize) }));
  traces[group].push({ fn, args, script, statements, ...outcome });
};

const drain = async <T>(chunks: AsyncIterable<T>): Promise<T[]> => {
  const out: T[] = [];
  for await (const chunk of chunks) out.push(chunk);
  return out;
};

/** A stream's chunks, and the failure that ended it if one did. */
const drained = async <T>(chunks: AsyncIterable<T>) => {
  const out: T[] = [];
  try {
    for await (const chunk of chunks) out.push(chunk);
  } catch (error) {
    return { chunks: out, failed: (error as { code?: string }).code ?? null };
  }
  return { chunks: out };
};

type Box = string | null;
const SCOPES: [Box, boolean][] = [
  [null, false],
  [null, true],
  ["Junk", false],
  ["INBOX/accounts/a", false],
  ["Sent Messages/accounts/a", true],
  ["Archive", false],
  ["Trash", false],
];

// --- repository: counts, UID lists, flag pushes ----------------------------------------

for (const [mailbox, sent] of SCOPES) {
  const args = { mailbox, sent };
  const call = (fn: "countMessages" | "getAllUids" | "getFirstUnseenUid" | "expungeDeletedMails") => () =>
    repository[fn](USER, mailbox, sent);

  await traced("repository", "countMessages", args, [rows({ total: 7, unread: 2 })], call("countMessages"));
  await traced("repository", "countMessages", args, [rows({ total: "12", unread: null })], call("countMessages"));
  await traced("repository", "countMessages", args, [rows()], call("countMessages"));
  await traced("repository", "countMessages", args, [fault("08006")], call("countMessages"));

  await traced("repository", "getAllUids", args, [rows({ uid: 4 }, { uid: 8 }, { uid: 15 })], call("getAllUids"));
  await traced("repository", "getAllUids", args, [rows()], call("getAllUids"));
  await traced("repository", "getAllUids", args, [fault("57014")], call("getAllUids"));

  await traced("repository", "getFirstUnseenUid", args, [rows({ uid: 42 })], call("getFirstUnseenUid"));
  await traced("repository", "getFirstUnseenUid", args, [rows()], call("getFirstUnseenUid"));
  await traced("repository", "getFirstUnseenUid", args, [fault("57014")], call("getFirstUnseenUid"));

  for (const mail_ids of [[], [MAIL], [MAIL, OTHER_MAIL, "not-a-uuid"]]) {
    const flagRows = mail_ids.length
      ? [rows(
          { mail_id: MAIL, uid: 9, read: true, saved: false, deleted: null, draft: false, answered: true },
          { mail_id: OTHER_MAIL, uid: 11, read: false, saved: true, deleted: true, draft: true, answered: false }
        )]
      : [];
    await traced("repository", "getMailFlagUpdates", { ...args, mail_ids }, flagRows, () =>
      repository.getMailFlagUpdates(USER, mailbox, sent, mail_ids)
    );
  }
  await traced("repository", "getMailFlagUpdates", { ...args, mail_ids: [MAIL] }, [fault("08006")], () =>
    repository.getMailFlagUpdates(USER, mailbox, sent, [MAIL])
  );
}

// --- repository: range reads -----------------------------------------------------------

/** One row of `mails`, whole. */
const mailRow = (mail_id: string, overrides: Row = {}): Row => ({
  mail_id,
  user_id: USER,
  message_id: `<${mail_id.slice(0, 8)}@example.com>`,
  subject: "Quarterly report — ünïcode ✓",
  date: DATE,
  html: "<p>Hello <b>world</b> 😀</p>",
  text: "Hello world 😀",
  from_address: [{ address: "sender@example.com", name: "Sender Name" }],
  from_text: "Sender Name <sender@example.com>",
  to_address: [{ address: "a@alice.mydomain", name: "" }, { address: "other@example.org", name: "Other", group: [] }],
  to_text: "a@alice.mydomain, Other <other@example.org>",
  cc_address: [],
  cc_text: "",
  bcc_address: null,
  bcc_text: null,
  reply_to_address: [{ address: "reply@example.com", name: "Reply" }],
  reply_to_text: null,
  envelope_from: [{ address: "bounce@example.com" }],
  envelope_to: [{ address: "a@alice.mydomain" }],
  attachments: [
    { content: { data: "attachment-id" }, contentType: "application/pdf", filename: "report.pdf", size: 1234 },
  ],
  read: true,
  saved: false,
  sent: false,
  deleted: false,
  draft: false,
  answered: true,
  expunged: false,
  insight: { summary: "A report", tags: ["work"] },
  uid_domain: 7,
  modseq: 12,
  spam_score: 3,
  spam_reasons: [],
  is_spam: false,
  rfc822_size: 4321,
  text_line_count: null,
  html_line_count: null,
  in_reply_to: null,
  reference_ids: null,
  updated: DATE,
  search_vector: "'hello':1 'world':2",
  uid_mailbox: 3,
  text_octets: 16,
  html_octets: 31,
  ...overrides,
});

const BARE_ROW: Row = mailRow(OTHER_MAIL, {
  subject: "",
  html: "",
  text: "",
  from_address: null,
  from_text: "text without a list",
  to_address: null,
  to_text: null,
  reply_to_address: null,
  envelope_from: null,
  envelope_to: null,
  attachments: null,
  insight: null,
  read: false,
  answered: false,
  saved: true,
  deleted: true,
  draft: true,
  sent: true,
  rfc822_size: null,
  uid_domain: 9,
  uid_mailbox: 0,
  text_octets: 0,
  html_octets: 0,
});

/** The rows a statement would return: only the columns it projects. */
const project = (row: Row, columns: string[]): Row =>
  Object.fromEntries(columns.filter((column) => column in row).map((column) => [column, row[column]]));

const modelFields = (models: Map<string, { selectedFields: ReadonlySet<string> }>) =>
  [...models.entries()].map(([id, model]) => [
    id,
    Object.fromEntries([...model.selectedFields].map((field) => [field, (model as unknown as Record<string, unknown>)[field]])),
  ]);

const rangeRead = async (
  mailbox: Box,
  sent: boolean,
  range: [number, number],
  useUid: boolean,
  fields: string[],
  changedSince: number | undefined,
  script: (columns: string[]) => Response[]
) => {
  const { selectedFields } = query.buildMailsByRangeQuery(USER, mailbox, sent, range[0], range[1], useUid, fields, changedSince);
  await traced(
    "repository",
    "getMailsByRange",
    enc({ mailbox, sent, start: range[0], end: range[1], useUid, fields, changedSince }),
    script(selectedFields),
    async () => modelFields(await repository.getMailsByRange(USER, mailbox, sent, range[0], range[1], useUid, fields, changedSince))
  );
};

for (const [mailbox, sent] of SCOPES) {
  for (const fields of [["*"], ["mail_id"], ["read", "uid_domain", "uid_mailbox"], ["subject", "bogus_field"], columnsOf(fetchProjections[8])]) {
    await rangeRead(mailbox, sent, [1, 50], true, fields, undefined, (columns) => [
      rows(project(mailRow(MAIL), columns), project(BARE_ROW, columns)),
    ]);
  }
  await rangeRead(mailbox, sent, [2, 3], false, ["read"], 5, () => [rows()]);
  await rangeRead(mailbox, sent, [1, MAX], true, ["read"], undefined, () => [fault("08006")]);
}
// A row that fails validation fails the whole read; a repeated id keeps its first place.
await rangeRead(null, false, [1, 9], true, ["read"], undefined, () => [rows({ mail_id: MAIL, read: "yes" })]);
await rangeRead(null, false, [1, 9], true, ["uid_domain"], undefined, () => [rows({ mail_id: MAIL })]);
await rangeRead(null, false, [1, 9], true, ["rfc822_size", "from_address"], undefined, () => [
  rows({ mail_id: MAIL, rfc822_size: null, from_address: "not a list" }),
]);
await rangeRead(null, false, [1, 9], true, ["read", "uid_domain"], undefined, () => [
  rows(
    { mail_id: MAIL, read: false, uid_domain: 1 },
    { mail_id: OTHER_MAIL, read: false, uid_domain: 2 },
    { mail_id: MAIL, read: true, uid_domain: 3 }
  ),
]);

// --- repository: STORE -----------------------------------------------------------------

const flagRow = (uid: number, modseq: number, mail_id = MAIL): Row => ({
  mail_id,
  uid,
  read: true,
  saved: uid % 2 === 0,
  deleted: false,
  draft: false,
  answered: true,
  modseq,
});

type StoreOperation = "FLAGS" | "+FLAGS" | "-FLAGS";
const storeCase = async (
  mailbox: Box,
  sent: boolean,
  range: [number, number],
  flags: string[],
  useUid: boolean,
  operation: StoreOperation,
  unchangedSince: number | undefined,
  script: Response[]
) =>
  traced(
    "repository",
    "setMailFlags",
    enc({ mailbox, sent, start: range[0], end: range[1], flags, useUid, operation, unchangedSince }),
    script,
    () => repository.setMailFlags(USER, mailbox, sent, range[0], range[1], flags, useUid, operation, unchangedSince)
  );

const reserved = (next_uid: number | string | null) => rows({ next_uid });
for (const [mailbox, sent] of SCOPES) {
  for (const useUid of [true, false]) {
    // A STORE that names no stored flag: the rows as they stand.
    await storeCase(mailbox, sent, [3, 9], [], useUid, "+FLAGS", undefined, [rows(flagRow(4, 7), flagRow(5, 8))]);
    await storeCase(mailbox, sent, [3, 9], ["$Forwarded"], useUid, "-FLAGS", 7, [rows(flagRow(4, 7), flagRow(5, 8), flagRow(6, 9))]);
    await storeCase(mailbox, sent, [3, 9], [], useUid, "+FLAGS", 0, [rows(flagRow(4, 1))]);
    await storeCase(mailbox, sent, [3, 9], [], useUid, "+FLAGS", undefined, [fault("08006")]);
    // An unconditional STORE: one reservation, one UPDATE.
    await storeCase(mailbox, sent, [3, 9], ["\\Seen"], useUid, "+FLAGS", undefined, [reserved(12), rows(flagRow(4, 12), flagRow(5, 12))]);
    await storeCase(mailbox, sent, [3, 9], ["\\Seen", "\\Deleted"], useUid, "-FLAGS", undefined, [reserved(13), rows()]);
    await storeCase(mailbox, sent, [1, MAX], [], useUid, "FLAGS", undefined, [reserved(14), rows(flagRow(1, 14))]);
    await storeCase(mailbox, sent, [3, 9], ["\\Flagged"], useUid, "FLAGS", undefined, [fault("40001")]);
    await storeCase(mailbox, sent, [3, 9], ["\\Flagged"], useUid, "FLAGS", undefined, [reserved(15), fault("57014")]);
    // A conditional STORE: the matched UIDs, then the guarded UPDATE.
    await storeCase(mailbox, sent, [4, 6], ["\\Deleted"], useUid, "+FLAGS", 15, [
      reserved(20),
      rows({ uid: 4 }, { uid: 5 }, { uid: 6 }),
      rows(flagRow(5, 20)),
    ]);
    await storeCase(mailbox, sent, [4, 6], ["\\Deleted"], useUid, "+FLAGS", 0, [reserved(21), rows({ uid: 4 }), rows()]);
    await storeCase(mailbox, sent, [4, 6], ["\\Deleted"], useUid, "+FLAGS", 99, [reserved(22), rows(), rows()]);
    await storeCase(mailbox, sent, [4, 6], ["\\Deleted"], useUid, "+FLAGS", 15, [reserved(23), fault("08006")]);
    await storeCase(mailbox, sent, [4, 6], ["\\Deleted"], useUid, "+FLAGS", 15, [reserved(24), rows({ uid: 4 }), fault("08006")]);
  }
}
// Numbers PostgreSQL cannot read reach it as JavaScript prints them.
await storeCase(null, false, [1e21, Infinity], ["\\Seen"], true, "+FLAGS", undefined, [reserved(3), fault("22P02")]);
await storeCase("Archive", false, [NaN, NaN], ["\\Seen"], false, "+FLAGS", 1e21, [reserved(4), rows(), rows()]);
await storeCase(null, false, [-1, -1], [], true, "+FLAGS", NaN, [rows(flagRow(4, 7))]);
await storeCase(null, false, [1, 5], [], true, "+FLAGS", Infinity, [rows(flagRow(4, 7))]);
// A reservation the counter answers oddly reads as 1.
await storeCase(null, false, [1, 5], ["\\Seen"], true, "+FLAGS", undefined, [reserved(null), rows(flagRow(4, 1))]);

// --- repository: SEARCH ----------------------------------------------------------------

for (const [mailbox, sent] of SCOPES) {
  for (const criteria of searches) {
    const failsToBind = "threw" in (await bind(query.buildSearchMailsByUidQuery(USER, mailbox, sent, criteria).values));
    await traced(
      "repository",
      "searchMailsByUid",
      { mailbox, sent, criteria: enc(criteria) },
      failsToBind ? [] : [rows({ uid: 0 }, { uid: 3 }, { uid: 5 }, { uid: -2 })],
      () => repository.searchMailsByUid(USER, mailbox, sent, criteria)
    );
  }
  await traced("repository", "searchMailsByUid", { mailbox, sent, criteria: [{ type: "SEEN" }] }, [fault("08006")], () =>
    repository.searchMailsByUid(USER, mailbox, sent, [{ type: "SEEN" }])
  );
}

// --- repository: EXPUNGE ---------------------------------------------------------------

for (const [mailbox, sent] of SCOPES) {
  const args = { mailbox, sent };
  const expunge = () => repository.expungeDeletedMails(USER, mailbox, sent);
  const domainScoped = mailbox === null || mailbox === "Junk";
  // Only the unified Sent view expunges with one UPDATE; INBOX and Junk look
  // up what they may remove first, as a mapped box does.
  if (domainScoped && sent) {
    await traced("repository", "expungeDeletedMails", args, [reserved(31), rows({ uid: 2 }, { uid: 7 })], expunge);
    await traced("repository", "expungeDeletedMails", args, [reserved(32), rows()], expunge);
    await traced("repository", "expungeDeletedMails", args, [fault("08006")], expunge);
    await traced("repository", "expungeDeletedMails", args, [reserved(33), fault("08006")], expunge);
  } else {
    const found = rows({ mail_id: MAIL, uid: 4 }, { mail_id: OTHER_MAIL, uid: 9 });
    await traced("repository", "expungeDeletedMails", args, [found, reserved(34), rows({ mail_id: OTHER_MAIL }, { mail_id: MAIL })], expunge);
    await traced("repository", "expungeDeletedMails", args, [found, reserved(35), rows({ mail_id: THIRD_MAIL })], expunge);
    await traced("repository", "expungeDeletedMails", args, [rows()], expunge);
    await traced("repository", "expungeDeletedMails", args, [fault("08006")], expunge);
    await traced("repository", "expungeDeletedMails", args, [found, fault("08006")], expunge);
    await traced("repository", "expungeDeletedMails", args, [found, reserved(36), fault("08006")], expunge);
  }

  for (const uids of [[], [4], [9, 4, 11]]) {
    const byUid = () => repository.expungeMailsByUid(USER, mailbox, sent, uids);
    const uidArgs = { ...args, uids };
    if (uids.length === 0) {
      await traced("repository", "expungeMailsByUid", uidArgs, [], byUid);
    } else if (domainScoped) {
      await traced("repository", "expungeMailsByUid", uidArgs, [reserved(40), rows({ uid: 4 })], byUid);
      await traced("repository", "expungeMailsByUid", uidArgs, [fault("08006")], byUid);
      await traced("repository", "expungeMailsByUid", uidArgs, [reserved(41), fault("57014")], byUid);
    } else {
      const found = rows({ mail_id: MAIL, uid: 4 }, { mail_id: OTHER_MAIL, uid: 9 });
      await traced("repository", "expungeMailsByUid", uidArgs, [found, reserved(42), rows({ mail_id: MAIL }, { mail_id: OTHER_MAIL })], byUid);
      await traced("repository", "expungeMailsByUid", uidArgs, [rows()], byUid);
      await traced("repository", "expungeMailsByUid", uidArgs, [fault("08006")], byUid);
      await traced("repository", "expungeMailsByUid", uidArgs, [found, fault("08006")], byUid);
      await traced("repository", "expungeMailsByUid", uidArgs, [found, reserved(43), fault("57014")], byUid);
    }
  }
}

// --- repository: the chunk readers -----------------------------------------------------

for (const source of ["text", "html"] as const) {
  const text = (script: Response[], chunkChars?: number) =>
    traced("repository", "pgTextChunks", enc({ mail_id: MAIL, user_id: USER, source, chunkChars }), script, () =>
      drained(repository.pgTextChunks(MAIL, USER, source, chunkChars))
    );
  await text([rows({ chunk: "ab🙂" }), rows({ chunk: "cd🙂" }), rows({ chunk: "e" })], 3);
  await text([rows({ chunk: "abc" }), rows({ chunk: "" })], 3);
  await text([rows({ chunk: "abc" }), rows()], 3);
  await text([rows({ chunk: null })], 3);
  await text([rows({ chunk: "short" })]);
  await text([rows()]);
  await text([fault("57014")], 3);
  await text([rows({ chunk: "abc" }), fault("08006")], 3);

  const bytes = (script: Response[], startByte?: number, chunkBytes?: number) =>
    traced("repository", "pgByteChunks", enc({ mail_id: MAIL, user_id: USER, source, startByte, chunkBytes }), script, () =>
      drained(repository.pgByteChunks(MAIL, USER, source, startByte, chunkBytes))
    );
  await bytes([rows({ chunk: { $hex: "616263" } }), rows({ chunk: { $hex: "f09f" } })], 7, 3);
  await bytes([rows({ chunk: { $hex: "616263" } }), rows({ chunk: { $hex: "" } })], 1, 3);
  await bytes([rows({ chunk: { $hex: "616263" } }), rows()], 1, 3);
  await bytes([rows({ chunk: null })], 4, 3);
  await bytes([rows({ chunk: { $hex: "00ff10" } })]);
  await bytes([rows({ chunk: { $hex: "61" } })], 49153);
  await bytes([fault("57014")], 1, 3);
  await bytes([rows({ chunk: { $hex: "616263" } }), fault("08006")], 1, 3);
}

// --- store -----------------------------------------------------------------------------

const users = {
  alice: { id: USER, username: "alice", email: "alice@alice.mydomain" },
  admin: { id: USER, username: "admin", email: "admin@mydomain" },
};
type UserName = keyof typeof users;
// A plain login on the user's own domain, as the session opens one.
const ownDomainOf = (user: UserName) => (user === "admin" ? "mydomain" : `${user}.mydomain`);
const storeOf = (user: UserName) =>
  new Store(new SignedUser(users[user]), {
    accountDomains: [ownDomainOf(user)],
    primaryDomain: ownDomainOf(user),
  });

const mailboxRow = (name: string, overrides: Row = {}): Row => ({
  mailbox_id: `mailbox-${name}`,
  user_id: USER,
  name,
  address: null,
  parent_id: null,
  uid_validity: 1,
  uid_next: 1,
  subscribed: true,
  special_use: null,
  created: DATE,
  ...overrides,
});
const addresses = (...list: (string | null)[]) => rows(...list.map((address) => ({ address })));

const LISTINGS: Record<string, Response[]> = {
  empty: [rows(), rows(), rows()],
  received: [addresses("work@alice.mydomain", "home@alice.mydomain"), rows(), rows()],
  sent: [rows(), addresses("me@alice.mydomain"), rows()],
  full: [
    addresses("work@alice.mydomain", "home @alice.mydomain", "", "work@alice.mydomain", "Trash@alice.mydomain", "plain"),
    addresses("me@alice.mydomain", " me@alice.mydomain", "work@alice.mydomain"),
    rows(
      mailboxRow("Archive"),
      mailboxRow("Projects/Wörk"),
      mailboxRow("Projects", { subscribed: false }),
      mailboxRow("trash"),
      mailboxRow("inbox/accounts/WORK"),
      mailboxRow("Sent Messages"),
      mailboxRow(" Padded "),
      mailboxRow("Role", { special_use: "\\Archive" }),
      mailboxRow("Tied", { address: "tied@alice.mydomain" }),
      mailboxRow("Archive", { mailbox_id: "mailbox-Archive-2", subscribed: false }),
      mailboxRow("archive")
    ),
  ],
  statsFail: [fault("08006"), fault("57014"), rows(mailboxRow("Archive"))],
  mailboxesFail: [addresses("work@alice.mydomain"), rows(), fault("08006")],
  badRow: [rows(), rows(), rows({ name: "no other column" })],
};

for (const user of ["alice", "admin"] as UserName[]) {
  for (const [listing, script] of Object.entries(LISTINGS)) {
    await traced("store", "listMailboxes", { user, listing }, script, () => storeOf(user).listMailboxes());
    await traced("store", "listMailboxEntries", { user, listing }, script, () => storeOf(user).listMailboxEntries());
    for (const box of ["Archive", "archive", "Drafts", "drafts", "INBOX/accounts/work", "Sent Messages", "Sent Messages/accounts/me", "Projects", "Nope", "", " Padded ", "Padded"]) {
      await traced("store", "mailboxExists", { user, listing, box }, script, () => storeOf(user).mailboxExists(box));
    }
  }
  for (const box of ["INBOX", "inbox", "Inbox"]) {
    await traced("store", "mailboxExists", { user, listing: null, box }, [], () => storeOf(user).mailboxExists(box));
  }
}

/** Every kind of mailbox name the session hands the store. */
const BOXES = [
  "INBOX",
  "inbox",
  "Sent Messages",
  "sent messages",
  "Drafts",
  "Junk",
  "Starred",
  "starred",
  "Trash",
  "INBOX/accounts/work",
  "Sent Messages/accounts/work",
  "INBOX/accounts",
  "Sent Messages/accounts",
  "Archive",
  "Projects/Wörk",
];

for (const user of ["alice", "admin"] as UserName[]) {
  for (const box of BOXES) {
    const store = storeOf(user);
    const args = { user, box };
    await traced("store", "countMessages", args, [rows({ total: 7, unread: 2 })], () => store.countMessages(box));
    await traced("store", "countMessages", args, [fault("08006")], () => store.countMessages(box));
    await traced("store", "getUidNext", args, [rows({ uid_next: 17 })], () => store.getUidNext(box));
    await traced("store", "getUidNext", args, [rows({ uid_next: null })], () => store.getUidNext(box));
    await traced("store", "getUidNext", args, [fault("08006")], () => store.getUidNext(box));
    await traced("store", "getAllUids", args, [rows({ uid: 4 }, { uid: 8 })], () => store.getAllUids(box));
    await traced("store", "getAllUids", args, [fault("08006")], () => store.getAllUids(box));
    await traced("store", "getFirstUnseenUid", args, [rows({ uid: 42 })], () => store.getFirstUnseenUid(box));
    await traced("store", "getFirstUnseenUid", args, [fault("08006")], () => store.getFirstUnseenUid(box));
    await traced("store", "getHighestModseq", args, [rows({ highest: 77 })], () => store.getHighestModseq(box));
    await traced("store", "getHighestModseq", args, [fault("08006")], () => store.getHighestModseq(box));
    await traced("store", "getMailFlagUpdates", { ...args, mailIds: [MAIL] }, [rows({ mail_id: MAIL, uid: 9, read: true, saved: 0, deleted: null, draft: "", answered: 1 })], () =>
      store.getMailFlagUpdates(box, [MAIL])
    );
    await traced("store", "getMailFlagUpdates", { ...args, mailIds: [MAIL] }, [fault("08006")], () => store.getMailFlagUpdates(box, [MAIL]));
    await traced("store", "getMailFlagUpdates", { ...args, mailIds: [] }, [], () => store.getMailFlagUpdates(box, []));
    await traced("store", "expungeUids", { ...args, uids: [] }, [], () => store.expungeUids(box, []));
  }
}

// --- store: range reads ----------------------------------------------------------------

const CLONE_FIELDS = [
  "subject", "date", "html", "text", "from", "to", "cc", "bcc", "replyTo", "envelopeFrom", "envelopeTo",
  "attachments", "messageId", "insight", "read", "saved", "deleted", "draft", "answered", "uid",
];
const messages = async (
  box: string,
  range: [number, number],
  fields: string[],
  useUid: boolean | undefined,
  changedSince: number | undefined,
  script: (columns: string[]) => Response[]
) => {
  const store = storeOf("alice");
  const columns = ["mail_id", ...columnsOf(fields)];
  await traced("store", "getMessages", enc({ box, start: range[0], end: range[1], fields, useUid, changedSince }), script(columns), () =>
    store.getMessages(box, range[0], range[1], fields, useUid, changedSince)
  );
};
for (const box of ["INBOX", "Sent Messages", "Drafts", "Trash", "INBOX/accounts/work", "Archive"]) {
  for (const fields of [...fetchProjections, CLONE_FIELDS, ["*"], [], ["uid"], ["sent", "envelopeTo", "rfc822_size"]]) {
    await messages(box, [1, 50], fields, true, undefined, (columns) =>
      fields.length === 1 && fields[0] === "*"
        ? [rows(mailRow(MAIL), BARE_ROW)]
        : [rows(project(mailRow(MAIL), columns), project(BARE_ROW, columns))]
    );
  }
  await messages(box, [2, 4], ["uid", "read"], false, 12, () => [rows()]);
  await messages(box, [1, MAX], ["uid", "read"], undefined, undefined, () => [fault("08006")]);
  await messages(box, [1, 1], ["uid", "read"], true, undefined, () => [rows({ mail_id: MAIL, uid_domain: "seven" })]);
}

// --- store: STORE, EXPUNGE, SEARCH -----------------------------------------------------

for (const box of BOXES) {
  const store = storeOf("alice");
  const flagsArgs = (flags: string[], useUid: boolean | undefined, operation: StoreOperation | undefined, unchangedSince?: number) =>
    enc({ box, start: 3, end: 9, flags, useUid, operation, unchangedSince });
  await traced("store", "setFlags", flagsArgs(["\\Seen"], true, "+FLAGS"), [reserved(12), rows(flagRow(4, 12))], () =>
    store.setFlags(box, 3, 9, ["\\Seen"], true, "+FLAGS")
  );
  await traced("store", "setFlags", flagsArgs(["\\Deleted"], true, "-FLAGS", 15), [reserved(20), rows({ uid: 4 }, { uid: 5 }), rows(flagRow(5, 20))], () =>
    store.setFlags(box, 3, 9, ["\\Deleted"], true, "-FLAGS", 15)
  );
  await traced("store", "setFlags", flagsArgs([], undefined, undefined), [reserved(21), rows(flagRow(4, 21))], () =>
    store.setFlags(box, 3, 9, [])
  );
  await traced("store", "setFlags", flagsArgs(["Junk"], false, "+FLAGS"), [fault("08006")], () => store.setFlags(box, 3, 9, ["Junk"], false, "+FLAGS"));

  // The repository's branch: INBOX in any case, the unified Sent view only as spelled, the utility views in any case.
  const domainScoped =
    box.toUpperCase() === "INBOX" || box === "Sent Messages" || ["drafts", "junk"].includes(box.toLowerCase());
  const found = rows({ mail_id: MAIL, uid: 4 }, { mail_id: OTHER_MAIL, uid: 9 });
  // EXPUNGE takes the one-UPDATE branch on the unified Sent view only: INBOX
  // and the utility views look up what they may remove first.
  await traced(
    "store",
    "expunge",
    { box },
    box === "Sent Messages" ? [reserved(31), rows({ uid: 2 }, { uid: 7 })] : [found, reserved(31), rows({ mail_id: MAIL })],
    () => store.expunge(box)
  );
  await traced("store", "expunge", { box }, [fault("08006")], () => store.expunge(box));
  await traced(
    "store",
    "expungeUids",
    { box, uids: [4, 9] },
    domainScoped ? [reserved(40), rows({ uid: 9 })] : [found, reserved(40), rows({ mail_id: OTHER_MAIL }, { mail_id: MAIL })],
    () => store.expungeUids(box, [4, 9])
  );
  await traced("store", "expungeUids", { box, uids: [4, 9] }, [fault("08006")], () => store.expungeUids(box, [4, 9]));
}

for (const box of ["INBOX", "Sent Messages", "Junk", "Trash", "INBOX/accounts/work", "Archive"]) {
  const store = storeOf("alice");
  for (const keys of SEARCH_KEYS) {
    const line = `a UID SEARCH ${keys}`;
    const parsed = parseCommand(line);
    if (!parsed.success || !parsed.value) continue;
    const request = parsed.value.request;
    const search = request.type === "UID" ? request.data.request : request;
    if (search.type !== "SEARCH") throw new Error(`${line} is not a SEARCH`);
    const criteria = search.data.criteria;
    // The store drops a criterion that simplifies to nothing, as here.
    const simplified = criteria.map(simplifyCriterion).filter((criterion) => criterion !== null);
    const failsToBind = "threw" in (await bind(query.buildSearchMailsByUidQuery(USER, null, false, simplified).values));
    await traced("store", "search", { box, line }, failsToBind ? [] : [rows({ uid: 3 }, { uid: 0 }, { uid: 5 })], () =>
      store.search(box, criteria)
    );
  }
  await traced("store", "search", { box, line: "a UID SEARCH SEEN" }, [fault("08006")], () =>
    store.search(box, [{ type: "SEEN" }])
  );
}

// --- store: storing a mail -------------------------------------------------------------

type MailInit = ConstructorParameters<typeof Mail>[0];
const address = (list: Json, text: string) => ({ value: list, text }) as never;

const MAILS: Record<string, MailInit> = {
  minimal: { messageId: "minimal@example.com", date: DATE },
  appended: {
    messageId: "appended@example.com",
    inReplyTo: "<parent@example.com>",
    references: ["<root@example.com>", "<parent@example.com>"],
    subject: "Appended — ünïcode ✓",
    date: DATE,
    html: "<p>Hello <b>world</b> 😀</p>",
    text: "Hello world 😀",
    from: address([{ address: "sender@example.com", name: "Sender Name" }], "Sender Name <sender@example.com>"),
    to: address([{ address: "work@alice.mydomain", name: "" }], "work@alice.mydomain"),
    cc: address([], ""),
    bcc: address([{ address: "bcc@example.com" }], "bcc@example.com"),
    replyTo: address([{ address: "reply@example.com", name: "Reply", extra: { kept: true } }], "Reply <reply@example.com>"),
    attachments: [{ content: { data: "no-such-attachment" }, contentType: "application/pdf", filename: "report.pdf", size: 1234 }],
    read: true,
    answered: true,
    uid: { domain: 41, account: 7 },
  },
  copied: {
    messageId: "0123456789abcdef.copy@server",
    subject: "Copied",
    date: DATE,
    text: "body",
    to: address([{ address: "Archive@alice.mydomain", name: "" }], "work@alice.mydomain"),
    envelopeFrom: [{ address: "bounce@example.com" }],
    envelopeTo: [{ address: "Archive@alice.mydomain", name: "" }],
    insight: { summary: "A copy" } as never,
    references: [],
    saved: true,
    deleted: true,
    draft: true,
    sent: true,
    uid: { domain: 42, account: 8 },
  },
};

const DESTINATIONS: (string | undefined)[] = [
  undefined,
  "",
  "INBOX",
  "inbox",
  "Sent Messages",
  "Drafts",
  "drafts",
  "Junk",
  "Starred",
  "Trash",
  "INBOX/accounts/work",
  "Sent Messages/accounts/work",
  "Archive",
];

/** The writes a save issues after its INSERT, for the script. */
const followUps = (mail: MailInit, destination: string | undefined): Response[] => {
  const lower = (destination ?? "").toLowerCase();
  const domainScoped = ["inbox", "sent messages", "drafts", "junk"].includes(lower);
  const mapped = destination ? !domainScoped : false;
  const inboxScope = destination
    ? lower === "inbox" || destination.startsWith("INBOX/accounts/") || lower === "drafts" || lower === "junk"
    : false;
  const sent = mail?.sent ?? false;
  const script: Response[] = [];
  // A mapping row is written only for a UID that was reserved.
  if (mapped && (mail?.uid?.account ?? 0) > 0) script.push(rows({ uid: 70 }));
  if (inboxScope && !sent && (mail?.uid?.domain ?? 0) > 0) script.push(rows({ uid: 71 }));
  const saved = lower === "starred" ? true : (mail?.saved ?? false);
  const deleted = lower === "trash" ? true : (mail?.deleted ?? false);
  // A flag that is set draws a pivot unless the destination is that pivot's own box.
  if (saved && destination !== "Starred") script.push(reserved(80), rows({ uid: 80 }));
  if (deleted && destination !== "Trash") script.push(reserved(81), rows({ uid: 81 }));
  return script;
};

for (const [name, init] of Object.entries(MAILS)) {
  for (const destination of DESTINATIONS) {
    const run = async () => {
      const mail = new Mail(structuredClone(init));
      const ok = await storeOf("alice").storeMail(mail, destination);
      return { ok, uid: mail.uid };
    };
    const args = enc({ mail: init, name, destination });
    await traced("store", "storeMail", args, [reserved(9), rows({ mail_id: MAIL }), ...followUps(init, destination)], run);
  }
}
// The INSERT returns no row; the reservation fails; the INSERT fails.
const failing = async (name: string, destination: string, script: Response[]) =>
  traced("store", "storeMail", enc({ mail: MAILS[name], name, destination }), script, async () => {
    const mail = new Mail(structuredClone(MAILS[name]));
    const ok = await storeOf("alice").storeMail(mail, destination);
    return { ok, uid: mail.uid };
  });
await failing("appended", "Archive", [reserved(9), rows()]);
await failing("appended", "Archive", [fault("08006")]);
await failing("appended", "Archive", [reserved(9), fault("22001")]);
await failing("appended", "Archive", [reserved(9), rows({ mail_id: MAIL }), fault("08006")]);
// The Message-ID is already held: the copy merges into the row that exists
// and reports that row's UIDs. The merge rewrites the envelope (an empty list
// is still a list), sets a utility destination's flag under a fresh
// mod-sequence, then records the mapping; a destination's own pivot is not
// written twice.
await failing("appended", "Archive", [
  reserved(9),
  fault("23505"),
  rows(mailRow(OTHER_MAIL, { uid_mailbox: undefined as never, text_octets: undefined as never, html_octets: undefined as never, uid_domain: 5 })),
  rows(),
  rows({ uid: 3 }),
]);
await failing("copied", "Trash", [
  reserved(9),
  fault("23505"),
  rows(mailRow(OTHER_MAIL, { uid_mailbox: undefined as never, text_octets: undefined as never, html_octets: undefined as never, uid_domain: 5, sent: true })),
  rows(),
  reserved(10),
  rows(),
  rows({ uid: 6 }),
]);
await failing("copied", "Starred", [
  reserved(9),
  fault("23505"),
  rows(mailRow(OTHER_MAIL, { uid_mailbox: undefined as never, text_octets: undefined as never, html_octets: undefined as never, uid_domain: 5 })),
  rows(),
  reserved(10),
  rows(),
  rows({ uid: 6 }),
]);
await failing("appended", "INBOX/accounts/work", [
  reserved(9),
  fault("23505"),
  rows(mailRow(OTHER_MAIL, { uid_mailbox: undefined as never, text_octets: undefined as never, html_octets: undefined as never, uid_domain: 5 })),
  rows(),
  rows({ uid: 3 }),
  rows({ uid: 5 }),
]);
await failing("copied", "Trash", [reserved(9), fault("23505"), rows()]);

// ---------------------------------------------------------------------------

const here = import.meta.dir;
const write = (name: string, value: unknown) => fs.writeFileSync(path.join(here, name), `${JSON.stringify(value, null, 1)}\n`);
write("builders.json", { generatedBy: "src/server-rs/tests/golden/imapdb/generate.ts", ...builders });
write("traces.json", { generatedBy: "src/server-rs/tests/golden/imapdb/generate.ts", ...traces });

const count = (groups: Record<string, unknown[]>) =>
  Object.entries(groups)
    .map(([group, list]) => `${group} ${list.length}`)
    .join(", ");
process.stdout.write(`builders: ${count(builders)}\ntraces: ${count(traces)}\nsearch lines that do not parse: ${unparsed}\n`);
void drain;
void optionalNumber;
process.exit(0);
