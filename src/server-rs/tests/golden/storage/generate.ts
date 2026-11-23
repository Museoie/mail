/**
 * Generates the golden fixtures the Rust mail-storage tests compare against,
 * from the real TypeScript repositories:
 *
 *   bun src/server-rs/tests/golden/storage/generate.ts
 *
 * - `builders.json`: what the pure functions return (statement builders, view
 *   rules, mapping and pivot decisions, model validation, RFC 822 sizes).
 * - `traces.json`: the statements each repository function issues, in order,
 *   with the values it binds and what it returns. The pool is replaced by a
 *   script that answers each statement from a list, so no database is needed
 *   and every branch (a lost race, a unique violation, a dropped connection)
 *   can be reached on demand.
 *
 * `src/postgres/repositories/mails/golden_tests.rs` replays both files
 * through the Rust port. Re-run this script whenever the TypeScript changes.
 */
import fs from "fs";
import os from "os";
import path from "path";
import { Pool, DatabaseError } from "../../../../server/lib/postgres/pool";

process.env.NODE_ENV ??= "test";

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
type Response = { rows: Record<string, Json>[]; rowCount?: number } | { error: { code?: string; message: string } };

// ---------------------------------------------------------------------------
// The scripted pool
// ---------------------------------------------------------------------------

let responses: Response[] = [];
let trace: { sql: string; values: unknown[] }[] = [];

(Pool.prototype as unknown as { query: unknown }).query = async function (
  textOrConfig: string | { text: string; values?: unknown[] },
  values?: unknown[]
) {
  const sql = typeof textOrConfig === "string" ? textOrConfig : textOrConfig.text;
  const bound = (typeof textOrConfig === "string" ? values : textOrConfig.values) ?? [];
  trace.push({ sql, values: bound });
  const response = responses.shift();
  if (!response) throw new Error(`no scripted response for statement ${trace.length}: ${sql}`);
  if ("error" in response) {
    throw new DatabaseError(
      Object.assign(new Error(response.error.message), { errno: response.error.code })
    );
  }
  // Rows are handed out as copies: a model may keep the object it was given.
  const rows = structuredClone(response.rows);
  return { rows, rowCount: response.rowCount ?? rows.length, command: "" };
};

const models = await import("../../../../server/lib/postgres/models");
const core = await import("../../../../server/lib/postgres/repositories/mails/core");
const coreQuery = await import("../../../../server/lib/postgres/repositories/mails/core-query");
const counters = await import("../../../../server/lib/postgres/repositories/mails/counters");
const mapping = await import("../../../../server/lib/postgres/repositories/mails/mapping-decisions");
const pivots = await import("../../../../server/lib/postgres/repositories/mails/pivot-decisions");
const setFlags = await import("../../../../server/lib/postgres/repositories/mails/set-flags-query");
const views = await import("../../../../server/lib/postgres/repositories/mails/views");
const mailboxes = await import("../../../../server/lib/postgres/repositories/mailboxes");
const sessionUtils = await import("../../../../server/lib/imap/session-utils");

// The repositories log what they absorb and rethrow; the fixtures record it.
console.error = () => undefined;
console.warn = () => undefined;
console.log = () => undefined;
console.info = () => undefined;

// ---------------------------------------------------------------------------
// Fixed values
// ---------------------------------------------------------------------------

const USER = "11111111-1111-4111-8111-111111111111";
const MAIL = "22222222-2222-4222-8222-222222222222";
const OTHER_MAIL = "33333333-3333-4333-8333-333333333333";
const MAILBOX = "44444444-4444-4444-8444-444444444444";
const FIXED_IDS = [USER, MAIL, OTHER_MAIL, MAILBOX];
const DATE = "2024-01-02T03:04:05.000Z";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const ISO = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/;

/** Generated ids and the save time differ per run; everything else is exact. */
const normalize = (value: unknown): Json => {
  if (value === undefined || value === null) return null;
  if (value instanceof Date) return value.toISOString();
  if (typeof value === "string") {
    if (UUID.test(value) && !FIXED_IDS.includes(value)) return "<uuid>";
    if (ISO.test(value) && value !== DATE) return "<now>";
    return value;
  }
  return value as Json;
};

// ---------------------------------------------------------------------------
// Argument encoding shared with the Rust tests
// ---------------------------------------------------------------------------

const UNDEFINED = { $undefined: true };
const NOW = { $dbNow: true };

/** Turns the JSON spelling of a filter or data bag into what the Table takes. */
const decode = (value: unknown): unknown => {
  if (value === null || typeof value !== "object") return value;
  if (Array.isArray(value)) return value.map(decode);
  const object = value as Record<string, unknown>;
  if (object.$undefined === true) return undefined;
  if (object.$dbNow === true) return models.DB_NOW;
  return Object.fromEntries(Object.entries(object).map(([key, entry]) => [key, decode(entry)]));
};

const tables: Record<string, typeof models.mailsTable> = {
  mails: models.mailsTable,
  mailboxes: models.mailboxesTable as unknown as typeof models.mailsTable,
  mail_uid_counters: models.mailUidCountersTable as unknown as typeof models.mailsTable,
  mail_mailbox_uid: models.mailMailboxUidTable as unknown as typeof models.mailsTable,
};

// ---------------------------------------------------------------------------
// builders.json
// ---------------------------------------------------------------------------

type Args = Record<string, Json>;

const statementBuilders: Record<string, (a: Args) => { sql: string; values: unknown[] }> = {
  buildDomainUidQuery: (a) => counters.buildDomainUidQuery(a.user_id as string, a.sent as boolean),
  buildDomainUidNextQuery: (a) => counters.buildDomainUidNextQuery(a.user_id as string, a.sent as boolean),
  buildAccountUidQuery: (a) =>
    counters.buildAccountUidQuery(a.user_id as string, a.account as string, a.sent as boolean),
  buildAccountUidNextQuery: (a) =>
    counters.buildAccountUidNextQuery(a.user_id as string, a.account as string, a.sent as boolean),
  buildMailboxUidQuery: (a) => counters.buildMailboxUidQuery(a.user_id as string, a.mailbox as string),
  buildMailboxUidNextQuery: (a) => counters.buildMailboxUidNextQuery(a.user_id as string, a.mailbox as string),
  buildWriteMailboxUidQuery: (a) =>
    counters.buildWriteMailboxUidQuery(
      a.user_id as string,
      a.mailbox as string,
      a.mail_id as string,
      a.uid as number
    ),
  buildModseqQuery: (a) => counters.buildModseqQuery(a.user_id as string),
  buildMarkMailSpamQuery: (a) =>
    coreQuery.buildMarkMailSpamQuery(
      a.user_id as string,
      a.mail_id as string,
      a.is_spam as boolean,
      a.modseq as number
    ),
  buildMailExistsQuery: (a) => coreQuery.buildMailExistsQuery(a.user_id as string, a.mail_id as string),
};

const statements: { fn: string; args: Args; sql: string; values: Json[] }[] = [];
const statement = (fn: string, args: Args) => {
  const { sql, values } = statementBuilders[fn](args);
  statements.push({ fn, args, sql, values: values.map(normalize) });
};

for (const sent of [false, true]) {
  statement("buildDomainUidQuery", { user_id: USER, sent });
  statement("buildDomainUidNextQuery", { user_id: USER, sent });
  for (const account of ["user@hoie.kim", "plain", "", "a@b@c", "Üser@example.com"]) {
    statement("buildAccountUidQuery", { user_id: USER, account, sent });
    statement("buildAccountUidNextQuery", { user_id: USER, account, sent });
  }
}
for (const mailbox of ["Starred", "Trash", "Archive/2024", ""]) {
  statement("buildMailboxUidQuery", { user_id: USER, mailbox });
  statement("buildMailboxUidNextQuery", { user_id: USER, mailbox });
}
statement("buildWriteMailboxUidQuery", { user_id: USER, mailbox: "Archive", mail_id: MAIL, uid: 7 });
statement("buildWriteMailboxUidQuery", { user_id: USER, mailbox: "INBOX", mail_id: MAIL, uid: 2147483648 });
statement("buildModseqQuery", { user_id: USER });
for (const is_spam of [true, false]) {
  statement("buildMarkMailSpamQuery", { user_id: USER, mail_id: MAIL, is_spam, modseq: 41 });
}
statement("buildMailExistsQuery", { user_id: USER, mail_id: MAIL });

const MAILBOX_NAMES: (string | null)[] = [
  null,
  "Junk",
  "junk",
  "Drafts",
  "DRAFTS",
  "Drafts2",
  "INBOX",
  "INBOX/accounts",
  "INBOX/accounts/someone",
  "INBOX/accountsish",
  "Inbox/accounts/someone",
  "Sent Messages",
  "Sent Messages/accounts/someone",
  "Archive",
  "Starred",
  "Trash",
  "constructor",
  "toString",
  "",
  "JUNKK".slice(0, 3) + "K",
  "ınbox",
];

const setFlagsCases = [];
for (const mailbox of [null, "Junk", "drafts", "INBOX/accounts/someone", "Sent Messages/accounts/someone", "Archive", "Starred", ""]) {
  for (const sent of [false, true]) {
    for (const useUid of [true, false]) {
      for (const conditional of [false, true]) {
        const args = {
          user_id: USER,
          mailbox,
          sent,
          start: 3,
          end: 9,
          useUid,
          setClause: useUid ? "read = TRUE" : "saved = FALSE, deleted = TRUE",
          conditional,
        };
        const built = setFlags.buildSetMailFlagsQueries(
          args.user_id,
          args.mailbox,
          args.sent,
          args.start,
          args.end,
          args.useUid,
          args.setClause,
          args.conditional
        );
        setFlagsCases.push({ args, ...built, baseValues: built.baseValues.map(normalize) });
      }
    }
  }
}

const viewCases = [];
for (const mailbox of MAILBOX_NAMES) {
  for (const sent of [false, true]) {
    for (const prefix of ["", "m."]) {
      viewCases.push({
        mailbox,
        sent,
        prefix,
        isUtilityView: views.isUtilityView(mailbox),
        usesDomainUidSpace: views.usesDomainUidSpace(mailbox),
        isInboxTree: views.isInboxTree(mailbox, sent),
        filter: Object.entries(views.membershipFilter(mailbox, sent)),
        expression: views.membershipExpression(mailbox, sent, prefix),
        condition: views.membershipCondition(mailbox, sent, prefix),
      });
    }
  }
}

const destinationCases = [...MAILBOX_NAMES.filter((name): name is string => name !== null), "inbox", "iNbOx", "INBOXER", "INBOX/keep"].map(
  (destination) => ({ destination, view: views.domainViewForDestination(destination) ?? null })
);

const mappingCases = [];
for (const mailbox of [undefined, "", "INBOX/accounts/admin", "Archive"]) {
  for (const uid_mailbox of [undefined, 0, -1, 7]) {
    for (const domain_mailbox of [undefined, "", "INBOX"]) {
      for (const uid_domain of [undefined, 0, 42]) {
        for (const sent of [false, true]) {
          const input = { mailbox, uid_mailbox, domain_mailbox, uid_domain, sent };
          mappingCases.push({
            input: Object.fromEntries(Object.entries(input).filter(([, value]) => value !== undefined)),
            writes: mapping.decideMappingWrites(input),
          });
        }
      }
    }
  }
}

const pivotCases = [];
for (const saved of [undefined, true, false]) {
  for (const deleted of [undefined, true, false]) {
    for (const destMailbox of [undefined, "Archive", "Starred", "Trash", "starred", "trash", ""]) {
      const decisions = pivots.decideMappedPivots(saved, deleted, destMailbox);
      // core.ts carries its own copy of the same rule.
      if (JSON.stringify(decisions) !== JSON.stringify(core.decideMappedPivots(saved, deleted, destMailbox))) {
        throw new Error("core.decideMappedPivots and pivot-decisions disagree");
      }
      pivotCases.push({ saved: saved ?? null, deleted: deleted ?? null, destMailbox: destMailbox ?? null, decisions });
    }
  }
}

// --- models ----------------------------------------------------------------

const mailRow = (overrides: Record<string, Json> = {}): Record<string, Json> => ({
  mail_id: MAIL,
  user_id: USER,
  message_id: "<test@example.com>",
  subject: "Hello",
  date: DATE,
  html: "<p>hi</p>",
  text: "hi",
  from_address: [{ address: "sender@example.com", name: "Sender" }],
  from_text: "Sender <sender@example.com>",
  to_address: null,
  to_text: null,
  cc_address: null,
  cc_text: null,
  bcc_address: null,
  bcc_text: null,
  reply_to_address: null,
  reply_to_text: null,
  envelope_from: null,
  envelope_to: null,
  attachments: null,
  read: false,
  saved: false,
  sent: false,
  deleted: false,
  draft: false,
  answered: false,
  expunged: false,
  insight: null,
  uid_domain: 5,
  modseq: 9,
  spam_score: 0,
  spam_reasons: null,
  is_spam: false,
  rfc822_size: null,
  text_line_count: null,
  html_line_count: null,
  in_reply_to: null,
  reference_ids: null,
  updated: DATE,
  search_vector: null,
  ...overrides,
});

const without = (row: Record<string, Json>, ...keys: string[]) =>
  Object.fromEntries(Object.entries(row).filter(([key]) => !keys.includes(key)));

const mailboxRow = (overrides: Record<string, Json> = {}): Record<string, Json> => ({
  mailbox_id: MAILBOX,
  user_id: USER,
  name: "Archive",
  address: null,
  parent_id: null,
  uid_validity: 1,
  uid_next: 1,
  subscribed: true,
  special_use: null,
  created: DATE,
  ...overrides,
});

const outcome = (work: () => unknown): Json => {
  try {
    return { ok: normalize(work()) as Json };
  } catch (error) {
    return { threw: (error as Error).message };
  }
};

const mailModelCases = [
  mailRow(),
  mailRow({ in_reply_to: "<a@x>", reference_ids: ["<a@x>"], spam_reasons: ["SPAM_WORD", "BLACKLIST"] }),
  mailRow({ text_line_count: 12, html_line_count: 34, rfc822_size: 4096, insight: { score: 1 } }),
  mailRow({ attachments: [{ filename: "a.txt", size: 3 }], envelope_to: [{ address: "a@x" }] }),
  without(mailRow(), "in_reply_to", "reference_ids"),
  mailRow({ mail_id: 999 }),
  mailRow({ read: "true" }),
  mailRow({ uid_domain: "0" }),
  mailRow({ reference_ids: "<a@x>" }),
  mailRow({ text_line_count: "12" }),
  mailRow({ from_address: "x", spam_reasons: {}, updated: 5 }),
  without(mailRow(), "updated", "search_vector", "html"),
  { mail_id: 1 },
  null,
  "bad",
  42,
  true,
  [],
].map((data) => ({
  data: data as Json,
  result: outcome(() => new models.MailModel(data).toJSON() as unknown as Json),
}));

const partialMailModelCases = (
  [
    [["mail_id", "subject", "read"], { mail_id: MAIL, subject: "Hey", read: true }],
    [["mail_id"], { mail_id: MAIL }],
    [["nonexistent_field"], { nonexistent_field: "x" }],
    [["totally_fake_col", "mail_id", "another_fake"], {}],
    [["read"], { read: "yes" }],
    [["mail_id", "read"], { mail_id: MAIL, read: false }],
    [[], {}],
    [["from_address"], { from_address: null }],
    [["mail_id", "text_octets", "html_octets"], { mail_id: MAIL, text_octets: 12345, html_octets: 67890 }],
    [["text_octets"], { text_octets: "12345" }],
    [["html_octets"], { html_octets: null }],
    [["uid_mailbox", "uid_domain", "modseq"], { uid_mailbox: 3, uid_domain: 9, modseq: 12, extra: "ignored" }],
    [["in_reply_to", "reference_ids", "subject"], { subject: "s" }],
    [["subject", "date"], {}],
    [["read", "read", "mail_id"], { read: 1, mail_id: 2 }],
    [["mail_id"], null],
    [["mail_id"], "bad"],
    [["bogus"], null],
  ] as [string[], Json][]
).map(([fields, data]) => ({
  fields,
  data,
  result: outcome(() => {
    const partial = new models.PartialMailModel(fields, data) as unknown as Record<string, Json> & {
      selectedFields: Set<string>;
    };
    return {
      selectedFields: [...partial.selectedFields].sort(),
      values: Object.fromEntries(
        [...partial.selectedFields].filter((field) => partial[field] !== undefined).map((field) => [field, partial[field]])
      ),
    };
  }),
}));

const mailboxModelCases = [
  mailboxRow(),
  mailboxRow({ address: "alice@example.com", special_use: "\\Inbox", uid_next: 42 }),
  mailboxRow({ uid_validity: "1" }),
  mailboxRow({ subscribed: "true" }),
  without(mailboxRow(), "created"),
  null,
].map((data) => ({
  data: data as Json,
  result: outcome(() => new models.MailboxModel(data).toJSON() as unknown as Json),
}));

const counterModelCases = [
  { user_id: USER, uid_kind: "domain", uid_scope: "", sent: false, last_uid: 7 },
  { user_id: USER, uid_kind: "domain", uid_scope: "", sent: "false", last_uid: 7 },
  { user_id: USER, uid_kind: "domain", uid_scope: "", sent: false },
].map((data) => ({
  data: data as Json,
  result: outcome(() => new models.MailUidCounterModel(data).toJSON() as unknown as Json),
}));

const mappingModelCases = [
  { user_id: USER, mailbox: "INBOX", mail_id: MAIL, uid: 7 },
  { user_id: USER, mailbox: "INBOX", mail_id: MAIL, uid: "7" },
  { user_id: USER, mailbox: null, mail_id: MAIL, uid: 7 },
].map((data) => ({
  data: data as Json,
  result: outcome(() => new models.MailMailboxUidModel(data).toJSON() as unknown as Json),
}));

const validateObjectCases = (
  [
    [{ name: "Alice", age: 30 }, []],
    [null, []],
    ["oops", []],
    [42, []],
    [false, []],
    [{ name: 42, age: "old" }, []],
    [{ name: 42, age: "wrong" }, ["age"]],
    [{}, []],
    [[], []],
    [{ name: null, age: [1, { a: "b" }] }, []],
  ] as [Json, string[]][]
).map(([input, skip]) => ({
  input,
  skip,
  errors: models.validateObject(
    input,
    {
      name: (value: unknown) => typeof value === "string",
      age: (value: unknown) => typeof value === "number",
    },
    skip as ("name" | "age")[]
  ),
}));

// --- RFC 822 size -----------------------------------------------------------

const ATTACHMENT_ON_DISK = "golden-attachment-on-disk";
const ATTACHMENT_BYTES = 1000;

/** The shape `saveMail` measures: octet counts, no loaded bodies. */
const sizeCases = [];
{
  const attachmentSets: Json[] = [
    null,
    [],
    [{ content: { data: "missing-file" }, contentType: "application/pdf", filename: "a.pdf", size: 3 }],
    [
      { content: { data: ATTACHMENT_ON_DISK }, contentType: "text/plain", filename: 'quo"te\\.txt', size: 1 },
      { content: { data: "" }, filename: "no-type" },
      { content: { data: "missing" }, contentType: "application" },
      { content: { data: "missing" }, contentType: "/pdf", filename: "" },
      { content: { data: "missing" }, contentType: "a/b/c", filename: "line\r\nbreak .txt" },
    ],
    { not: "a list" },
  ];
  const headerSets: Record<string, Json>[] = [
    {},
    {
      subject: "Sübject line\r\nInjected: x",
      from_text: "Sender <sender@example.com>",
      to_text: "a@x.com, b@x.com",
      cc_text: "c@x.com",
      bcc_text: "d@x.com",
      reply_to_text: "r@x.com",
    },
    { subject: "", from_text: "", to_text: "only-to@x.com" },
  ];
  for (const [text_octets, html_octets] of [
    [0, 0],
    [5, 0],
    [0, 7],
    [4, 9],
    [3000, 1],
  ]) {
    for (const attachments of attachmentSets) {
      for (const headers of headerSets) {
        sizeCases.push({
          message_id: "<size@example.com>",
          date: DATE,
          text_octets,
          html_octets,
          mail_id: MAIL,
          attachments,
          ...headers,
        });
      }
    }
  }
  sizeCases.push(
    { message_id: "", date: DATE, text_octets: 1, html_octets: 1, mail_id: MAIL, attachments: null },
    { message_id: "<m@x>", date: "not a date", text_octets: 0, html_octets: 0, mail_id: MAIL, attachments: null },
    { message_id: "<m@x>", date: "", text_octets: 0, html_octets: 0, mail_id: MAIL, attachments: null },
    { message_id: "<m@x>", date: "Tue, 02 Jan 2024 03:04:05 +0900", text_octets: 2, html_octets: 0, mail_id: MAIL, attachments: null },
    { message_id: "<m@x>", date: DATE, text_octets: 2, html_octets: 2, mail_id: "id with spaces/ünïcode", attachments: [] }
  );
}

const workDirectory = fs.mkdtempSync(path.join(fs.realpathSync(os.tmpdir()), "mail-golden-"));
process.chdir(workDirectory);
fs.mkdirSync("attachments");
fs.writeFileSync(path.join("attachments", ATTACHMENT_ON_DISK), Buffer.alloc(ATTACHMENT_BYTES, 0x61));

const sizeResults = sizeCases.map((input) => {
  const text = (key: string) => (input as Record<string, Json>)[key] as string | undefined;
  const address = (key: string) => (text(key) ? { value: [], text: text(key) as string } : undefined);
  return {
    input: input as Json,
    result: outcome(() =>
      sessionUtils.computeFullMessageSize(
        {
          messageId: input.message_id,
          date: input.date,
          subject: text("subject"),
          text_octets: input.text_octets,
          html_octets: input.html_octets,
          mail_id: input.mail_id,
          user_id: USER,
          attachments: (input.attachments ?? undefined) as never,
          from: address("from_text"),
          to: address("to_text"),
          cc: address("cc_text"),
          bcc: address("bcc_text"),
          replyTo: address("reply_to_text"),
        },
        input.mail_id
      )
    ),
  };
});

// --- localeCompare -----------------------------------------------------------

const NAMES = [
  "b", "a", "B", "A", "a b", "a-b", "ab", "a_b", "a.b", "Archive", "archive", "ARCHIVE", "Zebra", "apple",
  "10", "9", "2", "étoile", "etoile", "Etoile", "日本語", "INBOX/keep", "Work/Projects", "work", "_x", "-x",
  " x", "x", "É", "e", "ß", "ss", "", "a1", "a10", "a2", "Ω", "я", "😀", "~", "!", "résumé", "resume",
  "Resume", "RESUME", "Receipts 2024", "Receipts-2023", "receipts", "Ärger", "Zürich", "zebra", "한국어",
  "Projects/Alpha", "Projects/alpha", "Projects", "#tag", "@home", "[Gmail]/All Mail", "0", "00", "á", "á",
];

const builders = {
  generatedBy: "src/server-rs/tests/golden/storage/generate.ts",
  statements,
  setFlags: setFlagsCases,
  views: viewCases,
  destinations: destinationCases,
  mappingWrites: mappingCases,
  mappedPivots: pivotCases,
  models: {
    mailTypeCheckerKeys: Object.keys(models.MailModel.typeChecker),
    partialValidFields: [...models.PartialMailModel.validFields],
    mailColumns: models.mailColumns,
    mailboxColumns: models.mailboxColumns,
    mailUidCounterColumns: models.mailUidCounterColumns,
    mailMailboxUidColumns: models.mailMailboxUidColumns,
    constants: {
      MAILBOX_NAME_MAX_BYTES: models.MAILBOX_NAME_MAX_BYTES,
      MAILBOX_COUNT_MAX: models.MAILBOX_COUNT_MAX,
      INBOX_VIEW: views.INBOX_VIEW,
      DRAFTS_VIEW: views.DRAFTS_VIEW,
      JUNK_VIEW: views.JUNK_VIEW,
      STARRED_MAILBOX: pivots.STARRED_MAILBOX,
      TRASH_MAILBOX: pivots.TRASH_MAILBOX,
    },
    mail: mailModelCases,
    partialMail: partialMailModelCases,
    mailbox: mailboxModelCases,
    mailUidCounter: counterModelCases,
    mailMailboxUid: mappingModelCases,
    validateObject: validateObjectCases,
  },
  rfc822Size: {
    attachmentOnDisk: { id: ATTACHMENT_ON_DISK, bytes: ATTACHMENT_BYTES },
    cases: sizeResults,
  },
  localeCompare: { names: NAMES, sorted: [...NAMES].sort((a, b) => a.localeCompare(b)) },
};

// ---------------------------------------------------------------------------
// traces.json
// ---------------------------------------------------------------------------

const input = (value: Args) => value as never;

const calls: Record<string, (a: Args) => Promise<unknown>> = {
  // Table
  "table.query": (a) => tables[a.table as string].query(decode(a.filters) as never),
  "table.count": (a) => tables[a.table as string].count(decode(a.filters) as never),
  "table.queryOne": (a) =>
    a.columns === null || a.columns === undefined
      ? tables[a.table as string].queryOne(decode(a.filters) as never)
      : tables[a.table as string].queryOne(decode(a.filters) as never, a.columns as string[]),
  "table.insert": (a) => tables[a.table as string].insert(decode(a.data) as never, a.returning as string[]),
  "table.update": (a) =>
    tables[a.table as string].update(a.id as string, decode(a.data) as never, a.returning as string[]),
  "table.hardDelete": (a) => tables[a.table as string].hardDelete(a.id as string),
  "table.deleteWhere": (a) => tables[a.table as string].deleteWhere(decode(a.filters) as never),
  "table.updateWhere": (a) =>
    tables[a.table as string].updateWhere(
      decode(a.filters) as never,
      decode(a.data) as never,
      (a.returning as string[] | null) ?? undefined
    ),
  // counters
  getDomainUidNext: (a) => counters.getDomainUidNext(a.user_id as string, a.sent as boolean),
  getAccountUidNext: (a) =>
    counters.getAccountUidNext(a.user_id as string, a.account as string, a.sent as boolean),
  getMailboxUidNext: (a) => counters.getMailboxUidNext(a.user_id as string, a.mailbox as string),
  getUidNext: (a) => counters.getUidNext(a.user_id as string, a.scope as never),
  deleteMailboxUid: (a) =>
    counters.deleteMailboxUid(a.user_id as string, a.mailbox as string, a.mail_id as string),
  syncMailboxPivot: (a) =>
    counters.syncMailboxPivot(
      a.user_id as string,
      a.mailbox as string,
      a.mail_id as string,
      a.isPresent as boolean
    ),
  writeMailboxUid: (a) =>
    counters.writeMailboxUid(a.user_id as string, a.mailbox as string, a.mail_id as string, a.uid as number),
  getNextModseq: (a) => counters.getNextModseq(a.user_id as string),
  getHighestModseq: (a) =>
    counters.getHighestModseq(a.user_id as string, a.mailbox as string | null, a.sent as boolean),
  // core
  syncMappedPivotsForRow: (a) =>
    core.syncMappedPivotsForRow(
      a.user_id as string,
      a.mail_id as string,
      (a.saved ?? undefined) as boolean | undefined,
      (a.deleted ?? undefined) as boolean | undefined,
      (a.destMailbox ?? undefined) as string | undefined
    ),
  recordMappings: (a) =>
    core.recordMappings(
      a.user_id as string,
      a.mail_id as string,
      a.writes as never,
      (a.mappedDestination ?? undefined) as string | undefined
    ),
  saveMail: (a) => core.saveMail(input(a)),
  getMailByMessageId: async (a) =>
    (await core.getMailByMessageId(a.user_id as string, a.message_id as string))?.toJSON(),
  getMailById: async (a) => (await core.getMailById(a.user_id as string, a.mail_id as string))?.toJSON(),
  updateRfc822Size: (a) =>
    core.updateRfc822Size(a.user_id as string, a.mail_id as string, a.rfc822_size as number),
  markMailRead: (a) => core.markMailRead(a.user_id as string, a.mail_id as string),
  markMailSaved: (a) => core.markMailSaved(a.user_id as string, a.mail_id as string, a.saved as boolean),
  deleteMail: (a) => core.deleteMail(a.user_id as string, a.mail_id as string),
  markMailSpam: (a) => core.markMailSpam(a.user_id as string, a.mail_id as string, a.is_spam as boolean),
  // mailboxes
  getMailboxesByUser: async (a) =>
    (await mailboxes.getMailboxesByUser(a.user_id as string)).map((mailbox) => mailbox.toJSON()),
  getMailboxByName: async (a) =>
    (await mailboxes.getMailboxByName(a.user_id as string, a.name as string))?.toJSON(),
  getMailboxByAddress: async (a) =>
    (await mailboxes.getMailboxByAddress(a.user_id as string, a.address as string))?.toJSON(),
  createMailbox: async (a) => {
    const result = await mailboxes.createMailbox(input(a));
    return result.status === "created" ? { status: result.status, mailbox: result.mailbox.toJSON() } : result;
  },
  deleteMailboxByName: (a) => mailboxes.deleteMailboxByName(a.user_id as string, a.name as string),
  renameMailbox: (a) =>
    mailboxes.renameMailbox(a.user_id as string, a.oldName as string, a.newName as string),
  setMailboxSubscribed: (a) =>
    mailboxes.setMailboxSubscribed(a.user_id as string, a.name as string, a.subscribed as boolean),
};

const traces: Json[] = [];

const rows = (...items: Record<string, Json>[]): Response => ({ rows: items });
const none = (): Response => ({ rows: [] });
const affected = (rowCount: number): Response => ({ rows: [], rowCount });
const fault = (code: string | undefined, message: string): Response => ({ error: { code, message } });
const DUPLICATE = fault("23505", "duplicate key value violates unique constraint");
const TERMINATED = fault("57P01", "terminating connection due to administrator command");

const run = async (name: string, fn: string, args: Args, scripted: Response[]) => {
  responses = structuredClone(scripted);
  trace = [];
  let result: Json;
  try {
    result = { ok: normalize(await calls[fn](args)) as Json };
  } catch (error) {
    // A TypeError is the JavaScript runtime refusing a value; only the fact of
    // the failure carries over to another language, not its wording.
    const thrown = error as { message?: string; code?: string };
    result = {
      threw: {
        message: thrown.message ?? String(error),
        code: thrown.code ?? null,
        runtime: error instanceof TypeError,
      },
    };
  }
  if (responses.length > 0) throw new Error(`${name}: ${responses.length} scripted responses were not consumed`);
  traces.push({
    name,
    fn,
    args,
    responses: scripted as unknown as Json,
    trace: trace.map(({ sql, values }) => ({ sql, values: values.map(normalize) })),
    result: JSON.parse(JSON.stringify(result)) as Json,
  });
};

// --- Table -------------------------------------------------------------------

await run("table.query mails by owner and message id", "table.query", { table: "mails", filters: { user_id: USER, message_id: "<m@x>" } }, [rows(mailRow())]);
await run("table.query with a NULL filter", "table.query", { table: "mailboxes", filters: { user_id: USER, parent_id: null } }, [none()]);
await run("table.query with no filter", "table.query", { table: "mail_uid_counters", filters: {} }, [none()]);
await run("table.count", "table.count", { table: "mailboxes", filters: { user_id: USER } }, [rows({ count: 12 })]);
await run("table.count with no row", "table.count", { table: "mailboxes", filters: { user_id: USER } }, [none()]);
await run("table.queryOne whole row", "table.queryOne", { table: "mails", filters: { mail_id: MAIL, user_id: USER }, columns: null }, [rows(mailRow())]);
await run("table.queryOne projected", "table.queryOne", { table: "mails", filters: { mail_id: MAIL, user_id: USER }, columns: ["read", "saved"] }, [rows({ read: true, saved: false })]);
await run("table.queryOne unknown column", "table.queryOne", { table: "mails", filters: { mail_id: MAIL }, columns: ["read", "nope", "also_nope"] }, []);
await run("table.queryOne by a contained document", "table.queryOne", { table: "mails", filters: { user_id: USER, attachments: [{ content: { data: "attachment-id" } }] }, columns: null }, [rows(mailRow({ attachments: [{ content: { data: "attachment-id" }, filename: "a.pdf" }] }))]);
await run("table.query by a contained object and a NULL", "table.query", { table: "mails", filters: { insight: { tags: ["work"] }, in_reply_to: null, user_id: USER } }, [none()]);
await run("table.queryOne miss", "table.queryOne", { table: "mailboxes", filters: { user_id: USER, address: "a@x" }, columns: null }, [none()]);
await run("table.insert stamps updated on mails", "table.insert", { table: "mails", data: { mail_id: MAIL, user_id: USER, read: false, uid_domain: 3, from_text: null }, returning: ["mail_id"] }, [rows({ mail_id: MAIL })]);
await run("table.insert leaves mailboxes unstamped", "table.insert", { table: "mailboxes", data: { user_id: USER, name: "Archive", subscribed: true, uid_validity: 1 }, returning: ["*"] }, [rows(mailboxRow())]);
await run("table.insert without a returned row", "table.insert", { table: "mail_mailbox_uid", data: { user_id: USER, mailbox: "INBOX", mail_id: MAIL, uid: 4 }, returning: ["uid"] }, [none()]);
await run("table.update by primary key", "table.update", { table: "mailboxes", id: MAILBOX, data: { name: "New", uid_validity: 2 }, returning: ["mailbox_id"] }, [rows({ mailbox_id: MAILBOX })]);
await run("table.update stamps updated on mails", "table.update", { table: "mails", id: MAIL, data: { subject: "s", raw: "skipped" }, returning: ["mail_id"] }, [rows({ mail_id: MAIL })]);
await run("table.update with nothing to set", "table.update", { table: "mailboxes", id: MAILBOX, data: {}, returning: ["mailbox_id"] }, []);
await run("table.hardDelete hit", "table.hardDelete", { table: "mailboxes", id: MAILBOX }, [affected(1)]);
await run("table.hardDelete miss", "table.hardDelete", { table: "mailboxes", id: MAILBOX }, [affected(0)]);
await run("table.deleteWhere", "table.deleteWhere", { table: "mails", filters: { mail_id: MAIL, user_id: USER } }, [affected(1)]);
await run("table.deleteWhere with a comparison", "table.deleteWhere", { table: "mails", filters: { user_id: USER, uid_domain: { op: "<=", value: 9, notNull: true } } }, [affected(3)]);
await run("table.deleteWhere refuses an undefined id", "table.deleteWhere", { table: "mails", filters: { mail_id: UNDEFINED, user_id: USER } }, []);
await run("table.deleteWhere refuses an empty bag", "table.deleteWhere", { table: "mails", filters: {} }, []);
await run("table.updateWhere with DB_NOW and RETURNING", "table.updateWhere", { table: "mails", filters: { mail_id: MAIL, user_id: USER, read: false }, data: { read: true, updated: NOW, modseq: 12 }, returning: ["mail_id"] }, [rows({ mail_id: MAIL })]);
await run("table.updateWhere without RETURNING", "table.updateWhere", { table: "mails", filters: { mail_id: MAIL, user_id: USER }, data: { rfc822_size: 4096 }, returning: null }, [affected(1)]);
await run("table.updateWhere with an IN list", "table.updateWhere", { table: "mails", filters: { mail_id: { op: "IN", value: [MAIL, OTHER_MAIL] } }, data: { expunged: true, updated: NOW, modseq: 31 }, returning: ["mail_id"] }, [rows({ mail_id: MAIL }, { mail_id: OTHER_MAIL })]);
await run("table.updateWhere with IN, a range and a flag", "table.updateWhere", { table: "mails", filters: { user_id: USER, sent: false, uid_domain: { op: "IN", value: [3, 4, 9], notNull: true }, modseq: { op: ">", value: 5 }, expunged: false }, data: { expunged: true, updated: NOW, modseq: 40 }, returning: ["uid_domain as uid"] }, [rows({ uid: 3 })]);
await run("table.updateWhere with nullary conditions", "table.updateWhere", { table: "mails", filters: { user_id: USER, in_reply_to: { op: "IS NULL" }, is_spam: { op: "IS NOT TRUE" }, insight: null }, data: { answered: true }, returning: ["mail_id"] }, [none()]);
await run("table.updateWhere refuses an empty IN list", "table.updateWhere", { table: "mails", filters: { mail_id: { op: "IN", value: [] } }, data: { expunged: true }, returning: null }, []);
await run("table.updateWhere refuses an undefined id", "table.updateWhere", { table: "mails", filters: { mail_id: UNDEFINED, message_id: UNDEFINED, user_id: USER }, data: { read: true }, returning: null }, []);
await run("table.updateWhere refuses a condition with no value", "table.updateWhere", { table: "mails", filters: { mail_id: { op: "=", value: UNDEFINED }, user_id: USER }, data: { read: true }, returning: null }, []);
await run("table.updateWhere refuses an empty bag", "table.updateWhere", { table: "mails", filters: {}, data: { read: true }, returning: null }, []);
await run("table.updateWhere with nothing to set", "table.updateWhere", { table: "mails", filters: { mail_id: MAIL }, data: {}, returning: ["mail_id"] }, []);
await run("table.updateWhere keeps falsy predicates", "table.updateWhere", { table: "mails", filters: { expunged: false, uid_domain: 0, subject: "", insight: null }, data: { read: false }, returning: null }, [affected(0)]);

// --- counters ----------------------------------------------------------------

for (const sent of [false, true]) {
  await run(`getDomainUidNext sent=${sent}`, "getDomainUidNext", { user_id: USER, sent }, [rows({ next_uid: 17 })]);
}
await run("getDomainUidNext with no row reads 1", "getDomainUidNext", { user_id: USER, sent: false }, [none()]);
await run("getDomainUidNext with a zero reads 1", "getDomainUidNext", { user_id: USER, sent: false }, [rows({ next_uid: 0 })]);
await run("getDomainUidNext fault propagates", "getDomainUidNext", { user_id: USER, sent: false }, [TERMINATED]);
await run("getAccountUidNext received", "getAccountUidNext", { user_id: USER, account: "admin@example.com", sent: false }, [rows({ next_uid: 4 })]);
await run("getAccountUidNext sent", "getAccountUidNext", { user_id: USER, account: "admin@example.com", sent: true }, [rows({ next_uid: 9 })]);
await run("getAccountUidNext empty account", "getAccountUidNext", { user_id: USER, account: "", sent: false }, [rows({ next_uid: 1 })]);
await run("getAccountUidNext fault propagates", "getAccountUidNext", { user_id: USER, account: "a@x", sent: false }, [TERMINATED]);
await run("getMailboxUidNext", "getMailboxUidNext", { user_id: USER, mailbox: "Starred" }, [rows({ next_uid: 3 })]);
await run("getMailboxUidNext fault propagates", "getMailboxUidNext", { user_id: USER, mailbox: "Trash" }, [TERMINATED]);
await run("getUidNext domain", "getUidNext", { user_id: USER, scope: { kind: "domain", sent: true } }, [rows({ uid_next: 12 })]);
await run("getUidNext account", "getUidNext", { user_id: USER, scope: { kind: "account", account: "admin@example.com", sent: false } }, [rows({ uid_next: 5 })]);
await run("getUidNext mailbox", "getUidNext", { user_id: USER, scope: { kind: "mailbox", mailbox: "Trash" } }, [rows({ uid_next: 2 })]);
await run("getUidNext with no row reads 1", "getUidNext", { user_id: USER, scope: { kind: "domain", sent: false } }, [none()]);
await run("getUidNext fault propagates", "getUidNext", { user_id: USER, scope: { kind: "domain", sent: false } }, [TERMINATED]);
await run("deleteMailboxUid", "deleteMailboxUid", { user_id: USER, mailbox: "Starred", mail_id: MAIL }, [affected(1)]);
await run("deleteMailboxUid fault propagates", "deleteMailboxUid", { user_id: USER, mailbox: "Starred", mail_id: MAIL }, [TERMINATED]);
await run("syncMailboxPivot present", "syncMailboxPivot", { user_id: USER, mailbox: "Starred", mail_id: MAIL, isPresent: true }, [rows({ next_uid: 6 }), rows({ uid: 6 })]);
await run("syncMailboxPivot present keeps an existing uid", "syncMailboxPivot", { user_id: USER, mailbox: "Trash", mail_id: MAIL, isPresent: true }, [rows({ next_uid: 6 }), rows({ uid: 2 })]);
await run("syncMailboxPivot absent", "syncMailboxPivot", { user_id: USER, mailbox: "Starred", mail_id: MAIL, isPresent: false }, [affected(0)]);
await run("syncMailboxPivot reservation fault stops before the write", "syncMailboxPivot", { user_id: USER, mailbox: "Starred", mail_id: MAIL, isPresent: true }, [TERMINATED]);
await run("syncMailboxPivot write fault propagates", "syncMailboxPivot", { user_id: USER, mailbox: "Starred", mail_id: MAIL, isPresent: true }, [rows({ next_uid: 6 }), DUPLICATE]);
await run("writeMailboxUid returns the persisted uid", "writeMailboxUid", { user_id: USER, mailbox: "Archive", mail_id: MAIL, uid: 7 }, [rows({ uid: 3 })]);
await run("writeMailboxUid with no row returns the proposed uid", "writeMailboxUid", { user_id: USER, mailbox: "Archive", mail_id: MAIL, uid: 7 }, [none()]);
await run("writeMailboxUid fault propagates", "writeMailboxUid", { user_id: USER, mailbox: "Archive", mail_id: MAIL, uid: 7 }, [DUPLICATE]);
await run("getNextModseq", "getNextModseq", { user_id: USER }, [rows({ next_uid: 88 })]);
await run("getNextModseq fault propagates", "getNextModseq", { user_id: USER }, [TERMINATED]);
for (const mailbox of [null, "Junk", "drafts", "INBOX/accounts/admin", "Archive"]) {
  await run(`getHighestModseq ${mailbox}`, "getHighestModseq", { user_id: USER, mailbox, sent: false }, [rows({ highest: 23 })]);
}
await run("getHighestModseq sent lane", "getHighestModseq", { user_id: USER, mailbox: "Sent Messages/accounts/admin", sent: true }, [rows({ highest: 4 })]);
await run("getHighestModseq with no row reads 1", "getHighestModseq", { user_id: USER, mailbox: null, sent: false }, [none()]);
await run("getHighestModseq keeps a zero", "getHighestModseq", { user_id: USER, mailbox: null, sent: false }, [rows({ highest: 0 })]);
await run("getHighestModseq absorbs a fault as 1", "getHighestModseq", { user_id: USER, mailbox: "Archive", sent: false }, [TERMINATED]);

// --- core: pivots and mappings ---------------------------------------------------

await run("syncMappedPivotsForRow both flags", "syncMappedPivotsForRow", { user_id: USER, mail_id: MAIL, saved: true, deleted: false, destMailbox: "Archive" }, [rows({ next_uid: 2 }), rows({ uid: 2 }), affected(1)]);
await run("syncMappedPivotsForRow skips the destination's own pivot", "syncMappedPivotsForRow", { user_id: USER, mail_id: MAIL, saved: true, deleted: true, destMailbox: "Starred" }, [rows({ next_uid: 5 }), rows({ uid: 5 })]);
await run("syncMappedPivotsForRow nothing to do", "syncMappedPivotsForRow", { user_id: USER, mail_id: MAIL, saved: null, deleted: null, destMailbox: null }, []);
await run("syncMappedPivotsForRow stops at the first fault", "syncMappedPivotsForRow", { user_id: USER, mail_id: MAIL, saved: false, deleted: true, destMailbox: null }, [TERMINATED]);

const BOTH_WRITES = [
  { mailbox: "INBOX/accounts/admin", uid: 7, scope: "mailbox" },
  { mailbox: "INBOX", uid: 42, scope: "domain" },
];
await run("recordMappings writes both rows and reports the mapped one", "recordMappings", { user_id: USER, mail_id: MAIL, writes: BOTH_WRITES, mappedDestination: "INBOX/accounts/admin" }, [rows({ uid: 7 }), rows({ uid: 42 })]);
await run("recordMappings reports the uid the mapping holds", "recordMappings", { user_id: USER, mail_id: MAIL, writes: [BOTH_WRITES[0]], mappedDestination: "INBOX/accounts/admin" }, [rows({ uid: 3 })]);
await run("recordMappings drops a domain row whose uid is taken", "recordMappings", { user_id: USER, mail_id: MAIL, writes: BOTH_WRITES, mappedDestination: "INBOX/accounts/admin" }, [rows({ uid: 7 }), DUPLICATE]);
await run("recordMappings aborts when the mapped uid is taken", "recordMappings", { user_id: USER, mail_id: MAIL, writes: BOTH_WRITES, mappedDestination: "INBOX/accounts/admin" }, [DUPLICATE, rows({ uid: 42 })]);
await run("recordMappings aborts on any other domain fault", "recordMappings", { user_id: USER, mail_id: MAIL, writes: BOTH_WRITES, mappedDestination: "INBOX/accounts/admin" }, [rows({ uid: 7 }), TERMINATED]);
await run("recordMappings domain row alone reports nothing", "recordMappings", { user_id: USER, mail_id: MAIL, writes: [BOTH_WRITES[1]], mappedDestination: null }, [rows({ uid: 42 })]);
await run("recordMappings with no writes", "recordMappings", { user_id: USER, mail_id: MAIL, writes: [], mappedDestination: "Archive" }, []);

// --- core: saveMail ---------------------------------------------------------------

const MODSEQ = rows({ next_uid: 31 });
const INSERTED = rows({ mail_id: MAIL });

const fullInput: Args = {
  user_id: USER,
  message_id: "<full@example.com>",
  in_reply_to: "<parent@example.com>",
  reference_ids: ["<root@example.com>", "<parent@example.com>"],
  subject: "A subject — with ünïcode",
  date: DATE,
  html: "<p>Hello <b>world</b></p>",
  text: "Hello world",
  from_address: [{ address: "sender@example.com", name: "Sender" }],
  from_text: "Sender <sender@example.com>",
  to_address: [{ address: "admin@example.com", name: "" }],
  to_text: "admin@example.com",
  cc_address: [{ address: "cc@example.com" }],
  cc_text: "cc@example.com",
  bcc_address: [{ address: "bcc@example.com" }],
  bcc_text: "bcc@example.com",
  reply_to_address: [{ address: "reply@example.com" }],
  reply_to_text: "reply@example.com",
  envelope_from: [{ address: "bounce@example.com" }],
  envelope_to: [{ address: "admin@example.com" }],
  attachments: [{ content: { data: "missing-file" }, contentType: "application/pdf", filename: "a.pdf", size: 3 }],
  read: true,
  saved: false,
  sent: false,
  deleted: false,
  draft: false,
  answered: true,
  expunged: false,
  insight: { summary: "s", tags: ["a", "b"] },
  uid_domain: 12,
  uid_mailbox: 5,
  spam_score: 3,
  spam_reasons: ["RULE_A", "RULE_B"],
  is_spam: false,
  mailbox: "INBOX/accounts/admin",
  domain_mailbox: "INBOX",
};

await run("saveMail inserts a received mail with every field", "saveMail", fullInput, [MODSEQ, INSERTED, rows({ uid: 5 }), rows({ uid: 12 })]);
await run("saveMail inserts the smallest mail", "saveMail", { user_id: USER, message_id: "<min@example.com>" }, [MODSEQ, INSERTED]);
await run("saveMail stores empty documents and falsy values as written", "saveMail", { user_id: USER, message_id: "<empty@example.com>", date: DATE, subject: "", html: "", text: "", from_address: [], from_text: "", envelope_to: [], attachments: [], reference_ids: [], spam_reasons: [], in_reply_to: "", insight: {}, uid_domain: 0, uid_mailbox: 0, spam_score: 0, mailbox: "", domain_mailbox: "" }, [MODSEQ, INSERTED]);
await run("saveMail stores explicit nulls as NULL", "saveMail", { user_id: USER, message_id: "<nulls@example.com>", date: DATE, in_reply_to: null, reference_ids: null, from_address: null, from_text: null, to_address: null, envelope_to: null, attachments: null, insight: null, spam_reasons: null }, [MODSEQ, INSERTED]);
await run("saveMail inserts a sent mail and files only its account box", "saveMail", { ...fullInput, message_id: "<sent@example.com>", sent: true, read: true, mailbox: "Sent Messages/accounts/admin", uid_domain: 3, uid_mailbox: 2 }, [MODSEQ, INSERTED, rows({ uid: 2 })]);
await run("saveMail into INBOX records the domain view alone", "saveMail", { user_id: USER, message_id: "<append@example.com>", date: DATE, text: "x", uid_domain: 8, uid_mailbox: 4, domain_mailbox: "INBOX" }, [MODSEQ, INSERTED, rows({ uid: 8 })]);
await run("saveMail returns the uid the mapping already held", "saveMail", { user_id: USER, message_id: "<copy@example.com>", date: DATE, uid_domain: 8, uid_mailbox: 9, mailbox: "Archive" }, [MODSEQ, INSERTED, rows({ uid: 4 })]);
await run("saveMail syncs the Starred and Trash pivots of a flagged row", "saveMail", { user_id: USER, message_id: "<flags@example.com>", date: DATE, saved: true, deleted: true, uid_domain: 2, uid_mailbox: 6, mailbox: "Archive" }, [MODSEQ, INSERTED, rows({ uid: 6 }), rows({ next_uid: 1 }), rows({ uid: 1 }), rows({ next_uid: 4 }), rows({ uid: 4 })]);
await run("saveMail placement overrides the flag it names", "saveMail", { user_id: USER, message_id: "<draft@example.com>", date: DATE, draft: false, uid_domain: 2, domain_mailbox: "INBOX", placement: { draft: true } }, [MODSEQ, INSERTED, rows({ uid: 2 })]);
await run("saveMail into Junk", "saveMail", { user_id: USER, message_id: "<junk@example.com>", date: DATE, uid_domain: 2, domain_mailbox: "INBOX", placement: { is_spam: true } }, [MODSEQ, INSERTED, rows({ uid: 2 })]);
await run("saveMail into Starred skips the pivot its own mapping wrote", "saveMail", { user_id: USER, message_id: "<star@example.com>", date: DATE, uid_domain: 2, uid_mailbox: 3, mailbox: "Starred", placement: { saved: true } }, [MODSEQ, INSERTED, rows({ uid: 3 })]);
await run("saveMail into Trash of a starred mail still syncs Starred", "saveMail", { user_id: USER, message_id: "<trash@example.com>", date: DATE, saved: true, uid_domain: 2, uid_mailbox: 3, mailbox: "Trash", placement: { deleted: true } }, [MODSEQ, INSERTED, rows({ uid: 3 }), rows({ next_uid: 8 }), rows({ uid: 8 })]);
await run("saveMail placement can clear a flag the mail carried", "saveMail", { user_id: USER, message_id: "<unstar@example.com>", date: DATE, saved: true, deleted: true, placement: { saved: false, deleted: false, draft: false, is_spam: false } }, [MODSEQ, INSERTED]);
await run("saveMail with an empty placement", "saveMail", { user_id: USER, message_id: "<noplace@example.com>", date: DATE, placement: {} }, [MODSEQ, INSERTED]);
await run("saveMail sizes text, html and an attachment on disk", "saveMail", { user_id: USER, message_id: "<sized@example.com>", date: DATE, subject: "Sized", text: "plain ünï", html: "<p>html 😀</p>", from_text: "a@x", to_text: "b@x", cc_text: "c@x", bcc_text: "d@x", reply_to_text: "e@x", attachments: [{ content: { data: ATTACHMENT_ON_DISK }, contentType: "text/plain", filename: "on-disk.txt", size: 1 }, { content: { data: "" }, filename: "" }] }, [MODSEQ, INSERTED]);
await run("saveMail returns nothing when the INSERT returns no row", "saveMail", { user_id: USER, message_id: "<norow@example.com>", date: DATE, uid_domain: 2, uid_mailbox: 3, mailbox: "Archive", domain_mailbox: "INBOX" }, [MODSEQ, none()]);
await run("saveMail stops when the mod-sequence cannot be reserved", "saveMail", { user_id: USER, message_id: "<nomodseq@example.com>", date: DATE }, [TERMINATED]);
await run("saveMail propagates an INSERT fault", "saveMail", { user_id: USER, message_id: "<fault@example.com>", date: DATE }, [MODSEQ, TERMINATED]);
await run("saveMail refuses attachments it cannot measure", "saveMail", { user_id: USER, message_id: "<badatt@example.com>", date: DATE, attachments: [{ filename: "no-content" }] }, [MODSEQ]);
await run("saveMail drops a domain view row whose uid is taken", "saveMail", { user_id: USER, message_id: "<legacy@example.com>", date: DATE, uid_domain: 7, uid_mailbox: 2, mailbox: "INBOX/accounts/admin", domain_mailbox: "INBOX" }, [MODSEQ, INSERTED, rows({ uid: 2 }), DUPLICATE]);
await run("saveMail propagates a mapping fault that is not a duplicate", "saveMail", { user_id: USER, message_id: "<mapfault@example.com>", date: DATE, uid_domain: 7, uid_mailbox: 2, mailbox: "Archive" }, [MODSEQ, INSERTED, TERMINATED]);
await run("saveMail propagates a pivot fault that is not a duplicate", "saveMail", { user_id: USER, message_id: "<pivotfault@example.com>", date: DATE, saved: true }, [MODSEQ, INSERTED, TERMINATED]);

const existing = mailRow({ mail_id: OTHER_MAIL, message_id: "<dup@example.com>", uid_domain: 5, sent: false, envelope_to: [{ address: "first@example.com", name: "First" }] });
const dupInput: Args = { user_id: USER, message_id: "<dup@example.com>", date: DATE, uid_domain: 9, uid_mailbox: 4, mailbox: "INBOX/accounts/second", domain_mailbox: "INBOX", envelope_to: [{ address: "first@example.com" }, { address: "second@example.com" }, { address: "second@example.com" }, { name: "no address" }] };

await run("saveMail merges a second delivery of the same Message-ID", "saveMail", dupInput, [MODSEQ, DUPLICATE, rows(existing), affected(1), rows({ uid: 4 }), rows({ uid: 5 })]);
await run("saveMail merge with no stored envelope", "saveMail", dupInput, [MODSEQ, DUPLICATE, rows({ ...existing, envelope_to: null }), affected(1), rows({ uid: 4 }), rows({ uid: 5 })]);
await run("saveMail merge without an incoming envelope issues no envelope update", "saveMail", { ...dupInput, envelope_to: null }, [MODSEQ, DUPLICATE, rows(existing), rows({ uid: 4 }), rows({ uid: 5 })]);
await run("saveMail merge with an empty incoming envelope still rewrites it", "saveMail", { ...dupInput, envelope_to: [] }, [MODSEQ, DUPLICATE, rows(existing), affected(1), rows({ uid: 4 }), rows({ uid: 5 })]);
await run("saveMail merge returns nothing when the row cannot be read back", "saveMail", dupInput, [MODSEQ, DUPLICATE, none()]);
await run("saveMail merge into a sent survivor files no domain view", "saveMail", dupInput, [MODSEQ, DUPLICATE, rows({ ...existing, sent: true }), affected(1), rows({ uid: 4 })]);
await run("saveMail merge uses the surviving row's uid_domain", "saveMail", { ...dupInput, mailbox: null, uid_mailbox: null }, [MODSEQ, DUPLICATE, rows({ ...existing, uid_domain: 77 }), affected(1), rows({ uid: 77 })]);
await run("saveMail merge applies a placement and stamps a mod-sequence", "saveMail", { ...dupInput, envelope_to: null, mailbox: null, uid_mailbox: null, placement: { draft: true } }, [MODSEQ, DUPLICATE, rows(existing), rows({ next_uid: 32 }), affected(1), rows({ uid: 5 })]);
await run("saveMail merge placement into Starred skips the Starred pivot", "saveMail", { ...dupInput, envelope_to: null, mailbox: "Starred", domain_mailbox: null, placement: { saved: true } }, [MODSEQ, DUPLICATE, rows(existing), rows({ next_uid: 32 }), affected(1), rows({ uid: 4 })]);
await run("saveMail merge placement mirrors flag flips into both pivots", "saveMail", { ...dupInput, envelope_to: null, mailbox: "Archive", domain_mailbox: null, placement: { saved: true, deleted: false } }, [MODSEQ, DUPLICATE, rows(existing), rows({ next_uid: 32 }), affected(1), rows({ uid: 4 }), rows({ next_uid: 2 }), rows({ uid: 2 }), affected(1)]);
await run("saveMail merge with an empty placement still stamps a mod-sequence", "saveMail", { ...dupInput, envelope_to: null, mailbox: null, uid_mailbox: null, domain_mailbox: null, placement: {} }, [MODSEQ, DUPLICATE, rows(existing), rows({ next_uid: 32 }), affected(1)]);
await run("saveMail merge drops a domain view row whose uid is taken", "saveMail", dupInput, [MODSEQ, DUPLICATE, rows(existing), affected(1), rows({ uid: 4 }), DUPLICATE]);
await run("saveMail merge fails when the mapped uid is taken", "saveMail", dupInput, [MODSEQ, DUPLICATE, rows(existing), affected(1), DUPLICATE, rows({ uid: 5 })]);
await run("saveMail merge propagates a fault reading the row back", "saveMail", dupInput, [MODSEQ, DUPLICATE, TERMINATED]);
await run("saveMail merge propagates an invalid stored row", "saveMail", dupInput, [MODSEQ, DUPLICATE, rows({ ...existing, uid_domain: "5" })]);
await run("saveMail merge refuses a stored envelope that is not a list", "saveMail", dupInput, [MODSEQ, DUPLICATE, rows({ ...existing, envelope_to: { address: "x" } })]);
await run("saveMail takes the merge path when the mapped uid is taken after the INSERT", "saveMail", { user_id: USER, message_id: "<taken@example.com>", date: DATE, uid_domain: 9, uid_mailbox: 3, mailbox: "Archive", envelope_to: [{ address: "a@x" }] }, [MODSEQ, INSERTED, DUPLICATE, rows(mailRow({ message_id: "<taken@example.com>", uid_domain: 9, envelope_to: [{ address: "a@x" }] })), affected(1), DUPLICATE]);
await run("saveMail takes the merge path when a pivot uid is taken after the INSERT", "saveMail", { user_id: USER, message_id: "<pivot@example.com>", date: DATE, saved: true, uid_domain: 9 }, [MODSEQ, INSERTED, rows({ next_uid: 3 }), DUPLICATE, rows(mailRow({ message_id: "<pivot@example.com>", uid_domain: 9, saved: true }))]);

// --- core: reads and flags -----------------------------------------------------------

await run("getMailByMessageId hit", "getMailByMessageId", { user_id: USER, message_id: "<m@x>" }, [rows(mailRow({ in_reply_to: "<p@x>", reference_ids: ["<p@x>"] }), mailRow({ mail_id: OTHER_MAIL }))]);
await run("getMailByMessageId miss", "getMailByMessageId", { user_id: USER, message_id: "<m@x>" }, [none()]);
await run("getMailByMessageId validates every row", "getMailByMessageId", { user_id: USER, message_id: "<m@x>" }, [rows(mailRow(), mailRow({ read: "no" }))]);
await run("getMailByMessageId fault propagates", "getMailByMessageId", { user_id: USER, message_id: "<m@x>" }, [TERMINATED]);
await run("getMailById hit", "getMailById", { user_id: USER, mail_id: MAIL }, [rows(mailRow({ attachments: [{ filename: "a" }], rfc822_size: 99 }))]);
await run("getMailById miss", "getMailById", { user_id: USER, mail_id: MAIL }, [none()]);
await run("getMailById invalid row", "getMailById", { user_id: USER, mail_id: MAIL }, [rows(without(mailRow(), "subject"))]);
await run("getMailById fault propagates", "getMailById", { user_id: USER, mail_id: MAIL }, [TERMINATED]);
await run("updateRfc822Size", "updateRfc822Size", { user_id: USER, mail_id: MAIL, rfc822_size: 123456 }, [affected(1)]);
await run("updateRfc822Size fault propagates", "updateRfc822Size", { user_id: USER, mail_id: MAIL, rfc822_size: 1 }, [TERMINATED]);

await run("markMailRead missing mail", "markMailRead", { user_id: USER, mail_id: MAIL }, [none()]);
await run("markMailRead already read reserves nothing", "markMailRead", { user_id: USER, mail_id: MAIL }, [rows({ read: true })]);
await run("markMailRead flips the flag", "markMailRead", { user_id: USER, mail_id: MAIL }, [rows({ read: false }), rows({ next_uid: 14 }), rows({ mail_id: MAIL })]);
await run("markMailRead lost the race to another reader", "markMailRead", { user_id: USER, mail_id: MAIL }, [rows({ read: false }), rows({ next_uid: 14 }), none(), rows({ read: true })]);
await run("markMailRead lost the race to a delete", "markMailRead", { user_id: USER, mail_id: MAIL }, [rows({ read: false }), rows({ next_uid: 14 }), none(), none()]);
await run("markMailRead probe fault propagates", "markMailRead", { user_id: USER, mail_id: MAIL }, [TERMINATED]);
await run("markMailRead reservation fault propagates", "markMailRead", { user_id: USER, mail_id: MAIL }, [rows({ read: false }), TERMINATED]);
await run("markMailRead update fault propagates", "markMailRead", { user_id: USER, mail_id: MAIL }, [rows({ read: false }), rows({ next_uid: 14 }), TERMINATED]);

for (const saved of [true, false]) {
  const pivot = saved ? [rows({ next_uid: 3 }), rows({ uid: 3 })] : [affected(1)];
  await run(`markMailSaved(${saved}) missing mail`, "markMailSaved", { user_id: USER, mail_id: MAIL, saved }, [none()]);
  await run(`markMailSaved(${saved}) already holds the value`, "markMailSaved", { user_id: USER, mail_id: MAIL, saved }, [rows({ saved }), ...pivot]);
  await run(`markMailSaved(${saved}) flips the flag`, "markMailSaved", { user_id: USER, mail_id: MAIL, saved }, [rows({ saved: !saved }), rows({ next_uid: 21 }), rows({ mail_id: MAIL }), ...pivot]);
  await run(`markMailSaved(${saved}) lost the race to another writer`, "markMailSaved", { user_id: USER, mail_id: MAIL, saved }, [rows({ saved: !saved }), rows({ next_uid: 21 }), none(), rows({ saved }), ...pivot]);
  await run(`markMailSaved(${saved}) lost the race to a delete`, "markMailSaved", { user_id: USER, mail_id: MAIL, saved }, [rows({ saved: !saved }), rows({ next_uid: 21 }), none(), none()]);
}
await run("markMailSaved pivot fault propagates", "markMailSaved", { user_id: USER, mail_id: MAIL, saved: true }, [rows({ saved: true }), TERMINATED]);
await run("markMailSaved update fault propagates", "markMailSaved", { user_id: USER, mail_id: MAIL, saved: true }, [rows({ saved: false }), rows({ next_uid: 21 }), TERMINATED]);

await run("deleteMail hit", "deleteMail", { user_id: USER, mail_id: MAIL }, [affected(1)]);
await run("deleteMail miss", "deleteMail", { user_id: USER, mail_id: MAIL }, [affected(0)]);
await run("deleteMail fault propagates", "deleteMail", { user_id: USER, mail_id: MAIL }, [TERMINATED]);

await run("markMailSpam changed", "markMailSpam", { user_id: USER, mail_id: MAIL, is_spam: true }, [rows({ next_uid: 50 }), rows({ mail_id: MAIL })]);
await run("markMailSpam unchanged", "markMailSpam", { user_id: USER, mail_id: MAIL, is_spam: false }, [rows({ next_uid: 50 }), none(), rows({ "?column?": 1 })]);
await run("markMailSpam missing mail", "markMailSpam", { user_id: USER, mail_id: MAIL, is_spam: true }, [rows({ next_uid: 50 }), none(), none()]);
await run("markMailSpam reservation fault propagates", "markMailSpam", { user_id: USER, mail_id: MAIL, is_spam: true }, [TERMINATED]);
await run("markMailSpam update fault propagates", "markMailSpam", { user_id: USER, mail_id: MAIL, is_spam: true }, [rows({ next_uid: 50 }), TERMINATED]);
await run("markMailSpam probe fault propagates", "markMailSpam", { user_id: USER, mail_id: MAIL, is_spam: true }, [rows({ next_uid: 50 }), none(), TERMINATED]);

// --- mailboxes -----------------------------------------------------------------------

await run("getMailboxesByUser sorts by name", "getMailboxesByUser", { user_id: USER }, [rows(...["work", "Archive", "archive", "Zebra", "étoile", "10", "9", "_x", "日本語", "Etoile"].map((name, index) => mailboxRow({ name, uid_next: index })))]);
await run("getMailboxesByUser empty", "getMailboxesByUser", { user_id: USER }, [none()]);
await run("getMailboxesByUser invalid row", "getMailboxesByUser", { user_id: USER }, [rows(mailboxRow(), mailboxRow({ name: null }))]);
await run("getMailboxesByUser fault propagates", "getMailboxesByUser", { user_id: USER }, [TERMINATED]);
await run("getMailboxByName hit", "getMailboxByName", { user_id: USER, name: "archive" }, [rows(mailboxRow())]);
await run("getMailboxByName miss", "getMailboxByName", { user_id: USER, name: "archive" }, [none()]);
await run("getMailboxByAddress hit", "getMailboxByAddress", { user_id: USER, address: "alice@example.com" }, [rows(mailboxRow({ address: "alice@example.com", special_use: "\\Inbox" }))]);
await run("getMailboxByAddress miss", "getMailboxByAddress", { user_id: USER, address: "alice@example.com" }, [none()]);

await run("createMailbox created", "createMailbox", { user_id: USER, name: "Archive" }, [none(), rows({ count: 3 }), rows(mailboxRow())]);
await run("createMailbox with every field", "createMailbox", { user_id: USER, name: "Team", address: "team@example.com", parent_id: MAILBOX, special_use: "\\Archive", subscribed: false }, [none(), rows({ count: 0 }), rows(mailboxRow({ name: "Team", address: "team@example.com", parent_id: MAILBOX, special_use: "\\Archive", subscribed: false }))]);
await run("createMailbox subscribed true is the default", "createMailbox", { user_id: USER, name: "Archive", subscribed: true }, [none(), rows({ count: 999 }), rows(mailboxRow())]);
await run("createMailbox exists", "createMailbox", { user_id: USER, name: "Archive" }, [rows(mailboxRow())]);
await run("createMailbox at the limit", "createMailbox", { user_id: USER, name: "Archive" }, [none(), rows({ count: 1000 })]);
await run("createMailbox past the limit", "createMailbox", { user_id: USER, name: "Archive" }, [none(), rows({ count: 1001 })]);
await run("createMailbox INSERT returned no row", "createMailbox", { user_id: USER, name: "Archive" }, [none(), rows({ count: 0 }), none()]);
await run("createMailbox INSERT fault propagates", "createMailbox", { user_id: USER, name: "Archive" }, [none(), rows({ count: 0 }), DUPLICATE]);

await run("deleteMailboxByName deleted", "deleteMailboxByName", { user_id: USER, name: "Archive" }, [rows(mailboxRow()), affected(1)]);
await run("deleteMailboxByName not found", "deleteMailboxByName", { user_id: USER, name: "Archive" }, [none()]);
await run("deleteMailboxByName protected", "deleteMailboxByName", { user_id: USER, name: "INBOX" }, [rows(mailboxRow({ special_use: "\\Inbox" }))]);
await run("deleteMailboxByName deleted even when the row is already gone", "deleteMailboxByName", { user_id: USER, name: "Archive" }, [rows(mailboxRow()), affected(0)]);

await run("renameMailbox renamed", "renameMailbox", { user_id: USER, oldName: "Archive", newName: "Old" }, [rows(mailboxRow({ uid_validity: 4 })), none(), rows({ mailbox_id: MAILBOX })]);
await run("renameMailbox not found", "renameMailbox", { user_id: USER, oldName: "Archive", newName: "Old" }, [none()]);
await run("renameMailbox protected", "renameMailbox", { user_id: USER, oldName: "INBOX", newName: "Old" }, [rows(mailboxRow({ special_use: "\\Inbox" }))]);
await run("renameMailbox name taken", "renameMailbox", { user_id: USER, oldName: "Archive", newName: "Old" }, [rows(mailboxRow()), rows(mailboxRow({ name: "Old" }))]);

await run("setMailboxSubscribed updated", "setMailboxSubscribed", { user_id: USER, name: "Archive", subscribed: false }, [rows(mailboxRow()), rows({ mailbox_id: MAILBOX })]);
await run("setMailboxSubscribed not found", "setMailboxSubscribed", { user_id: USER, name: "Archive", subscribed: true }, [none()]);
await run("setMailboxSubscribed row vanished", "setMailboxSubscribed", { user_id: USER, name: "Archive", subscribed: true }, [rows(mailboxRow()), none()]);

// ---------------------------------------------------------------------------

const directory = import.meta.dir;
const write = (name: string, value: unknown) =>
  fs.writeFileSync(path.join(directory, name), `${JSON.stringify(value, null, 1)}\n`);

write("builders.json", builders);
write("traces.json", {
  generatedBy: "src/server-rs/tests/golden/storage/generate.ts",
  fixedIds: FIXED_IDS,
  fixedDate: DATE,
  attachmentOnDisk: { id: ATTACHMENT_ON_DISK, bytes: ATTACHMENT_BYTES },
  cases: traces,
});

fs.rmSync(workDirectory, { recursive: true, force: true });
process.stdout.write(
  `builders.json: ${statements.length} statements, ${setFlagsCases.length} STORE shapes, ${sizeResults.length} sizes\n` +
    `traces.json: ${traces.length} cases\n`
);
process.exit(0);
