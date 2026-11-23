/**
 * The differential database scenario for the IMAP data layer.
 *
 *   bun src/server-rs/tests/golden/imapdb/scenario.ts
 *
 * Drives the real TypeScript `imap/store.ts` (and through it
 * `repositories/mails/imap.ts`) through a scripted scenario against a fresh
 * PostgreSQL database, then writes two files beside this one:
 *
 * - `scenario.steps.json`: the scenario itself, as data.
 * - `scenario.expected.json`: every step's return value and the resulting
 *   `mails`, `mail_mailbox_uid`, `mail_uid_counters` and `mailboxes` tables.
 *
 * `tests/imap_store_differential.rs` runs the same steps through the Rust
 * store against another fresh database and requires the same record.
 *
 * Most steps are one store call. A few are the session steps around the
 * store, run with the session's own helpers: FETCH (the projection
 * `getRequestedFields` asks for, one range read per range), STORE (one call
 * per range, then the Starred / Trash pivots), COPY and MOVE
 * (`cloneMailToDestination`, then `expungeUids` for a MOVE) and the storage
 * step of APPEND. Mail is delivered as SMTP delivery files it, through the
 * storage core. Generated ids are replaced by labels.
 *
 * PostgreSQL is reached with the usual `POSTGRES_*` variables (default
 * `postgres:postgres@127.0.0.1:5432`). The database is created and dropped
 * here, named `mail_imapdb_ts_<pid>`.
 */
import crypto from "crypto";
import fs from "fs";
import os from "os";
import path from "path";
import { SQL } from "bun";
import type { StoreFetchedMail } from "../../../../server/lib/imap/store";

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
type Args = Record<string, Json>;
interface Step {
  op: string;
  args: Args;
}

// ---------------------------------------------------------------------------
// The scenario
// ---------------------------------------------------------------------------

const DOMAIN = "example.com";
const USERS: Record<string, { id: string; username: string; email: string }> = {
  admin: { id: "a0000000-0000-4000-8000-000000000001", username: "admin", email: `admin@${DOMAIN}` },
  alice: { id: "a0000000-0000-4000-8000-000000000002", username: "alice", email: `alice@alice.${DOMAIN}` },
  bob: { id: "a0000000-0000-4000-8000-000000000003", username: "bob", email: `bob@bob.${DOMAIN}` },
};

const steps: Step[] = [];
const step = (op: string, args: Args = {}) => {
  steps.push({ op, args });
};

const ADMIN = `admin@${DOMAIN}`;
const SALES = `sales@${DOMAIN}`;
const WORK = `work@alice.${DOMAIN}`;
const HOME = `home@alice.${DOMAIN}`;

let day = 0;
/** A distinct date per mail, a day apart, so no two sort together. */
const nextDate = () => {
  day += 1;
  return new Date(Date.UTC(2024, 0, 1, 8, 30, 0) + day * 86_400_000).toISOString();
};

const address = (list: Json, text: string): Json => ({ value: list, text });

/** A received mail, delivered to `account` as SMTP delivery does. */
const deliver = (user: string, account: string, messageId: string, extra: Args = {}) =>
  step("deliver", {
    user,
    sent: false,
    messageId,
    subject: `Subject of ${messageId}`,
    date: nextDate(),
    text: `Body of ${messageId}`,
    html: `<p>Body of ${messageId}</p>`,
    from: address([{ address: "sender@outside.test", name: "Sender" }], "Sender <sender@outside.test>"),
    to: address([{ address: account, name: "" }], account),
    envelopeFrom: [{ address: "bounce@outside.test" }],
    envelopeTo: [{ address: account }],
    ...extra,
  });

/** A sent mail, recorded from `account` as the send path does. */
const sendFrom = (user: string, account: string, messageId: string, extra: Args = {}) =>
  step("deliver", {
    user,
    sent: true,
    messageId,
    subject: `Sent ${messageId}`,
    date: nextDate(),
    text: `Sent body of ${messageId}`,
    html: `<p>Sent body of ${messageId}</p>`,
    from: address([{ address: account, name: "Me" }], `Me <${account}>`),
    to: address([{ address: "friend@outside.test", name: "Friend" }], "Friend <friend@outside.test>"),
    envelopeFrom: [{ address: account }],
    envelopeTo: [{ address: "friend@outside.test" }],
    read: true,
    ...extra,
  });

/** Every kind of mailbox name the session hands the store. */
const ADMIN_BOXES = [
  "INBOX",
  "Sent Messages",
  "Drafts",
  "Junk",
  "Starred",
  "Trash",
  "INBOX/accounts/admin",
  "INBOX/accounts/sales",
  "Sent Messages/accounts/admin",
  "INBOX/accounts",
  "Sent Messages/accounts",
  "Archive",
  "Projects/Wörk \"2024\"",
  "Nope",
];

const readEverything = (user: string, boxes: string[]) => {
  for (const box of boxes) {
    step("countMessages", { user, box });
    step("getUidNext", { user, box });
    step("getAllUids", { user, box });
    step("getFirstUnseenUid", { user, box });
    step("getHighestModseq", { user, box });
  }
};

const listing = (user: string) => {
  step("listMailboxes", { user });
  step("listMailboxEntries", { user });
};

// --- users ---------------------------------------------------------------------------

for (const { id, username, email } of Object.values(USERS)) {
  step("sql", { sql: "INSERT INTO users (user_id, username, email) VALUES ($1, $2, $3)", values: [id, username, email] });
}
// Rows the order of a statement leaves tied come back in their physical
// order; no vacuum may move them halfway through.
for (const table of ["mails", "mail_mailbox_uid", "mail_uid_counters", "mailboxes"]) {
  step("ddl", { sql: `ALTER TABLE ${table} SET (autovacuum_enabled = false)` });
}

// --- an empty account: every read answers for a mailbox with nothing in it ------------

listing("bob");
for (const box of ["INBOX", "inbox", "Drafts", "drafts", "Starred", "Sent Messages", "Archive", "INBOX/accounts", ""]) {
  step("mailboxExists", { user: "bob", box });
}
readEverything("bob", ["INBOX", "Sent Messages", "Drafts", "Junk", "Starred", "Trash", "INBOX/accounts/bob", "Archive"]);
step("search", { user: "bob", box: "INBOX", line: "a UID SEARCH ALL" });
step("fetch", { user: "bob", box: "INBOX", line: "a UID FETCH 1:* (UID FLAGS)" });
step("fetch", { user: "bob", box: "INBOX", line: "a FETCH 1:* (FLAGS)" });
step("expunge", { user: "bob", box: "INBOX" });
step("expunge", { user: "bob", box: "Archive" });
step("expungeUids", { user: "bob", box: "INBOX", uids: [1, 2] });
step("expungeUids", { user: "bob", box: "Archive", uids: [] });
step("setFlags", { user: "bob", box: "INBOX", start: 1, end: 5, flags: ["\\Seen"], useUid: true, operation: "+FLAGS" });
step("getMailFlagUpdates", { user: "bob", box: "INBOX", messageIds: [] });

// --- deliveries ----------------------------------------------------------------------

deliver("admin", ADMIN, "<a1@outside.test>", {
  subject: "Quarterly report — ünïcode ✓",
  text: "Hello world 😀 quarterly needle",
  html: "<p>Hello <b>world</b> 😀</p>",
  cc: address([{ address: "cc@outside.test", name: "Cc" }], "Cc <cc@outside.test>"),
  replyTo: address([{ address: "reply@outside.test", name: "Reply" }], "Reply <reply@outside.test>"),
  attachments: [{ content: { data: "missing-attachment" }, contentType: "application/pdf", filename: "report.pdf", size: 1234 }],
  insight: { summary: "A report" },
  inReplyTo: "<parent@outside.test>",
  references: ["<root@outside.test>", "<parent@outside.test>"],
});
deliver("admin", ADMIN, "<a2@outside.test>", { text: "Plain only, 100% _real_", html: "" });
deliver("admin", SALES, "<a3@outside.test>", {
  subject: "Sales lead",
  from: address([{ address: "lead@buyer.test", name: "Lead" }], "Lead <lead@buyer.test>"),
  bcc: address([{ address: SALES, name: "" }], SALES),
  read: true,
});
deliver("admin", SALES, "<a4@outside.test>", { subject: "Sales follow-up", saved: true });
deliver("admin", ADMIN, "<a5@outside.test>", {
  subject: "Buy now",
  isSpam: true,
  spamScore: 9,
  spamReasons: ["URIBL", "BAYES_99"],
});
deliver("admin", ADMIN, "<a6@outside.test>", { subject: "HTML only", text: "", html: "<p>only html</p>" });
deliver("admin", ADMIN, "<a7@outside.test>", { subject: "Reply all", read: true, deleted: false });
sendFrom("admin", ADMIN, "<s1@example.com>");
sendFrom("admin", ADMIN, "<s2@example.com>", { subject: "Second sent", read: false });
sendFrom("admin", SALES, "<s3@example.com>");

deliver("alice", WORK, "<w1@outside.test>", {
  cc: address(
    [
      { address: HOME, name: "" },
      { address: "other@outside.test", name: "Other" },
    ],
    `${HOME}, Other <other@outside.test>`
  ),
});
deliver("alice", HOME, "<h1@outside.test>", { read: true });
sendFrom("alice", WORK, "<ws1@alice.example.com>");

// A long body with characters of every width, for the chunked readers.
deliver("bob", `bob@bob.${DOMAIN}`, "<long@outside.test>", {
  subject: "Long",
  textRepeat: { unit: "a€😀b", count: 7_501 },
  html: "<p>short</p>",
});
deliver("bob", `bob@bob.${DOMAIN}`, "<short@outside.test>", { subject: "Short", text: "x😀y", html: "" });

// --- user mailboxes ------------------------------------------------------------------

for (const [user, name, extra] of [
  ["admin", "Archive", {}],
  ["admin", "Projects", { subscribed: false }],
  ["admin", "Projects/Wörk \"2024\"", {}],
  ["admin", "trash", {}],
  ["admin", "Sent Messages", {}],
  ["admin", " Padded ", {}],
  ["admin", "Role", { special_use: "\\Archive" }],
  ["admin", "Tied", { address: `tied@${DOMAIN}` }],
  ["alice", "Archive", {}],
  ["alice", "Later", { subscribed: false }],
] as [string, string, Args][]) {
  step("createMailbox", { user, name, ...extra });
}

// --- listings ------------------------------------------------------------------------

for (const user of ["admin", "alice"]) listing(user);
for (const box of [
  ...ADMIN_BOXES,
  "inbox",
  "Inbox",
  "drafts",
  "STARRED",
  "sent messages",
  "Sent Messages/accounts/sales",
  "INBOX/accounts/Admin",
  "Projects",
  " Padded ",
  "Padded",
  "Role",
  "Tied",
  "trash",
  "",
]) {
  step("mailboxExists", { user: "admin", box });
}
step("mailboxExists", { user: "alice", box: "INBOX/accounts/work" });
step("mailboxExists", { user: "alice", box: "INBOX/accounts/home" });
step("mailboxExists", { user: "alice", box: "INBOX/accounts/other" });
step("mailboxExists", { user: "alice", box: "INBOX/accounts/admin" });

readEverything("admin", ADMIN_BOXES);
readEverything("alice", ["INBOX", "INBOX/accounts/work", "INBOX/accounts/home", "Sent Messages", "Sent Messages/accounts/work", "Archive"]);

// --- FETCH ---------------------------------------------------------------------------

const PROJECTIONS = [
  "FLAGS",
  "(UID FLAGS)",
  "(FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)",
  "(UID RFC822.SIZE FLAGS BODY.PEEK[HEADER.FIELDS (From To Cc Bcc Subject Date Message-ID Priority X-Priority References Newsgroups In-Reply-To Content-Type Reply-To)])",
  "BODY.PEEK[]",
  "BODY.PEEK[]<0.2048>",
  "(BODY.PEEK[TEXT] BODYSTRUCTURE)",
  "BODY.PEEK[1]",
  "BODY.PEEK[HEADER]",
  "BODY.PEEK[HEADER.FIELDS.NOT (Received)]",
  "(RFC822.HEADER RFC822.SIZE)",
  "(UID MODSEQ)",
  "ALL",
  "FULL",
  "FAST",
  "ENVELOPE",
  "BODYSTRUCTURE",
  "INTERNALDATE",
];
for (const items of PROJECTIONS) {
  for (const box of ["INBOX", "INBOX/accounts/sales", "Sent Messages"]) {
    step("fetch", { user: "admin", box, line: `a UID FETCH 1:* ${items}` });
  }
}
const RANGES = ["*", "2", "2:4", "4:2", "1,3,5", "3:*", "99:*", "1:2,2:3", "*:1", "5:*"];
for (const range of RANGES) {
  for (const box of ["INBOX", "INBOX/accounts/admin", "Sent Messages/accounts/admin", "Junk"]) {
    step("fetch", { user: "admin", box, line: `a UID FETCH ${range} (UID FLAGS)` });
    step("fetch", { user: "admin", box, line: `a FETCH ${range} (FLAGS)` });
  }
}
for (const changedSince of [0, 1, 5, 12, 999]) {
  step("fetch", { user: "admin", box: "INBOX", line: `a UID FETCH 1:* (FLAGS) (CHANGEDSINCE ${changedSince})` });
  step("fetch", { user: "admin", box: "INBOX/accounts/sales", line: `a FETCH 1:* (UID) (CHANGEDSINCE ${changedSince})` });
}
// The range read itself, with argument shapes the session does not produce.
for (const [box, start, end, useUid, fields, changedSince] of [
  ["INBOX", 2, 3, false, ["uid", "read"], null],
  ["INBOX/accounts/admin", 1, 2, false, ["uid", "subject"], null],
  ["INBOX", 1, 9007199254740991, true, ["*"], null],
  ["INBOX/accounts/admin", 1, 9007199254740991, true, ["*"], 3],
  ["INBOX", 1, 9007199254740991, true, [], null],
  ["INBOX", 1, 9007199254740991, true, ["sent", "envelopeTo", "rfc822_size", "bogus"], null],
  ["Sent Messages", 0, 0, true, ["uid"], null],
] as [string, number, number, boolean, string[], number | null][]) {
  step("getMessages", { user: "admin", box, start, end, useUid, fields, changedSince });
}
step("concurrentFetch", { user: "admin", box: "INBOX", fields: ["uid", "subject", "text_octets"], count: 6 });
// Another user's store sees none of it.
step("fetch", { user: "alice", box: "INBOX", line: "a UID FETCH 1:* (UID FLAGS ENVELOPE)" });
step("fetch", { user: "alice", box: "INBOX/accounts/admin", line: "a UID FETCH 1:* (UID FLAGS)" });

// --- the chunked body readers --------------------------------------------------------

for (const [messageId, source, chunkChars] of [
  ["<long@outside.test>", "text", null],
  ["<long@outside.test>", "text", 7],
  ["<long@outside.test>", "html", null],
  ["<short@outside.test>", "text", 1],
  ["<short@outside.test>", "html", null],
] as [string, string, number | null][]) {
  step("textChunks", { user: "bob", messageId, source, chunkChars });
}
for (const [messageId, source, startByte, chunkBytes] of [
  ["<long@outside.test>", "text", null, null],
  ["<long@outside.test>", "text", 49_153, null],
  ["<long@outside.test>", "text", 5, 3],
  ["<short@outside.test>", "text", 2, 2],
  ["<short@outside.test>", "html", null, null],
] as [string, string, number | null, number | null][]) {
  step("byteChunks", { user: "bob", messageId, source, startByte, chunkBytes });
}
step("textChunks", { user: "alice", messageId: "<long@outside.test>", source: "text", chunkChars: null });

// --- SEARCH --------------------------------------------------------------------------

const SEARCH_KEYS = [
  "ALL", "ANSWERED", "DELETED", "FLAGGED", "NEW", "OLD", "RECENT", "SEEN", "UNANSWERED", "UNDELETED", "UNFLAGGED",
  "UNSEEN", "DRAFT", "UNDRAFT", "KEYWORD $Forwarded", "UNKEYWORD $Forwarded",
  "BEFORE 4-Jan-2024", "ON 4-Jan-2024", "SINCE 5-Jan-2024", "SENTBEFORE 6-Jan-2024", "SENTON 9-Jan-2024",
  "SENTSINCE 10-Jan-2024", "BEFORE 31-Feb-2024", "SINCE 1-Jan-275760", "ON 13-Sep-275760",
  "FROM lead", 'TO "sales@"', "CC cc@", "BCC sales", 'SUBJECT "100% _real_"', "SUBJECT ünïcode", "BODY needle",
  "BODY \"100%\"", "BODY _real_", "TEXT quarterly", "TEXT Sender",
  "HEADER Subject Sales", "HEADER FROM lead", "HEADER to admin", 'HEADER Message-ID "<a2@"', "HEADER X-Mailer z",
  "UID 1:3", "UID 1,3:5,9", "UID 5:*", "UID *", "UID 9:2", "UID 99999999999999999999999",
  "1:3", "*", "LARGER 100", "SMALLER 5",
  "NOT SEEN", "NOT FROM sender", "NOT UID 3", "NOT 2:3", "NOT ALL", "NOT KEYWORD k", "NOT NOT SEEN",
  "NOT OR SEEN FLAGGED", "NOT BEFORE 4-Jan-2024", "NOT HEADER X-Spam yes",
  "OR SEEN FLAGGED", "OR FROM lead TO admin", "OR ALL SEEN", "OR KEYWORD k SEEN", "OR KEYWORD a KEYWORD b",
  "OR SUBJECT Sales ALL", "OR UID 1:2 UID 5", "OR NOT SEEN NOT FLAGGED", "OR OR SEEN FLAGGED DELETED",
  "OR SEEN OR FLAGGED DELETED", "OR 1:2 SUBJECT Sales", "OR LARGER 5 SMALLER 9", "OR BEFORE 31-Feb-2024 ALL",
  "SEEN FLAGGED", "UNSEEN FROM sender SINCE 1-Jan-2024", "NOT DELETED SUBJECT Sales OR FROM lead FROM sender UID 1:100",
  "MODSEQ 5", "FOO", "CHARSET UTF-8 SUBJECT Sales",
];
for (const box of ["INBOX", "INBOX/accounts/sales", "Sent Messages", "Junk"]) {
  for (const keys of SEARCH_KEYS) step("search", { user: "admin", box, line: `a UID SEARCH ${keys}` });
}
for (const keys of ["ALL", "FROM sender", "TEXT Sent", "SEEN"]) {
  step("search", { user: "admin", box: "Sent Messages/accounts/admin", line: `a UID SEARCH ${keys}` });
  step("search", { user: "alice", box: "INBOX", line: `a UID SEARCH ${keys}` });
}

// --- APPEND --------------------------------------------------------------------------

const appended = (messageId: string, extra: Args = {}): Args => ({
  messageId,
  subject: `Appended ${messageId}`,
  date: nextDate(),
  text: `Appended body ${messageId}`,
  html: `<p>Appended body ${messageId}</p>`,
  from: address([{ address: ADMIN, name: "Admin" }], `Admin <${ADMIN}>`),
  to: address([{ address: "friend@outside.test", name: "" }], "friend@outside.test"),
  ...extra,
});
for (const [mailbox, flags, messageId] of [
  ["Drafts", [], "<d1@example.com>"],
  ["drafts", ["\\Draft", "\\Seen"], "<d2@example.com>"],
  ["INBOX", ["\\Seen", "\\Flagged"], "<p1@example.com>"],
  ["inbox", [], "<p2@example.com>"],
  ["Junk", [], "<p3@example.com>"],
  ["Starred", [], "<p4@example.com>"],
  ["Trash", ["\\Seen"], "<p5@example.com>"],
  ["Sent Messages", ["\\Seen"], "<p6@example.com>"],
  ["Sent Messages/accounts/admin", [], "<p7@example.com>"],
  ["INBOX/accounts/admin", ["\\Answered", "\\Deleted"], "<p8@example.com>"],
  ["INBOX/accounts", [], "<p9@example.com>"],
  ["Archive", ["\\Flagged", "\\Deleted", "\\Draft"], "<p10@example.com>"],
  ["Projects/Wörk \"2024\"", ["$Forwarded", "\\seen"], "<p11@example.com>"],
  ["Nope", [], "<p12@example.com>"],
  ["Archive", [], "<p1@example.com>"],
] as [string, string[], string][]) {
  const extra: Args =
    messageId === "<p8@example.com>"
      ? {
          inReplyTo: "<a1@outside.test>",
          references: ["<root@outside.test>", "<a1@outside.test>"],
          attachments: [{ content: { data: "appended-attachment" }, contentType: "text/plain", filename: "a.txt", size: 3 }],
          cc: address([{ address: "cc@outside.test", name: "" }], "cc@outside.test"),
        }
      : {};
  step("append", { user: "admin", mailbox, flags, mail: appended(messageId, extra) });
}
// The store's write, with destinations the session does not produce.
step("storeMail", { user: "admin", destination: null, mail: appended("<direct1@example.com>", { uid: { domain: 0, account: 0 } }) });
step("storeMail", { user: "admin", destination: "", mail: appended("<direct2@example.com>", { uid: { domain: 77, account: 0 } }) });
step("storeMail", { user: "admin", destination: "Archive", mail: appended("<direct3@example.com>", { uid: { domain: 501, account: 0 } }) });
step("storeMail", { user: "bob", destination: "INBOX", mail: appended("<bad-date@example.com>", { date: "not a date", uid: { domain: 5, account: 0 } }) });

readEverything("admin", ["INBOX", "Drafts", "Junk", "Starred", "Trash", "Sent Messages", "Archive", "INBOX/accounts/admin"]);
listing("admin");

// --- STORE ---------------------------------------------------------------------------

for (const [box, line] of [
  ["INBOX", "a UID STORE 1:* +FLAGS.SILENT (\\Seen)"],
  ["INBOX", "a STORE 1 -FLAGS (\\Seen)"],
  ["INBOX", "a STORE 2:3 +FLAGS (\\Flagged)"],
  ["INBOX", "a UID STORE 3 FLAGS (\\Flagged \\Deleted)"],
  ["INBOX", "a STORE 1 +FLAGS ()"],
  ["INBOX", "a STORE 1 +FLAGS ($Forwarded)"],
  ["INBOX", "a STORE 1 +FLAGS (\\seen)"],
  ["INBOX", "a STORE 2 FLAGS ()"],
  ["INBOX", "a STORE 99 +FLAGS (\\Seen)"],
  ["INBOX", "a UID STORE 99:* +FLAGS (\\Seen)"],
  ["INBOX/accounts/sales", "a STORE 1:* +FLAGS (\\Answered)"],
  ["INBOX/accounts/sales", "a UID STORE 1 -FLAGS (\\Flagged)"],
  ["INBOX/accounts/admin", "a STORE 1:2 +FLAGS (\\Draft)"],
  ["Drafts", "a STORE 1 -FLAGS (\\Draft)"],
  ["Sent Messages", "a STORE 1:* +FLAGS (\\Flagged)"],
  ["Starred", "a STORE 1 -FLAGS (\\Flagged)"],
  ["Junk", "a STORE 1 +FLAGS (\\Seen)"],
  ["Archive", "a STORE 1:* +FLAGS (\\Seen \\Answered)"],
  // CONDSTORE: UNCHANGEDSINCE below, at and above the rows' mod-sequences.
  ["INBOX", "a STORE 1:* (UNCHANGEDSINCE 0) +FLAGS (\\Answered)"],
  ["INBOX", "a STORE 1:* (UNCHANGEDSINCE 3) +FLAGS (\\Answered)"],
  ["INBOX", "a UID STORE 1:* (UNCHANGEDSINCE 999) -FLAGS (\\Answered)"],
  ["INBOX", "a STORE 1,1:2 (UNCHANGEDSINCE 20) +FLAGS (\\Seen)"],
  ["INBOX", "a STORE 1:* (UNCHANGEDSINCE 5) +FLAGS ()"],
  ["INBOX", "a STORE 1:* (UNCHANGEDSINCE 999) +FLAGS ($Junk)"],
  ["INBOX/accounts/sales", "a UID STORE 1:* (UNCHANGEDSINCE 1) FLAGS (\\Seen)"],
  ["INBOX/accounts/sales", "a UID STORE 1:* (UNCHANGEDSINCE 999) FLAGS (\\Seen)"],
  ["Archive", "a STORE 1:* (UNCHANGEDSINCE 999) -FLAGS (\\Deleted)"],
]) {
  step("store", { user: "admin", box, line });
}
// The store call itself, with sequence numbers, which the session never passes.
step("setFlags", { user: "admin", box: "INBOX", start: 2, end: 3, flags: ["\\Seen"], useUid: false, operation: "-FLAGS" });
step("setFlags", { user: "admin", box: "INBOX/accounts/admin", start: 1, end: 9007199254740991, flags: ["\\Seen"], useUid: false, operation: "+FLAGS" });
step("setFlags", { user: "admin", box: "Archive", start: 1, end: 2, flags: [], useUid: false, operation: null, unchangedSince: 999 });
step("getMailFlagUpdates", { user: "admin", box: "INBOX", messageIds: ["<a1@outside.test>", "<a3@outside.test>", "<s1@example.com>"] });
step("getMailFlagUpdates", { user: "admin", box: "INBOX/accounts/sales", messageIds: ["<a3@outside.test>", "<a4@outside.test>", "<a1@outside.test>"] });
step("getMailFlagUpdates", { user: "admin", box: "Starred", messageIds: ["<a4@outside.test>", "<p4@example.com>", "<a1@outside.test>"] });
step("getMailFlagUpdates", { user: "admin", box: "Sent Messages", messageIds: ["<s1@example.com>", "<a1@outside.test>"] });
step("getMailFlagUpdates", { user: "admin", box: "Drafts", messageIds: ["<d1@example.com>", "<d2@example.com>"] });
readEverything("admin", ["INBOX", "Drafts", "Starred", "Trash", "INBOX/accounts/sales", "Archive"]);

// --- COPY between every kind of mailbox ----------------------------------------------

const SOURCES = [
  "INBOX",
  "INBOX/accounts/admin",
  "INBOX/accounts/sales",
  "Sent Messages",
  "Sent Messages/accounts/admin",
  "Drafts",
  "Junk",
  "Starred",
  "Trash",
  "Archive",
];
const DESTINATIONS = [
  "INBOX",
  "INBOX/accounts/sales",
  "Sent Messages",
  "Sent Messages/accounts/admin",
  "Drafts",
  "drafts",
  "Junk",
  "Starred",
  "Trash",
  "Archive",
  "Projects/Wörk \"2024\"",
  "INBOX/accounts",
  "Nope",
];
for (const source of SOURCES) {
  for (const destination of DESTINATIONS) {
    step("copy", { user: "admin", box: source, line: `a UID COPY * "${destination.replace(/"/g, '\\"')}"` });
  }
}
step("copy", { user: "admin", box: "INBOX", line: "a UID COPY 3:5,4:6 Archive" });
step("copy", { user: "admin", box: "INBOX", line: "a COPY 2,1 Archive" });
step("copy", { user: "admin", box: "INBOX", line: "a COPY 999 Archive" });
step("copy", { user: "admin", box: "INBOX", line: "a UID COPY 999 Archive" });
step("copy", { user: "alice", box: "INBOX/accounts/work", line: "a UID COPY 1:* Archive" });
step("copy", { user: "alice", box: "INBOX", line: "a UID COPY 1:* INBOX/accounts/home" });
// A COPY keeps its source's date, so the boxes it fans one mail out to tie
// on their latest mail. A newer delivery to each decides their order.
for (const [user, account] of [
  ["admin", `INBOX/accounts@${DOMAIN}`],
  ["admin", `Projects/Wörk "2024"@${DOMAIN}`],
  ["admin", `Archive@${DOMAIN}`],
  ["admin", SALES],
  ["alice", `Archive@alice.${DOMAIN}`],
  ["alice", HOME],
]) {
  deliver(user, account, `<latest-${account.split("@")[0].replace(/[^A-Za-z]/g, "")}-${user}@outside.test>`);
}
listing("admin");
listing("alice");
readEverything("admin", ["INBOX", "Drafts", "Junk", "Starred", "Trash", "Archive", "Projects/Wörk \"2024\"", "Sent Messages", "INBOX/accounts/sales"]);

// --- MOVE ----------------------------------------------------------------------------

for (const [source, line] of [
  ["Archive", 'a UID MOVE 1:2 "Projects/Wörk \\"2024\\""'],
  ["INBOX/accounts/sales", "a MOVE 1 Archive"],
  ["INBOX/accounts/admin", "a UID MOVE * Trash"],
  ["Starred", "a MOVE 1 INBOX"],
  ["Trash", "a MOVE 1 Starred"],
  ["Drafts", "a MOVE 1 Archive"],
  ["Junk", "a MOVE 1 INBOX"],
  ["Sent Messages/accounts/admin", 'a MOVE 1 "Sent Messages"'],
  ["Sent Messages", "a MOVE 1 Archive"],
  ["INBOX", "a UID MOVE 1 INBOX/accounts/sales"],
  ["INBOX", "a MOVE 1 inbox"],
  ["INBOX", "a MOVE 1 Nope"],
  ["INBOX", "a MOVE 999 Archive"],
  ["Archive", "a MOVE 1:* Trash"],
]) {
  step("move", { user: "admin", box: source, line });
}
step("expungeUids", { user: "admin", box: "Trash", uids: [1, 999] });
step("expungeUids", { user: "admin", box: "INBOX", uids: [998, 999] });
readEverything("admin", ["INBOX", "Starred", "Trash", "Archive", "Projects/Wörk \"2024\"", "INBOX/accounts/sales"]);

// --- EXPUNGE -------------------------------------------------------------------------

for (const box of ["Trash", "INBOX/accounts/sales", "Archive", "Drafts", "Junk", "Sent Messages", "Starred", "INBOX"]) {
  step("store", { user: "admin", box, line: "a STORE 1 +FLAGS (\\Deleted)" });
  step("expunge", { user: "admin", box });
  step("getAllUids", { user: "admin", box });
}
step("expunge", { user: "admin", box: "INBOX" });
readEverything("admin", ADMIN_BOXES);

// --- mailbox changes -----------------------------------------------------------------

step("renameMailbox", { user: "admin", oldName: "Archive", newName: "Archive 2" });
step("deleteMailboxByName", { user: "admin", name: "Projects/Wörk \"2024\"" });
step("setMailboxSubscribed", { user: "admin", name: "Projects", subscribed: true });
step("setMailboxSubscribed", { user: "admin", name: "Archive 2", subscribed: false });
listing("admin");
readEverything("admin", ["Archive", "Archive 2", "Projects/Wörk \"2024\""]);
step("createMailbox", { user: "admin", name: "Archive" });
step("createMailbox", { user: "admin", name: "Projects/Wörk \"2024\"" });
listing("admin");
readEverything("admin", ["Archive", "Archive 2", "Projects/Wörk \"2024\""]);
step("fetch", { user: "admin", box: "Archive", line: "a UID FETCH 1:* (UID FLAGS ENVELOPE)" });
step("search", { user: "admin", box: "Projects/Wörk \"2024\"", line: "a UID SEARCH ALL" });

// --- the end state of every view -------------------------------------------------------

for (const user of ["admin", "alice", "bob"]) listing(user);
readEverything("admin", ADMIN_BOXES);
step("fetch", { user: "admin", box: "INBOX", line: "a UID FETCH 1:* (UID FLAGS MODSEQ)" });
step("fetch", { user: "admin", box: "Trash", line: "a UID FETCH 1:* (UID FLAGS)" });
step("fetch", { user: "admin", box: "Starred", line: "a UID FETCH 1:* (UID FLAGS)" });

// ---------------------------------------------------------------------------
// The database
// ---------------------------------------------------------------------------

const connection = {
  host: process.env.POSTGRES_HOST || "127.0.0.1",
  port: process.env.POSTGRES_PORT || "5432",
  user: process.env.POSTGRES_USER || "postgres",
  password: process.env.POSTGRES_PASSWORD ?? "postgres",
};
const database = `mail_imapdb_ts_${process.pid}`;
Object.assign(process.env, {
  POSTGRES_HOST: connection.host,
  POSTGRES_PORT: connection.port,
  POSTGRES_USER: connection.user,
  POSTGRES_PASSWORD: connection.password,
  POSTGRES_DATABASE: database,
  EMAIL_DOMAIN: DOMAIN,
  TZ: "UTC",
  NODE_ENV: process.env.NODE_ENV ?? "test",
});

const admin = new SQL({
  hostname: connection.host,
  port: Number(connection.port),
  username: connection.user,
  password: connection.password,
  database: "postgres",
  max: 1,
});
await admin.unsafe(`CREATE DATABASE "${database}"`);

const here = import.meta.dir;
const workDirectory = fs.mkdtempSync(path.join(fs.realpathSync(os.tmpdir()), "mail-imapdb-"));
process.chdir(workDirectory);

/** Leaves nothing behind, however the run ends. */
const cleanUp = async () => {
  await admin.unsafe(`DROP DATABASE IF EXISTS "${database}" WITH (FORCE)`);
  await admin.close();
  fs.rmSync(workDirectory, { recursive: true, force: true });
};
const abandon = (error: unknown) => {
  void cleanUp().finally(() => {
    process.stderr.write(`${error instanceof Error ? (error.stack ?? error.message) : String(error)}\n`);
    process.exit(1);
  });
};
process.on("uncaughtException", abandon);
process.on("unhandledRejection", abandon);

// The `server` barrel first: it fixes the order the module graph loads in.
await import("../../../../server/index");
const { pool } = await import("../../../../server/lib/postgres/client");
const { Mail, SignedUser } = await import("../../../../common");
const core = await import("../../../../server/lib/postgres/repositories/mails/core");
const counters = await import("../../../../server/lib/postgres/repositories/mails/counters");
const mailboxes = await import("../../../../server/lib/postgres/repositories/mailboxes");
const repository = await import("../../../../server/lib/postgres/repositories/mails/imap");
const httpQuery = await import("../../../../server/lib/postgres/repositories/mails/http-query");
const { Store } = await import("../../../../server/lib/imap/store");
const { parseCommand } = await import("../../../../server/lib/imap/parsers/command-parser");
const { getRequestedFields, convertSequenceSet } = await import("../../../../server/lib/imap/fetch-helpers");
const { resolveSeqRangeToUids, resolveUidRangeSentinel } = await import("../../../../server/lib/imap/sequence-resolver");
const { cloneMailToDestination, resolveDestContext } = await import("../../../../server/lib/imap/message-ops");
const util = await import("../../../../server/lib/imap/util");

// The code under test logs what it absorbs; the record holds it.
const say = (line: string) => process.stdout.write(`${line}\n`);
console.error = () => undefined;
console.warn = () => undefined;
console.log = () => undefined;
console.info = () => undefined;
console.debug = () => undefined;

const schema = JSON.parse(fs.readFileSync(path.join(here, "../../../schema.json"), "utf8")) as {
  tables: { createSql: string }[];
  searchVectorDdl: string[];
  indexes: { sql: string }[];
};
for (const table of schema.tables) await pool.query(table.createSql);
for (const sql of schema.searchVectorDdl) await pool.query(sql);
for (const index of schema.indexes) await pool.query(index.sql);

// --- labels for generated ids --------------------------------------------------------

const labels = new Map<string, string>();
const refreshLabels = async () => {
  const mails = await pool.query("SELECT mail_id, user_id, message_id FROM mails");
  for (const row of mails.rows) {
    if (!labels.has(row.mail_id)) labels.set(row.mail_id, `mail(${row.user_id} ${row.message_id})`);
  }
  const boxes = await pool.query("SELECT mailbox_id, user_id, name FROM mailboxes");
  for (const row of boxes.rows) {
    if (!labels.has(row.mailbox_id)) labels.set(row.mailbox_id, `mailbox(${row.user_id} ${row.name})`);
  }
};

const VOLATILE_KEYS = ["created"];
/** Replaces generated ids by what they stand for, and drops the clock. */
const normalize = (value: unknown): Json => {
  if (value === undefined || value === null) return null;
  if (typeof value === "string") return labels.get(value) ?? value;
  if (typeof value === "number" && !Number.isFinite(value)) return { $n: String(value) };
  if (Array.isArray(value)) return value.map(normalize);
  if (typeof value === "object") {
    const out: Record<string, Json> = {};
    for (const [key, entry] of Object.entries(value as Record<string, unknown>)) {
      if (entry !== undefined && !VOLATILE_KEYS.includes(key)) out[key] = normalize(entry);
    }
    return out;
  }
  return value as Json;
};

// --- running a step ------------------------------------------------------------------

type StoreInstance = InstanceType<typeof Store>;
type MailInstance = InstanceType<typeof Mail>;
const userOf = (a: Args) => USERS[a.user as string];
const storeOf = (a: Args): StoreInstance => new Store(new SignedUser(userOf(a)));

const mailIdsOf = async (user_id: string, messageIds: string[]): Promise<string[]> => {
  const result = await pool.query(
    "SELECT mail_id FROM mails WHERE user_id = $1 AND message_id = ANY($2) ORDER BY message_id COLLATE \"C\"",
    [user_id, messageIds]
  );
  return result.rows.map((row) => row.mail_id as string);
};

const mailIdOf = async (user_id: string, messageId: string): Promise<string> =>
  (await mailIdsOf(user_id, [messageId]))[0] ?? "c0000000-0000-4000-8000-00000000dead";

/**
 * The account boxes are listed newest mail first; two accounts whose latest
 * mail ties come back in whatever order PostgreSQL meets them, which no two
 * databases need share. The scenario must not depend on one.
 */
const assertAccountOrderDecided = async (a: Args) => {
  const { id, username } = userOf(a);
  for (const sent of [false, true]) {
    const domain = username === "admin" ? DOMAIN : `${username}.${DOMAIN}`;
    const { sql, values } = httpQuery.buildAccountStatsQuery(id, sent, domain, false);
    const rows = (await pool.query(sql, values)).rows;
    const latest = rows.map((row) => String(row.latest));
    if (new Set(latest).size !== latest.length) {
      throw new Error(`accounts tie on their latest mail for ${username} (sent ${sent}): ${JSON.stringify(rows)}`);
    }
  }
};

const sha256 = (data: string | Buffer) => crypto.createHash("sha256").update(data).digest("hex");

/** By UID, then by label: an UPDATE returns its rows in no set order. */
const byUid = (a: { uid: number; mail_id: string }, b: { uid: number; mail_id: string }) => {
  if (a.uid !== b.uid) return a.uid - b.uid;
  const [left, right] = [labels.get(a.mail_id) ?? a.mail_id, labels.get(b.mail_id) ?? b.mail_id];
  return left < right ? -1 : left > right ? 1 : 0;
};
const ascending = (a: number, b: number) => a - b;

/** The request a command line carries, and whether it was a UID command. */
const parseLine = (line: string) => {
  const parsed = parseCommand(line);
  if (!parsed.success || !parsed.value) throw new Error(`${line} does not parse`);
  const request = parsed.value.request;
  return request.type === "UID" ? { isUid: true, request: request.data.request } : { isUid: false, request };
};

/** The UID ranges a command's sequence set resolves to, as the session resolves them. */
const uidRanges = (
  sequenceSet: Parameters<typeof convertSequenceSet>[0],
  isUid: boolean,
  seqToUid: number[]
): { uidStart: number; uidEnd: number }[] => {
  const out: { uidStart: number; uidEnd: number }[] = [];
  for (const { start, end } of convertSequenceSet(sequenceSet)) {
    if (isUid || sequenceSet.type === "uid") {
      out.push(resolveUidRangeSentinel(seqToUid, start, end));
    } else {
      const resolved = resolveSeqRangeToUids(seqToUid, start, end);
      if (resolved) out.push(resolved);
    }
  }
  return out;
};

const CLONE_FIELDS = [
  "subject", "date", "html", "text", "from", "to", "cc", "bcc", "replyTo", "envelopeFrom", "envelopeTo",
  "attachments", "messageId", "insight", "read", "saved", "deleted", "draft", "answered", "uid",
];

/** COPY and MOVE as the session runs them, up to the response. */
const copyOrMove = async (a: Args, move: boolean): Promise<unknown> => {
  const store = storeOf(a);
  const box = a.box as string;
  const { isUid, request } = parseLine(a.line as string);
  if (request.type !== "COPY" && request.type !== "MOVE") throw new Error(`${a.line} is not a COPY or MOVE`);
  const destination = util.canonicalMailbox(request.data.mailbox);
  if (move && destination === box) return { status: "noop" };
  if (!(await store.mailboxExists(destination))) return { status: "TRYCREATE" };
  const seqToUid = (await store.getAllUids(box)) ?? [];
  const ranges = uidRanges(request.data.sequenceSet, isUid, seqToUid);
  if (ranges.length === 0) return { status: "nothing" };
  const sources: StoreFetchedMail[] = [];
  for (const { uidStart, uidEnd } of ranges) {
    const batch = await store.getMessages(box, uidStart, uidEnd, CLONE_FIELDS, true);
    batch.forEach((mail) => sources.push(mail));
  }
  if (sources.length === 0) return { status: "nothing" };
  const user = store.getUser();
  const dest = resolveDestContext(user.username, destination);
  const sourceIsDomainScoped = util.isDomainScoped(box);
  const srcUidOf = (mail: StoreFetchedMail) => (sourceIsDomainScoped ? mail.uid!.domain : mail.uid!.account);
  sources.sort((x, y) => srcUidOf(x) - srcUidOf(y));
  const seen = new Set<number>();
  const unique = sources.filter((mail) => {
    const uid = srcUidOf(mail);
    if (seen.has(uid)) return false;
    seen.add(uid);
    return true;
  });
  const pairs: [number, number][] = [];
  for (const source of unique) {
    const result = await cloneMailToDestination(store, source as never, srcUidOf(source), destination, dest);
    if (!result) return { status: "partial", pairs };
    pairs.push([result.srcUid, result.destUid]);
  }
  if (!move) return { status: "copied", destination, pairs };
  const expunged = await store.expungeUids(box, pairs.map(([src]) => src));
  return { status: "moved", destination, pairs, expunged: [...expunged].sort(ascending) };
};

/** A mail from its scenario fields, as `new Mail` builds it. */
const mailOf = (fields: Args): MailInstance => {
  const init = { ...fields } as Record<string, unknown>;
  delete init.textRepeat;
  return new Mail(init as never);
};

const ops: Record<string, (a: Args) => Promise<unknown>> = {
  sql: async (a) => ({ rowCount: (await pool.query(a.sql as string, a.values as unknown[])).rowCount }),
  ddl: async (a) => {
    await pool.query(a.sql as string);
    return null;
  },

  // SMTP delivery and the send path: reserve both UIDs, then save the mail
  // under its per-account box (and INBOX for received mail).
  deliver: async (a) => {
    const user_id = userOf(a).id;
    const sent = a.sent as boolean;
    const envelopeFrom = (a.envelopeFrom ?? null) as { address: string }[] | null;
    const envelopeTo = (a.envelopeTo ?? null) as { address: string }[] | null;
    const scope = sent ? envelopeFrom?.[0]?.address : envelopeTo?.[0]?.address;
    const mailbox = scope ? (sent ? util.accountToSentBox(scope) : util.accountToBox(scope)) : undefined;
    const uid_domain = await counters.getDomainUidNext(user_id, sent);
    const uid_mailbox = await counters.getAccountUidNext(user_id, scope ?? "", sent);
    const repeat = a.textRepeat as { unit: string; count: number } | undefined;
    const field = (name: string) => (a[name] ?? undefined) as { value: object; text: string } | undefined;
    const saved = await core.saveMail({
      user_id,
      message_id: a.messageId as string,
      in_reply_to: (a.inReplyTo ?? null) as string | null,
      reference_ids: ((a.references as string[] | undefined)?.length ? a.references : null) as string[] | null,
      subject: a.subject as string,
      date: a.date as string,
      html: a.html as string,
      text: repeat ? repeat.unit.repeat(repeat.count) : (a.text as string),
      from_address: field("from")?.value,
      from_text: field("from")?.text,
      to_address: field("to")?.value,
      to_text: field("to")?.text,
      cc_address: field("cc")?.value,
      cc_text: field("cc")?.text,
      bcc_address: field("bcc")?.value,
      bcc_text: field("bcc")?.text,
      reply_to_address: field("replyTo")?.value,
      reply_to_text: field("replyTo")?.text,
      envelope_from: envelopeFrom ?? undefined,
      envelope_to: envelopeTo ?? undefined,
      attachments: (a.attachments ?? undefined) as object | undefined,
      read: (a.read ?? false) as boolean,
      saved: (a.saved ?? false) as boolean,
      sent,
      deleted: (a.deleted ?? false) as boolean,
      draft: false,
      insight: (a.insight ?? undefined) as object | undefined,
      uid_domain,
      uid_mailbox,
      mailbox,
      spam_score: (a.spamScore ?? 0) as number,
      spam_reasons: (a.spamReasons ?? null) as string[] | null,
      is_spam: (a.isSpam ?? false) as boolean,
      domain_mailbox: "INBOX",
    });
    await refreshLabels();
    return saved;
  },

  createMailbox: async (a) => {
    const result = await mailboxes.createMailbox({
      user_id: userOf(a).id,
      name: a.name as string,
      address: (a.address ?? undefined) as string | undefined,
      special_use: (a.special_use ?? undefined) as string | undefined,
      subscribed: (a.subscribed ?? undefined) as boolean | undefined,
    } as never);
    await refreshLabels();
    return result.status === "created" ? { status: result.status, mailbox: result.mailbox.toJSON() } : result;
  },
  deleteMailboxByName: (a) => mailboxes.deleteMailboxByName(userOf(a).id, a.name as string),
  renameMailbox: (a) => mailboxes.renameMailbox(userOf(a).id, a.oldName as string, a.newName as string),
  setMailboxSubscribed: (a) => mailboxes.setMailboxSubscribed(userOf(a).id, a.name as string, a.subscribed as boolean),

  listMailboxes: async (a) => {
    await assertAccountOrderDecided(a);
    return storeOf(a).listMailboxes();
  },
  listMailboxEntries: async (a) => {
    await assertAccountOrderDecided(a);
    return storeOf(a).listMailboxEntries();
  },
  mailboxExists: (a) => storeOf(a).mailboxExists(a.box as string),
  countMessages: (a) => storeOf(a).countMessages(a.box as string),
  getUidNext: (a) => storeOf(a).getUidNext(a.box as string),
  getAllUids: (a) => storeOf(a).getAllUids(a.box as string),
  getFirstUnseenUid: (a) => storeOf(a).getFirstUnseenUid(a.box as string),
  getHighestModseq: (a) => storeOf(a).getHighestModseq(a.box as string),
  getMailFlagUpdates: async (a) => {
    const ids = await mailIdsOf(userOf(a).id, a.messageIds as string[]);
    const updates = await storeOf(a).getMailFlagUpdates(a.box as string, ids);
    return updates?.sort(byUid) ?? null;
  },
  getMessages: async (a) =>
    [
      ...(
        await storeOf(a).getMessages(
          a.box as string,
          a.start as number,
          a.end as number,
          a.fields as string[],
          a.useUid as boolean,
          (a.changedSince ?? undefined) as number | undefined
        )
      ).entries(),
    ],
  concurrentFetch: async (a) => {
    const store = storeOf(a);
    const reads = await Promise.all(
      Array.from({ length: a.count as number }, () =>
        store.getMessages(a.box as string, 1, Number.MAX_SAFE_INTEGER, a.fields as string[], true)
      )
    );
    const shapes = new Set(reads.map((read) => JSON.stringify([...read.entries()])));
    return { reads: reads.length, distinct: shapes.size, first: [...reads[0].entries()] };
  },
  setFlags: async (a) => {
    const result = await storeOf(a).setFlags(
      a.box as string,
      a.start as number,
      a.end as number,
      a.flags as string[],
      a.useUid as boolean,
      (a.operation ?? undefined) as "FLAGS" | "+FLAGS" | "-FLAGS" | undefined,
      (a.unchangedSince ?? undefined) as number | undefined
    );
    return { updated: [...result.updated].sort(byUid), failed: [...result.failed].sort(ascending) };
  },
  expunge: async (a) => [...(await storeOf(a).expunge(a.box as string))].sort(ascending),
  expungeUids: async (a) => [...(await storeOf(a).expungeUids(a.box as string, a.uids as number[]))].sort(ascending),
  search: async (a) => {
    const { request } = parseLine(a.line as string);
    if (request.type !== "SEARCH") throw new Error(`${a.line} is not a SEARCH`);
    return storeOf(a).search(a.box as string, request.data.criteria);
  },
  storeMail: async (a) => {
    const mail = mailOf(a.mail as Args);
    const ok = await storeOf(a).storeMail(mail, (a.destination ?? undefined) as string | undefined);
    await refreshLabels();
    return { ok, uid: mail.uid };
  },

  // FETCH as the session reads it: the projection the items ask for, one
  // range read per resolved range, merged by mail id.
  fetch: async (a) => {
    const store = storeOf(a);
    const box = a.box as string;
    const { isUid, request } = parseLine(a.line as string);
    if (request.type !== "FETCH") throw new Error(`${a.line} is not a FETCH`);
    const fetchRequest = request.data;
    const seqToUid = (await store.getAllUids(box)) ?? [];
    const fields = Array.from(getRequestedFields(fetchRequest.dataItems)) as string[];
    const merged = new Map<string, unknown>();
    for (const { uidStart, uidEnd } of uidRanges(fetchRequest.sequenceSet, isUid, seqToUid)) {
      const messages = await store.getMessages(box, uidStart, uidEnd, fields, true, fetchRequest.changedSince);
      messages.forEach((mail, id) => merged.set(id, mail));
    }
    return [...merged.entries()];
  },

  // STORE as the session runs it: one store call per resolved range, then
  // the Starred / Trash pivots of every row it changed.
  store: async (a) => {
    const store = storeOf(a);
    const box = a.box as string;
    const user_id = userOf(a).id;
    const { isUid, request } = parseLine(a.line as string);
    if (request.type !== "STORE") throw new Error(`${a.line} is not a STORE`);
    const { sequenceSet, operation, flags, unchangedSince } = request.data;
    const baseOperation = operation.replace(".SILENT", "") as "FLAGS" | "+FLAGS" | "-FLAGS";
    const touchesSaved = baseOperation === "FLAGS" || flags.includes("\\Flagged");
    const touchesDeleted = baseOperation === "FLAGS" || flags.includes("\\Deleted");
    const seqToUid = (await store.getAllUids(box)) ?? [];
    const passes: unknown[] = [];
    for (const { uidStart, uidEnd } of uidRanges(sequenceSet, isUid, seqToUid)) {
      const result = await store.setFlags(box, uidStart, uidEnd, flags, true, baseOperation, unchangedSince);
      const updated = [...result.updated].sort(byUid);
      for (const row of updated) {
        if (touchesSaved) await counters.syncMailboxPivot(user_id, "Starred", row.mail_id, row.saved);
        if (touchesDeleted) await counters.syncMailboxPivot(user_id, "Trash", row.mail_id, row.deleted);
      }
      passes.push({ uidStart, uidEnd, updated, failed: [...result.failed].sort(ascending) });
    }
    return passes;
  },

  copy: (a) => copyOrMove(a, false),
  move: (a) => copyOrMove(a, true),

  // APPEND's storage step, as the session runs it once the message is parsed.
  append: async (a) => {
    const store = storeOf(a);
    const target = util.canonicalMailbox(a.mailbox as string);
    if (!(await store.mailboxExists(target))) return { status: "TRYCREATE" };
    const flags = a.flags as string[];
    const mail = mailOf(a.mail as Args);
    mail.draft = flags.includes("\\Draft");
    mail.read = flags.includes("\\Seen");
    mail.saved = flags.includes("\\Flagged");
    mail.deleted = flags.includes("\\Deleted");
    mail.answered = flags.includes("\\Answered");
    const user = store.getUser();
    const account = util.boxToAccount(user.username, target);
    const targetIsSent = util.isSentBox(target);
    mail.sent = targetIsSent;
    mail.uid.domain = await counters.getDomainUidNext(user.id, targetIsSent);
    mail.uid.account = util.isMappedUtilityFolder(target)
      ? await counters.getMailboxUidNext(user.id, target)
      : await counters.getAccountUidNext(user.id, account, targetIsSent);
    const ok = await store.storeMail(mail, target);
    await refreshLabels();
    return { status: ok ? "stored" : "failed", uid: util.isDomainScoped(target) ? mail.uid.domain : mail.uid.account };
  },

  // The chunked readers against real rows: what each chunk holds.
  textChunks: async (a) => {
    const mail_id = await mailIdOf(userOf(a).id, a.messageId as string);
    const chunks: unknown[] = [];
    for await (const chunk of repository.pgTextChunks(
      mail_id,
      userOf(a).id,
      a.source as "text" | "html",
      (a.chunkChars ?? undefined) as number | undefined
    )) {
      chunks.push({ codePoints: [...chunk].length, utf16: chunk.length, sha256: sha256(chunk), head: [...chunk].slice(0, 4).join("") });
    }
    return chunks;
  },
  byteChunks: async (a) => {
    const mail_id = await mailIdOf(userOf(a).id, a.messageId as string);
    const chunks: unknown[] = [];
    for await (const chunk of repository.pgByteChunks(
      mail_id,
      userOf(a).id,
      a.source as "text" | "html",
      (a.startByte ?? undefined) as number | undefined,
      (a.chunkBytes ?? undefined) as number | undefined
    )) {
      chunks.push({ bytes: chunk.byteLength, sha256: sha256(chunk), head: chunk.subarray(0, 4).toString("hex") });
    }
    return chunks;
  },
};

const recorded: Json[] = [];
for (const [index, { op, args }] of steps.entries()) {
  const run = ops[op];
  if (!run) throw new Error(`no op ${op}`);
  let outcome: Json;
  try {
    // Rows an earlier step filed are named before this one sorts by name.
    await refreshLabels();
    const returned = await run(args);
    // A COPY files rows nobody has named yet; name them before recording.
    await refreshLabels();
    outcome = { ok: normalize(JSON.parse(JSON.stringify(returned ?? null))) };
  } catch (error) {
    if (error instanceof Error && error.message.startsWith("accounts tie")) throw error;
    const thrown = error as { code?: string; constraint?: string; message?: string };
    await refreshLabels();
    outcome = {
      threw: {
        code: thrown.code ?? null,
        constraint: thrown.constraint ?? null,
        // PostgreSQL's own wording carries over; a JavaScript error's does not.
        message: thrown.code ? (thrown.message ?? null) : null,
      },
    };
  }
  recorded.push({ step: index + 1, op, ...(outcome as object) } as Json);
}

// --- the tables ----------------------------------------------------------------------

const MAIL_COLUMNS =
  "mail_id, user_id, message_id, subject, date, html, text, from_address, from_text, to_address, to_text, cc_address, cc_text, bcc_address, bcc_text, reply_to_address, reply_to_text, envelope_from, envelope_to, attachments, read, saved, sent, deleted, draft, answered, expunged, insight, uid_domain, modseq, spam_score, spam_reasons, is_spam, rfc822_size, text_line_count, html_line_count, in_reply_to, reference_ids, md5(text) AS text_md5";

await refreshLabels();
const mails = (
  await pool.query(
    `SELECT ${MAIL_COLUMNS.replace("html, text,", "html, CASE WHEN length(text) > 1000 THEN NULL ELSE text END AS text,")} FROM mails ORDER BY user_id, message_id COLLATE "C"`
  )
).rows;
const mappings = (
  await pool.query(`SELECT user_id, mailbox, mail_id, uid FROM mail_mailbox_uid ORDER BY user_id, mailbox COLLATE "C", uid`)
).rows;
const counterRows = (
  await pool.query(
    `SELECT user_id, uid_kind, uid_scope, sent, last_uid FROM mail_uid_counters ORDER BY user_id, uid_kind, uid_scope COLLATE "C", sent`
  )
).rows;
const boxes = (
  await pool.query(
    `SELECT mailbox_id, user_id, name, address, parent_id, uid_validity, uid_next, subscribed, special_use FROM mailboxes ORDER BY user_id, name COLLATE "C"`
  )
).rows;

const tables = {
  mails: mails.map(normalize),
  mail_mailbox_uid: mappings.map(normalize),
  mail_uid_counters: counterRows.map(normalize),
  mailboxes: boxes.map(normalize),
};

// ---------------------------------------------------------------------------

const write = (name: string, value: unknown) => fs.writeFileSync(path.join(here, name), `${JSON.stringify(value, null, 1)}\n`);
write("scenario.steps.json", { generatedBy: "src/server-rs/tests/golden/imapdb/scenario.ts", steps });
write("scenario.expected.json", {
  generatedBy: "src/server-rs/tests/golden/imapdb/scenario.ts",
  results: recorded,
  tables,
});

await pool.end();
await cleanUp();

const failures = recorded.filter((entry) => "threw" in (entry as object)).length;
say(`${steps.length} steps (${failures} report a failure), ${mails.length} mails, ${mappings.length} mapping rows, ${counterRows.length} counters, ${boxes.length} mailboxes`);
process.exit(0);
