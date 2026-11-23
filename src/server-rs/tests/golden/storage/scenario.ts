/**
 * The differential database scenario for the mail storage core.
 *
 *   bun src/server-rs/tests/golden/storage/scenario.ts
 *
 * Drives the real TypeScript repositories through a scripted scenario against
 * a fresh PostgreSQL database, then writes two files beside this one:
 *
 * - `scenario.steps.json`: the scenario itself, as data.
 * - `scenario.expected.json`: every step's return value and the resulting
 *   `mails`, `mail_mailbox_uid`, `mail_uid_counters` and `mailboxes` tables.
 *
 * `tests/storage_differential.rs` runs the same steps through the Rust port
 * against another fresh database and requires the same dump. Generated ids
 * are replaced by labels and the save time by `<now>`; a value that depends
 * on which of several concurrent saves won a race is replaced by `<conc>`,
 * and the step that raced reports the set of values instead.
 *
 * PostgreSQL is reached with the usual `POSTGRES_*` variables (default
 * `postgres:postgres@127.0.0.1:5432`). The database is created and dropped
 * here, named `mail_core_ts_<pid>`.
 */
import fs from "fs";
import os from "os";
import path from "path";
import { SQL } from "bun";

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
type Args = Record<string, Json>;
interface Step {
  op: string;
  args: Args;
  as?: string;
}

// ---------------------------------------------------------------------------
// The scenario
// ---------------------------------------------------------------------------

const U1 = "a0000000-0000-4000-8000-000000000001";
const U2 = "a0000000-0000-4000-8000-000000000002";
const U3 = "a0000000-0000-4000-8000-000000000003";
const U4 = "a0000000-0000-4000-8000-000000000004";
const LEGACY_1 = "b0000000-0000-4000-8000-000000000001";
const LEGACY_2 = "b0000000-0000-4000-8000-000000000002";
const LEGACY_SENT = "b0000000-0000-4000-8000-000000000003";
const UNKNOWN_MAIL = "c0000000-0000-4000-8000-00000000dead";
const DATE = "2024-03-04T05:06:07.000Z";
const ATTACHMENT_ON_DISK = "scenario-attachment-on-disk";

const steps: Step[] = [];
const step = (op: string, args: Args = {}, as?: string) => {
  steps.push(as ? { op, args, as } : { op, args });
};
/** A value an earlier step returned, such as `ref("m1._id")`. */
const ref = (pathInResults: string): Json => ({ $ref: pathInResults });

const ADMIN = "admin@example.com";
const SALES = "sales@example.com";
const ADMIN_BOX = "INBOX/accounts/admin";
const SALES_BOX = "INBOX/accounts/sales";
const ADMIN_SENT_BOX = "Sent Messages/accounts/admin";

/** Reserves the two UIDs a received or sent mail carries, as SMTP delivery does. */
const reserve = (name: string, user_id: string, account: string, sent = false) => {
  step("getDomainUidNext", { user_id, sent }, `${name}.d`);
  step("getAccountUidNext", { user_id, account, sent }, `${name}.a`);
};

const peeks = (user_id: string, label: string) => {
  step("getUidNext", { user_id, scope: { kind: "domain", sent: false } }, `${label}.next.domain`);
  step("getUidNext", { user_id, scope: { kind: "domain", sent: true } }, `${label}.next.domain.sent`);
  step("getUidNext", { user_id, scope: { kind: "account", account: ADMIN, sent: false } }, `${label}.next.admin`);
  step("getUidNext", { user_id, scope: { kind: "account", account: ADMIN, sent: true } }, `${label}.next.admin.sent`);
  step("getUidNext", { user_id, scope: { kind: "account", account: SALES, sent: false } }, `${label}.next.sales`);
  step("getUidNext", { user_id, scope: { kind: "account", account: "Archive@example.com", sent: false } }, `${label}.next.archive`);
  step("getUidNext", { user_id, scope: { kind: "mailbox", mailbox: "Starred" } }, `${label}.next.starred`);
  step("getUidNext", { user_id, scope: { kind: "mailbox", mailbox: "Trash" } }, `${label}.next.trash`);
  for (const [mailbox, sent] of [
    [null, false],
    [null, true],
    ["Junk", false],
    ["Drafts", false],
    [ADMIN_BOX, false],
    [SALES_BOX, false],
    [ADMIN_SENT_BOX, true],
    ["Archive", false],
    ["Starred", false],
    ["Trash", false],
  ] as [string | null, boolean][]) {
    step("getHighestModseq", { user_id, mailbox, sent }, `${label}.modseq.${mailbox ?? "domain"}.${sent ? "sent" : "received"}`);
  }
};

// --- users and files -----------------------------------------------------------

for (const [user_id, username] of [
  [U1, "admin"],
  [U2, "bob"],
  [U3, "legacy"],
  [U4, "limits"],
]) {
  step("sql", {
    sql: "INSERT INTO users (user_id, username, email) VALUES ($1, $2, $3)",
    values: [user_id, username, `${username}@example.com`],
  });
}
step("writeAttachment", { id: ATTACHMENT_ON_DISK, bytes: 1234 });

// --- a fresh user: every counter starts at 1 ----------------------------------

peeks(U1, "fresh");
step("getNextModseq", { user_id: U2 }, "bob.modseq.first");

// --- received mail in every field shape ---------------------------------------

reserve("r1", U1, ADMIN);
step(
  "saveMail",
  {
    user_id: U1,
    message_id: "<full@example.com>",
    in_reply_to: "<parent@example.com>",
    reference_ids: ["<root@example.com>", "<parent@example.com>"],
    subject: "Quarterly report — ünïcode ✓",
    date: DATE,
    html: "<p>Hello <b>world</b> 😀</p>",
    text: "Hello world 😀",
    from_address: [{ address: "sender@example.com", name: "Sender Name" }],
    from_text: "Sender Name <sender@example.com>",
    to_address: [{ address: ADMIN, name: "" }, { address: "other@example.org", name: "Other" }],
    to_text: `${ADMIN}, Other <other@example.org>`,
    cc_address: [{ address: "cc@example.com" }],
    cc_text: "cc@example.com",
    bcc_address: [{ address: "bcc@example.com" }],
    bcc_text: "bcc@example.com",
    reply_to_address: [{ address: "reply@example.com", name: "Reply" }],
    reply_to_text: "Reply <reply@example.com>",
    envelope_from: [{ address: "bounce@example.com" }],
    envelope_to: [{ address: ADMIN }],
    attachments: [
      { content: { data: ATTACHMENT_ON_DISK }, contentType: "application/pdf", filename: "report.pdf", size: 1234 },
      { content: { data: "missing-on-disk" }, contentType: "image/png", filename: 'pic "1".png', size: 10 },
    ],
    read: false,
    saved: false,
    sent: false,
    insight: { summary: "A report", tags: ["work", "q1"], score: 7 },
    uid_domain: ref("r1.d"),
    uid_mailbox: ref("r1.a"),
    spam_score: 3,
    spam_reasons: [],
    is_spam: false,
    mailbox: ADMIN_BOX,
    domain_mailbox: "INBOX",
  },
  "m.full"
);

reserve("r2", U1, ADMIN);
step(
  "saveMail",
  { user_id: U1, message_id: "<minimal@example.com>", uid_domain: ref("r2.d"), uid_mailbox: ref("r2.a"), mailbox: ADMIN_BOX, domain_mailbox: "INBOX" },
  "m.minimal"
);

reserve("r3", U1, SALES);
step(
  "saveMail",
  {
    user_id: U1,
    message_id: "<empties@example.com>",
    date: DATE,
    subject: "",
    html: "",
    text: "",
    from_address: [],
    from_text: "",
    to_address: null,
    cc_address: [{ address: SALES }],
    cc_text: SALES,
    bcc_address: [{ address: "hidden@example.com" }],
    envelope_to: [{ address: SALES }],
    attachments: [],
    reference_ids: [],
    in_reply_to: "",
    insight: {},
    spam_reasons: null,
    uid_domain: ref("r3.d"),
    uid_mailbox: ref("r3.a"),
    mailbox: SALES_BOX,
    domain_mailbox: "INBOX",
  },
  "m.empties"
);

reserve("r4", U1, ADMIN);
step(
  "saveMail",
  {
    user_id: U1,
    message_id: "<spam@example.com>",
    date: "2024-03-05T00:00:00.000Z",
    subject: "You won!!!",
    html: "<h1>Claim now</h1>",
    from_address: [{ address: "winner@spam.example" }],
    from_text: "winner@spam.example",
    to_address: [{ address: ADMIN }],
    to_text: ADMIN,
    envelope_to: [{ address: ADMIN }],
    uid_domain: ref("r4.d"),
    uid_mailbox: ref("r4.a"),
    spam_score: 12,
    spam_reasons: ["Suspicious subject", "DNSBL: zen"],
    is_spam: true,
    mailbox: ADMIN_BOX,
    domain_mailbox: "INBOX",
  },
  "m.spam"
);

reserve("r5", U1, ADMIN);
step(
  "saveMail",
  {
    user_id: U1,
    message_id: "<text-only@example.com>",
    date: "2024-03-06T10:00:00+09:00",
    subject: "Plain",
    text: "Only text\nwith two lines",
    from_address: [{ address: "a@example.com", name: "A" }],
    from_text: "A <a@example.com>",
    to_address: [{ address: ADMIN }],
    to_text: ADMIN,
    envelope_to: [{ address: ADMIN }],
    read: true,
    answered: true,
    uid_domain: ref("r5.d"),
    uid_mailbox: ref("r5.a"),
    mailbox: ADMIN_BOX,
    domain_mailbox: "INBOX",
  },
  "m.text"
);

// A delivery whose envelope names no account: the empty scope.
step("getDomainUidNext", { user_id: U1, sent: false }, "r6.d");
step("getAccountUidNext", { user_id: U1, account: "", sent: false }, "r6.a");
step(
  "saveMail",
  { user_id: U1, message_id: "<no-envelope@example.com>", date: DATE, html: "<p>html only</p>", uid_domain: ref("r6.d"), uid_mailbox: ref("r6.a"), domain_mailbox: "INBOX" },
  "m.noenvelope"
);

// --- sent mail ---------------------------------------------------------------------

reserve("s1", U1, ADMIN, true);
step(
  "saveMail",
  {
    user_id: U1,
    message_id: "<sent-1@example.com>",
    date: DATE,
    subject: "Re: Quarterly report",
    html: "<p>Thanks</p>",
    text: "Thanks",
    in_reply_to: "<full@example.com>",
    reference_ids: ["<root@example.com>", "<parent@example.com>", "<full@example.com>"],
    from_address: [{ name: "Admin", address: ADMIN }],
    from_text: `Admin <${ADMIN}>`,
    to_address: [{ address: "sender@example.com" }],
    to_text: "sender@example.com",
    reply_to_address: [{ name: "Admin", address: ADMIN }],
    reply_to_text: ADMIN,
    envelope_from: [{ name: "Admin", address: ADMIN }],
    envelope_to: [{ address: "sender@example.com" }],
    attachments: [],
    read: true,
    sent: true,
    saved: false,
    uid_domain: ref("s1.d"),
    uid_mailbox: ref("s1.a"),
    mailbox: ADMIN_SENT_BOX,
    domain_mailbox: "INBOX",
    spam_score: 0,
    spam_reasons: null,
    is_spam: false,
  },
  "m.sent1"
);

// A mail to oneself: the sent copy is stored first, the inbound delivery of
// the same Message-ID merges into it.
reserve("s2", U1, ADMIN, true);
step(
  "saveMail",
  {
    user_id: U1,
    message_id: "<self@example.com>",
    date: DATE,
    subject: "Note to self",
    text: "remember",
    from_address: [{ address: ADMIN }],
    from_text: ADMIN,
    to_address: [{ address: ADMIN }],
    to_text: ADMIN,
    envelope_from: [{ address: ADMIN }],
    envelope_to: [{ address: ADMIN }],
    read: true,
    sent: true,
    uid_domain: ref("s2.d"),
    uid_mailbox: ref("s2.a"),
    mailbox: ADMIN_SENT_BOX,
    domain_mailbox: "INBOX",
  },
  "m.self.sent"
);
reserve("s2r", U1, ADMIN);
step(
  "saveMail",
  {
    user_id: U1,
    message_id: "<self@example.com>",
    date: DATE,
    subject: "Note to self",
    text: "remember",
    envelope_to: [{ address: ADMIN }, { address: "alias@example.com" }],
    uid_domain: ref("s2r.d"),
    uid_mailbox: ref("s2r.a"),
    mailbox: ADMIN_BOX,
    domain_mailbox: "INBOX",
  },
  "m.self.received"
);

// --- one Message-ID delivered to two accounts of one user --------------------------

reserve("d1", U1, ADMIN);
const fanned = {
  user_id: U1,
  message_id: "<fanout@example.com>",
  date: DATE,
  subject: "To two accounts",
  text: "same message",
  from_address: [{ address: "list@example.org" }],
  from_text: "list@example.org",
  to_address: [{ address: "list@example.org" }],
  to_text: "list@example.org",
};
step(
  "saveMail",
  { ...fanned, envelope_to: [{ address: ADMIN, name: "Admin" }], uid_domain: ref("d1.d"), uid_mailbox: ref("d1.a"), mailbox: ADMIN_BOX, domain_mailbox: "INBOX" },
  "m.fan.first"
);
reserve("d2", U1, SALES);
step(
  "saveMail",
  {
    ...fanned,
    subject: "Ignored: the first delivery's subject stays",
    envelope_to: [{ address: ADMIN }, { address: SALES }, { address: SALES }, { name: "no address" }],
    uid_domain: ref("d2.d"),
    uid_mailbox: ref("d2.a"),
    mailbox: SALES_BOX,
    domain_mailbox: "INBOX",
    read: true,
  },
  "m.fan.second"
);
// The same delivery again: nothing new to record, the mapping keeps its UID.
reserve("d3", U1, SALES);
step(
  "saveMail",
  { ...fanned, envelope_to: [{ address: SALES }], uid_domain: ref("d3.d"), uid_mailbox: ref("d3.a"), mailbox: SALES_BOX, domain_mailbox: "INBOX" },
  "m.fan.third"
);
// Another user holds the same Message-ID as a row of their own.
reserve("d4", U2, "bob@bob.example.com");
step(
  "saveMail",
  { ...fanned, user_id: U2, envelope_to: [{ address: "bob@bob.example.com" }], uid_domain: ref("d4.d"), uid_mailbox: ref("d4.a"), mailbox: "INBOX/accounts/bob", domain_mailbox: "INBOX" },
  "m.fan.bob"
);

// --- IMAP-shaped writes: APPEND, COPY, utility folders -----------------------------

// APPEND INBOX: domain-scoped, so no mapped destination.
step("getDomainUidNext", { user_id: U1, sent: false }, "i1.d");
step("getAccountUidNext", { user_id: U1, account: "INBOX@example.com", sent: false }, "i1.a");
step(
  "saveMail",
  { user_id: U1, message_id: "<appended-inbox@example.com>", date: DATE, subject: "Appended", text: "body", read: true, uid_domain: ref("i1.d"), uid_mailbox: ref("i1.a"), domain_mailbox: "INBOX" },
  "m.append.inbox"
);

// APPEND Drafts / Junk: the placement sets the flag the view selects on.
step("getDomainUidNext", { user_id: U1, sent: false }, "i2.d");
step(
  "saveMail",
  { user_id: U1, message_id: "<appended-drafts@example.com>", date: DATE, subject: "Draft", text: "unfinished", draft: false, uid_domain: ref("i2.d"), domain_mailbox: "INBOX", placement: { draft: true } },
  "m.append.drafts"
);
step("getDomainUidNext", { user_id: U1, sent: false }, "i3.d");
step(
  "saveMail",
  { user_id: U1, message_id: "<appended-junk@example.com>", date: DATE, subject: "Junk", text: "spam", uid_domain: ref("i3.d"), domain_mailbox: "INBOX", placement: { is_spam: true } },
  "m.append.junk"
);

// COPY into a user-created box, twice: the second merges and keeps the UID.
step("sql", { sql: "INSERT INTO mailboxes (user_id, name) VALUES ($1, $2)", values: [U1, "Archive"] });
for (const name of ["c1", "c2"]) {
  step("getDomainUidNext", { user_id: U1, sent: false }, `${name}.d`);
  step("getAccountUidNext", { user_id: U1, account: "Archive@example.com", sent: false }, `${name}.a`);
  step(
    "saveMail",
    {
      user_id: U1,
      message_id: "0123456789abcdef.copy@server",
      date: DATE,
      subject: "Quarterly report — ünïcode ✓",
      text: "Hello world 😀",
      to_address: [{ address: "Archive@example.com", name: "" }],
      to_text: `${ADMIN}, Other <other@example.org>`,
      envelope_to: [{ address: "Archive@example.com", name: "" }],
      cc_address: [],
      cc_text: "cc@example.com",
      read: true,
      saved: true,
      deleted: true,
      answered: true,
      uid_domain: ref(`${name}.d`),
      uid_mailbox: ref(`${name}.a`),
      mailbox: "Archive",
    },
    `m.copy.${name}`
  );
}

// COPY into Starred and Trash: mapped utility boxes with counters of their own.
step("getDomainUidNext", { user_id: U1, sent: false }, "c3.d");
step("getMailboxUidNext", { user_id: U1, mailbox: "Starred" }, "c3.a");
step(
  "saveMail",
  { user_id: U1, message_id: "fedcba9876543210.copy@server", date: DATE, subject: "Starred copy", text: "x", uid_domain: ref("c3.d"), uid_mailbox: ref("c3.a"), mailbox: "Starred", placement: { saved: true } },
  "m.copy.starred"
);
step("getDomainUidNext", { user_id: U1, sent: false }, "c4.d");
step("getMailboxUidNext", { user_id: U1, mailbox: "Trash" }, "c4.a");
step(
  "saveMail",
  { user_id: U1, message_id: "aaaabbbbccccdddd.copy@server", date: DATE, subject: "Trashed copy", text: "x", saved: true, uid_domain: ref("c4.d"), uid_mailbox: ref("c4.a"), mailbox: "Trash", placement: { deleted: true } },
  "m.copy.trash"
);
// The same message copied into Starred again merges, and a placement that
// clears a flag drops the pivot the first copy made.
step("getDomainUidNext", { user_id: U1, sent: false }, "c5.d");
step("getAccountUidNext", { user_id: U1, account: "Archive@example.com", sent: false }, "c5.a");
step(
  "saveMail",
  { user_id: U1, message_id: "aaaabbbbccccdddd.copy@server", date: DATE, uid_domain: ref("c5.d"), uid_mailbox: ref("c5.a"), mailbox: "Archive", placement: { saved: false, deleted: true } },
  "m.copy.trash.again"
);
step(
  "saveMail",
  { user_id: U1, message_id: "aaaabbbbccccdddd.copy@server", date: DATE, placement: {} },
  "m.copy.trash.emptyplacement"
);

// --- UIDs that are already taken ---------------------------------------------------

// Two received rows holding one uid_domain, as rows older than atomic
// reservation do: the second INBOX marker is dropped, the mail is kept.
step("saveMail", { user_id: U1, message_id: "<dup-domain-uid@example.com>", date: DATE, uid_domain: ref("r1.d"), domain_mailbox: "INBOX" }, "m.dupdomain");
// A mapped destination whose UID is taken fails the save after the row was
// inserted, by way of the merge path.
step(
  "saveMail",
  { user_id: U1, message_id: "<dup-mailbox-uid@example.com>", date: DATE, envelope_to: [{ address: ADMIN }], uid_domain: 900, uid_mailbox: ref("r1.a"), mailbox: ADMIN_BOX, domain_mailbox: "INBOX" },
  "m.dupmailbox"
);
step("saveMail", { user_id: U1, message_id: "<bad-date@example.com>", date: "not a date" }, "m.baddate");
step("saveMail", { user_id: "not-a-uuid", message_id: "<bad-user@example.com>", date: DATE }, "m.baduser");
step("saveMail", { user_id: UNKNOWN_MAIL, message_id: "<no-such-user@example.com>", date: DATE }, "m.nouser");
step("saveMail", { user_id: U1, message_id: "<fraction@example.com>", date: DATE, spam_score: 2.5 }, "m.fraction");

peeks(U1, "after-saves");

// --- reads -------------------------------------------------------------------------

step("getMailById", { user_id: U1, mail_id: ref("m.full._id") }, "read.full");
step("getMailById", { user_id: U1, mail_id: ref("m.minimal._id") }, "read.minimal");
step("getMailById", { user_id: U2, mail_id: ref("m.full._id") }, "read.other-user");
step("getMailById", { user_id: U1, mail_id: UNKNOWN_MAIL }, "read.unknown");
step("getMailById", { user_id: U1, mail_id: "not-a-uuid" }, "read.bad-id");
step("getMailByMessageId", { user_id: U1, message_id: "<fanout@example.com>" }, "read.fanout");
step("getMailByMessageId", { user_id: U2, message_id: "<fanout@example.com>" }, "read.fanout.bob");
step("getMailByMessageId", { user_id: U1, message_id: "<self@example.com>" }, "read.self");
step("getMailByMessageId", { user_id: U1, message_id: "<nobody@example.com>" }, "read.missing");
step("getMailByMessageId", { user_id: U1, message_id: "<dup-mailbox-uid@example.com>" }, "read.dupmailbox");
// The attachment route finds the mail an attachment belongs to by containment.
step("findMailByAttachment", { user_id: U1, attachment_id: ATTACHMENT_ON_DISK }, "read.by-attachment");
step("findMailByAttachment", { user_id: U2, attachment_id: ATTACHMENT_ON_DISK }, "read.by-attachment.other-user");
step("findMailByAttachment", { user_id: U1, attachment_id: "no-such-attachment" }, "read.by-attachment.missing");

// --- flags and their mod-sequences ---------------------------------------------------

step("markMailRead", { user_id: U1, mail_id: ref("m.full._id") }, "flag.read.first");
step("markMailRead", { user_id: U1, mail_id: ref("m.full._id") }, "flag.read.again");
step("markMailRead", { user_id: U1, mail_id: ref("m.text._id") }, "flag.read.already");
step("markMailRead", { user_id: U2, mail_id: ref("m.minimal._id") }, "flag.read.other-user");
step("markMailRead", { user_id: U1, mail_id: UNKNOWN_MAIL }, "flag.read.unknown");
step("markMailRead", { user_id: U1, mail_id: "not-a-uuid" }, "flag.read.bad-id");
step("getHighestModseq", { user_id: U1, mailbox: null, sent: false }, "flag.read.modseq");

step("markMailSaved", { user_id: U1, mail_id: ref("m.full._id"), saved: true }, "flag.save.first");
step("markMailSaved", { user_id: U1, mail_id: ref("m.full._id"), saved: true }, "flag.save.again");
step("markMailSaved", { user_id: U1, mail_id: ref("m.minimal._id"), saved: true }, "flag.save.second-mail");
step("markMailSaved", { user_id: U1, mail_id: ref("m.full._id"), saved: false }, "flag.unsave");
step("markMailSaved", { user_id: U1, mail_id: ref("m.full._id"), saved: false }, "flag.unsave.again");
step("markMailSaved", { user_id: U1, mail_id: ref("m.full._id"), saved: true }, "flag.resave");
step("markMailSaved", { user_id: U1, mail_id: ref("m.sent1._id"), saved: true }, "flag.save.sent");
step("markMailSaved", { user_id: U2, mail_id: ref("m.full._id"), saved: true }, "flag.save.other-user");
step("markMailSaved", { user_id: U1, mail_id: UNKNOWN_MAIL, saved: false }, "flag.save.unknown");
step("getUidNext", { user_id: U1, scope: { kind: "mailbox", mailbox: "Starred" } }, "flag.save.starred-next");

step("markMailSpam", { user_id: U1, mail_id: ref("m.text._id"), is_spam: true }, "flag.spam.first");
step("markMailSpam", { user_id: U1, mail_id: ref("m.text._id"), is_spam: true }, "flag.spam.again");
step("markMailSpam", { user_id: U1, mail_id: ref("m.spam._id"), is_spam: false }, "flag.unspam");
step("markMailSpam", { user_id: U2, mail_id: ref("m.spam._id"), is_spam: true }, "flag.spam.other-user");
step("markMailSpam", { user_id: U1, mail_id: UNKNOWN_MAIL, is_spam: true }, "flag.spam.unknown");
step("markMailSpam", { user_id: U1, mail_id: "not-a-uuid", is_spam: true }, "flag.spam.bad-id");
step("getHighestModseq", { user_id: U1, mailbox: "Junk", sent: false }, "flag.spam.junk-modseq");
step("getHighestModseq", { user_id: U1, mailbox: ADMIN_BOX, sent: false }, "flag.spam.box-modseq");

// IMAP STORE, through the statements `buildSetMailFlagsQueries` builds.
const store = (name: string, args: Args) => step("setFlags", { user_id: U1, sent: false, useUid: true, touchesSaved: false, touchesDeleted: false, unchangedSince: null, ...args }, name);
store("store.deleted.range", { mailbox: null, start: 1, end: 4, setClause: "deleted = TRUE", touchesDeleted: true });
store("store.flagged.seq", { mailbox: ADMIN_BOX, start: 2, end: 3, useUid: false, setClause: "saved = TRUE, answered = TRUE", touchesSaved: true });
store("store.replace", {
  mailbox: ADMIN_BOX,
  start: 1,
  end: 1,
  setClause: "\n        read = false,\n        saved = false,\n        deleted = false,\n        draft = false,\n        answered = false\n      ",
  touchesSaved: true,
  touchesDeleted: true,
});
store("store.conditional", { mailbox: null, start: 1, end: 1000, setClause: "answered = TRUE", unchangedSince: ref("after-saves.modseq.domain.received") });
store("store.conditional.zero", { mailbox: null, start: 1, end: 1000, setClause: "read = TRUE", unchangedSince: 0 });
store("store.junk", { mailbox: "Junk", start: 1, end: 1000, setClause: "read = TRUE" });
store("store.drafts.seq", { mailbox: "drafts", start: 1, end: 5, useUid: false, setClause: "draft = FALSE" });
store("store.archive.seq", { mailbox: "Archive", start: 1, end: 9, useUid: false, setClause: "deleted = FALSE", touchesDeleted: true });
store("store.sent", { mailbox: null, sent: true, start: 1, end: 9, setClause: "answered = TRUE" });
store("store.sent.box.seq", { mailbox: ADMIN_SENT_BOX, sent: true, start: 1, end: 1, useUid: false, setClause: "deleted = TRUE", touchesDeleted: true });
store("store.noop", { mailbox: SALES_BOX, start: 1, end: 9, setClause: "" });
store("store.nomatch", { mailbox: "Nowhere", start: 1, end: 9, setClause: "read = TRUE" });

step("updateRfc822Size", { user_id: U1, mail_id: ref("m.minimal._id"), rfc822_size: 4096 }, "size.update");
step("updateRfc822Size", { user_id: U2, mail_id: ref("m.full._id"), rfc822_size: 1 }, "size.other-user");
step("getMailById", { user_id: U1, mail_id: ref("m.minimal._id") }, "size.read");

peeks(U1, "after-flags");

// --- pivots and mappings, called directly ------------------------------------------

step("syncMailboxPivot", { user_id: U1, mailbox: "Trash", mail_id: ref("m.text._id"), isPresent: true }, "pivot.trash.add");
step("syncMailboxPivot", { user_id: U1, mailbox: "Trash", mail_id: ref("m.text._id"), isPresent: true }, "pivot.trash.again");
step("syncMailboxPivot", { user_id: U1, mailbox: "Trash", mail_id: ref("m.text._id"), isPresent: false }, "pivot.trash.remove");
step("syncMailboxPivot", { user_id: U1, mailbox: "Trash", mail_id: ref("m.text._id"), isPresent: false }, "pivot.trash.remove.again");
step("syncMailboxPivot", { user_id: U1, mailbox: "Trash", mail_id: UNKNOWN_MAIL, isPresent: true }, "pivot.unknown-mail");
step("writeMailboxUid", { user_id: U1, mailbox: "Projects", mail_id: ref("m.text._id"), uid: 50 }, "map.write");
step("writeMailboxUid", { user_id: U1, mailbox: "Projects", mail_id: ref("m.text._id"), uid: 51 }, "map.write.again");
step("writeMailboxUid", { user_id: U1, mailbox: "Projects", mail_id: ref("m.minimal._id"), uid: 50 }, "map.write.taken");
step("writeMailboxUid", { user_id: U1, mailbox: "Projects", mail_id: UNKNOWN_MAIL, uid: 60 }, "map.write.unknown-mail");
step("deleteMailboxUid", { user_id: U1, mailbox: "Projects", mail_id: ref("m.text._id") }, "map.delete");
step("deleteMailboxUid", { user_id: U1, mailbox: "Projects", mail_id: ref("m.text._id") }, "map.delete.again");
step(
  "recordMappings",
  {
    user_id: U1,
    mail_id: ref("m.text._id"),
    writes: [
      { mailbox: "Projects", uid: 70, scope: "mailbox" },
      { mailbox: "INBOX", uid: ref("r2.d"), scope: "domain" },
    ],
    mappedDestination: "Projects",
  },
  "map.record.domain-taken"
);
step(
  "recordMappings",
  { user_id: U1, mail_id: ref("m.spam._id"), writes: [{ mailbox: "Projects", uid: 70, scope: "mailbox" }], mappedDestination: "Projects" },
  "map.record.mailbox-taken"
);
step("syncMappedPivotsForRow", { user_id: U1, mail_id: ref("m.spam._id"), saved: true, deleted: true, destMailbox: "Starred" }, "pivot.row.skip-starred");
step("syncMappedPivotsForRow", { user_id: U1, mail_id: ref("m.spam._id"), saved: false, deleted: false, destMailbox: null }, "pivot.row.clear");

// --- delete --------------------------------------------------------------------------

step("deleteMail", { user_id: U2, mail_id: ref("m.empties._id") }, "delete.other-user");
step("deleteMail", { user_id: U1, mail_id: ref("m.empties._id") }, "delete.first");
step("deleteMail", { user_id: U1, mail_id: ref("m.empties._id") }, "delete.again");
// The highest UID in every scope it touched: UIDNEXT must not come back down.
step("deleteMail", { user_id: U1, mail_id: ref("m.copy.trash._id") }, "delete.highest");
step("deleteMail", { user_id: U1, mail_id: "not-a-uuid" }, "delete.bad-id");
step("getMailById", { user_id: U1, mail_id: ref("m.empties._id") }, "delete.read");

peeks(U1, "after-delete");

// --- a user whose rows predate the counters ----------------------------------------

for (const [mail_id, message_id, uid_domain, sent, modseq] of [
  [LEGACY_1, "<legacy-1@old>", 40, false, 1],
  [LEGACY_2, "<legacy-2@old>", 41, false, 6],
  [LEGACY_SENT, "<legacy-sent@old>", 7, true, 3],
] as [string, string, number, boolean, number][]) {
  step("sql", {
    sql: "INSERT INTO mails (mail_id, user_id, message_id, date, uid_domain, sent, modseq) VALUES ($1, $2, $3, $4, $5, $6, $7)",
    values: [mail_id, U3, message_id, DATE, uid_domain, sent, modseq],
  });
}
for (const [mailbox, mail_id, uid] of [
  ["INBOX/accounts/old", LEGACY_1, 12],
  ["old", LEGACY_2, 15],
  ["Starred", LEGACY_1, 3],
  ["Sent Messages/accounts/old", LEGACY_SENT, 5],
] as [string, string, number][]) {
  step("sql", { sql: "INSERT INTO mail_mailbox_uid (user_id, mailbox, mail_id, uid) VALUES ($1, $2, $3, $4)", values: [U3, mailbox, mail_id, uid] });
}
const OLD = "old@legacy.example.com";
const legacyPeeks = (label: string) => {
  step("getUidNext", { user_id: U3, scope: { kind: "domain", sent: false } }, `${label}.domain`);
  step("getUidNext", { user_id: U3, scope: { kind: "domain", sent: true } }, `${label}.domain.sent`);
  step("getUidNext", { user_id: U3, scope: { kind: "account", account: OLD, sent: false } }, `${label}.account`);
  step("getUidNext", { user_id: U3, scope: { kind: "account", account: OLD, sent: true } }, `${label}.account.sent`);
  step("getUidNext", { user_id: U3, scope: { kind: "account", account: "new@legacy.example.com", sent: false } }, `${label}.account.new`);
  step("getUidNext", { user_id: U3, scope: { kind: "mailbox", mailbox: "Starred" } }, `${label}.starred`);
  step("getUidNext", { user_id: U3, scope: { kind: "mailbox", mailbox: "Trash" } }, `${label}.trash`);
  step("getHighestModseq", { user_id: U3, mailbox: null, sent: false }, `${label}.modseq`);
  step("getHighestModseq", { user_id: U3, mailbox: null, sent: true }, `${label}.modseq.sent`);
  step("getHighestModseq", { user_id: U3, mailbox: "INBOX/accounts/old", sent: false }, `${label}.modseq.box`);
};
legacyPeeks("legacy.peek");
step("getDomainUidNext", { user_id: U3, sent: false }, "legacy.domain");
step("getDomainUidNext", { user_id: U3, sent: true }, "legacy.domain.sent");
step("getAccountUidNext", { user_id: U3, account: OLD, sent: false }, "legacy.account");
step("getAccountUidNext", { user_id: U3, account: OLD, sent: true }, "legacy.account.sent");
step("getMailboxUidNext", { user_id: U3, mailbox: "Starred" }, "legacy.starred");
step("getNextModseq", { user_id: U3 }, "legacy.modseq.reserve");
step("markMailSaved", { user_id: U3, mail_id: LEGACY_2, saved: true }, "legacy.save");
step("markMailRead", { user_id: U3, mail_id: LEGACY_1 }, "legacy.read");
legacyPeeks("legacy.after");

// --- concurrency -----------------------------------------------------------------------

step("concurrentReservations", { user_id: U2, kind: "domain", sent: false, count: 12 }, "conc.domain");
step("concurrentReservations", { user_id: U2, kind: "account", account: "bob@bob.example.com", sent: false, count: 12 }, "conc.account");
step("concurrentReservations", { user_id: U2, kind: "mailbox", mailbox: "Starred", count: 12 }, "conc.mailbox");
step("concurrentReservations", { user_id: U2, kind: "modseq", count: 12 }, "conc.modseq");
step("concurrentSaves", { user_id: U2, account: "bob@bob.example.com", mailbox: "INBOX/accounts/bob", count: 8, saved: true }, "conc.saves");
step("getDomainUidNext", { user_id: U2, sent: false }, "race.d");
step("concurrentDuplicates", { user_id: U2, account: "bob@bob.example.com", mailbox: "INBOX/accounts/bob", count: 6, uid_domain: ref("race.d") }, "conc.duplicates");
step("getUidNext", { user_id: U2, scope: { kind: "domain", sent: false } }, "conc.next.domain");
step("getUidNext", { user_id: U2, scope: { kind: "account", account: "bob@bob.example.com", sent: false } }, "conc.next.account");
step("getUidNext", { user_id: U2, scope: { kind: "mailbox", mailbox: "Starred" } }, "conc.next.starred");
step("getNextModseq", { user_id: U2 }, "conc.next.modseq");

// --- mailboxes -----------------------------------------------------------------------

const create = (name: string, as: string, extra: Args = {}) => step("createMailbox", { user_id: U1, name, ...extra }, as);
create("Projects", "box.projects");
create("projects", "box.projects.case");
create("PROJECTS", "box.projects.upper", { subscribed: false });
create("Receipts 2024", "box.receipts", { subscribed: false });
create("receipts-2023", "box.receipts.old", { subscribed: true });
create("Étoile", "box.etoile");
create("etoile", "box.etoile.plain");
create("日本語", "box.japanese");
create("_drafts-old", "box.underscore");
create("10", "box.ten");
create("9", "box.nine");
create("Team", "box.team", { address: "team@example.com", special_use: "\\Archive" });
create("Projects/Alpha", "box.child", { parent_id: ref("box.projects.mailbox.mailbox_id") });
create("Projects/Beta", "box.child.beta", { parent_id: ref("box.projects.mailbox.mailbox_id"), address: null, special_use: null });
create("Orphan", "box.bad-parent", { parent_id: UNKNOWN_MAIL });
step("createMailbox", { user_id: U2, name: "Projects" }, "box.bob.projects");
step("createMailbox", { user_id: UNKNOWN_MAIL, name: "Nobody" }, "box.no-user");
create("x".repeat(256), "box.too-long");

step("getMailboxesByUser", { user_id: U1 }, "box.list");
step("getMailboxesByUser", { user_id: U3 }, "box.list.empty");
step("getMailboxByName", { user_id: U1, name: "ARCHIVE" }, "box.byname");
step("getMailboxByName", { user_id: U1, name: "étoile" }, "box.byname.accent");
step("getMailboxByName", { user_id: U1, name: "missing" }, "box.byname.missing");
step("getMailboxByName", { user_id: U2, name: "Archive" }, "box.byname.other-user");
step("getMailboxByAddress", { user_id: U1, address: "team@example.com" }, "box.byaddress");
step("getMailboxByAddress", { user_id: U1, address: "TEAM@example.com" }, "box.byaddress.case");
step("getMailboxByAddress", { user_id: U2, address: "team@example.com" }, "box.byaddress.other-user");

step("renameMailbox", { user_id: U1, oldName: "receipts 2024", newName: "Receipts" }, "box.rename");
step("renameMailbox", { user_id: U1, oldName: "Receipts", newName: "Receipts 2025" }, "box.rename.again");
step("renameMailbox", { user_id: U1, oldName: "Receipts 2025", newName: "ARCHIVE" }, "box.rename.taken");
step("renameMailbox", { user_id: U1, oldName: "Receipts 2025", newName: "receipts 2025" }, "box.rename.same-name");
step("renameMailbox", { user_id: U1, oldName: "Nope", newName: "Whatever" }, "box.rename.missing");
step("renameMailbox", { user_id: U1, oldName: "team", newName: "Crew" }, "box.rename.protected");
step("renameMailbox", { user_id: U2, oldName: "Archive", newName: "Stolen" }, "box.rename.other-user");

step("setMailboxSubscribed", { user_id: U1, name: "projects", subscribed: false }, "box.unsubscribe");
step("setMailboxSubscribed", { user_id: U1, name: "projects", subscribed: false }, "box.unsubscribe.again");
step("setMailboxSubscribed", { user_id: U1, name: "PROJECTS/ALPHA", subscribed: false }, "box.unsubscribe.child");
step("setMailboxSubscribed", { user_id: U1, name: "Receipts 2025", subscribed: true }, "box.subscribe");
step("setMailboxSubscribed", { user_id: U1, name: "missing", subscribed: true }, "box.subscribe.missing");

step("deleteMailboxByName", { user_id: U1, name: "TEAM" }, "box.delete.protected");
step("deleteMailboxByName", { user_id: U1, name: "missing" }, "box.delete.missing");
step("deleteMailboxByName", { user_id: U1, name: "nine".replace("nine", "9") }, "box.delete");
step("deleteMailboxByName", { user_id: U1, name: "9" }, "box.delete.again");
step("deleteMailboxByName", { user_id: U2, name: "etoile" }, "box.delete.other-user");
// Deleting a parent takes its children with it.
step("deleteMailboxByName", { user_id: U1, name: "projects" }, "box.delete.parent");
step("getMailboxByName", { user_id: U1, name: "Projects/Alpha" }, "box.delete.child-gone");
step("getMailboxesByUser", { user_id: U1 }, "box.list.after");

// The ceiling: 1000 rows per user.
step("sql", { sql: "INSERT INTO mailboxes (user_id, name) SELECT $1::uuid, 'bulk-' || n FROM generate_series(1, 998) AS n", values: [U4] });
step("createMailbox", { user_id: U4, name: "Almost" }, "limit.999");
step("createMailbox", { user_id: U4, name: "Last" }, "limit.1000");
step("createMailbox", { user_id: U4, name: "One too many" }, "limit.refused");
step("createMailbox", { user_id: U4, name: "LAST" }, "limit.exists");
step("createMailbox", { user_id: U2, name: "Unaffected" }, "limit.other-user");
step("deleteMailboxByName", { user_id: U4, name: "bulk-1" }, "limit.make-room");
step("createMailbox", { user_id: U4, name: "One too many" }, "limit.room");
step("createMailbox", { user_id: U4, name: "Again too many" }, "limit.refused.again");

// --- NUL characters -----------------------------------------------------------------

// PostgreSQL text holds no NUL and JSONB refuses `\u0000`, so the save takes
// the character out of everything it writes, the Message-ID included: a
// second delivery spelled with a NUL elsewhere merges into the first.
reserve("nul", U4, ADMIN);
step(
  "saveMail",
  {
    user_id: U4,
    message_id: "<nul\u0000@example.com>",
    in_reply_to: "<pa\u0000rent@example.com>",
    reference_ids: ["<ro\u0000ot@example.com>", "<pa\u0000rent@example.com>"],
    subject: "nu\u0000l",
    date: DATE,
    html: "<p>a\u0000b</p>",
    text: "a\u0000b\u0000",
    from_address: [{ address: "sender@example.com", name: "Jo\u0000e" }],
    from_text: "Jo\u0000e <sender@example.com>",
    to_address: [{ address: ADMIN, name: "" }],
    to_text: ADMIN,
    reply_to_address: [{ address: "reply@example.com", name: "Re\u0000ply" }],
    reply_to_text: "Re\u0000ply <reply@example.com>",
    envelope_from: [{ address: "bounce@example.com" }],
    envelope_to: [{ address: ADMIN }],
    attachments: [{ content: { data: ATTACHMENT_ON_DISK }, contentType: "application/pdf", filename: "re\u0000port.pdf", size: 1234 }],
    insight: { summary: "nu\u0000l", tags: ["a\u0000"] },
    uid_domain: ref("nul.d"),
    uid_mailbox: ref("nul.a"),
    mailbox: ADMIN_BOX,
    domain_mailbox: "INBOX",
  },
  "m.nul"
);
reserve("nul.again", U4, SALES);
step(
  "saveMail",
  {
    user_id: U4,
    message_id: "<n\u0000ul@example.com>",
    date: DATE,
    envelope_to: [{ address: SALES }],
    uid_domain: ref("nul.again.d"),
    uid_mailbox: ref("nul.again.a"),
    mailbox: SALES_BOX,
    domain_mailbox: "INBOX",
  },
  "m.nul.again"
);
step("getMailByMessageId", { user_id: U4, message_id: "<nul@example.com>" }, "read.nul");

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

const connection = {
  host: process.env.POSTGRES_HOST || "127.0.0.1",
  port: process.env.POSTGRES_PORT || "5432",
  user: process.env.POSTGRES_USER || "postgres",
  password: process.env.POSTGRES_PASSWORD ?? "postgres",
};
const database = `mail_core_ts_${process.pid}`;
Object.assign(process.env, {
  POSTGRES_HOST: connection.host,
  POSTGRES_PORT: connection.port,
  POSTGRES_USER: connection.user,
  POSTGRES_PASSWORD: connection.password,
  POSTGRES_DATABASE: database,
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
const workDirectory = fs.mkdtempSync(path.join(fs.realpathSync(os.tmpdir()), "mail-scenario-"));
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

// The repositories read `POSTGRES_*` when they are first imported.
const { pool } = await import("../../../../server/lib/postgres/client");
const { mailsTable } = await import("../../../../server/lib/postgres/models");
const core = await import("../../../../server/lib/postgres/repositories/mails/core");
const counters = await import("../../../../server/lib/postgres/repositories/mails/counters");
const { buildSetMailFlagsQueries } = await import("../../../../server/lib/postgres/repositories/mails/set-flags-query");
const mailboxes = await import("../../../../server/lib/postgres/repositories/mailboxes");

// The repositories log the faults they report; the dump records them.
const say = (line: string) => process.stdout.write(`${line}\n`);
console.error = () => undefined;
console.warn = () => undefined;
console.log = () => undefined;
console.info = () => undefined;

const schema = JSON.parse(fs.readFileSync(path.join(here, "../../../schema.json"), "utf8")) as {
  tables: { createSql: string }[];
  searchVectorDdl: string[];
  indexes: { sql: string }[];
};
for (const table of schema.tables) await pool.query(table.createSql);
for (const sql of schema.searchVectorDdl) await pool.query(sql);
for (const index of schema.indexes) await pool.query(index.sql);

// --- labels for generated ids ------------------------------------------------------

const labels = new Map<string, string>();
/** Mails stored without a date carry the time of the save. */
const savedNow = new Set<string>();

const refreshLabels = async () => {
  const mails = await pool.query("SELECT mail_id, user_id, message_id FROM mails");
  for (const row of mails.rows) {
    if (!labels.has(row.mail_id)) labels.set(row.mail_id, `mail(${row.user_id} ${row.message_id})`);
  }
  const boxes = await pool.query("SELECT mailbox_id, user_id, name FROM mailboxes WHERE name NOT LIKE 'bulk-%'");
  for (const row of boxes.rows) {
    if (!labels.has(row.mailbox_id)) labels.set(row.mailbox_id, `mailbox(${row.user_id} ${row.name})`);
  }
};

const isRaced = (messageId: unknown) => typeof messageId === "string" && /^<(conc|race)-/.test(messageId);
const isMerged = (messageId: unknown) => typeof messageId === "string" && messageId.startsWith("<race-");

const RECENT_MS = 10 * 60 * 1000;

/** Replaces what differs from run to run by what it stands for. */
const normalize = (value: unknown): Json => {
  if (value === undefined || value === null) return null;
  if (typeof value === "string") return labels.get(value) ?? value;
  if (Array.isArray(value)) return value.map(normalize);
  if (typeof value === "object") {
    const object = value as Record<string, unknown>;
    const out: Record<string, Json> = {};
    for (const [key, entry] of Object.entries(object)) {
      if (entry !== undefined) out[key] = normalize(entry);
    }
    if (typeof object.mail_id === "string" && typeof object.date === "string" && savedNow.has(object.mail_id)) {
      const age = Math.abs(Date.now() - Date.parse(object.date));
      out.date = age < RECENT_MS ? "<now>" : `stale: ${object.date}`;
    }
    if (isRaced(object.message_id)) {
      for (const key of ["uid_domain", "modseq"]) if (key in out) out[key] = "<conc>";
      // Overlapping merges each rewrite the envelope from the row they read.
      if (isMerged(object.message_id) && "envelope_to" in out) out.envelope_to = "<conc>";
    }
    return out;
  }
  return value as Json;
};

// --- running a step ------------------------------------------------------------------

const results: Record<string, Json> = {};

const lookup = (pathInResults: string): Json => {
  const alias = Object.keys(results)
    .filter((name) => pathInResults === name || pathInResults.startsWith(`${name}.`))
    .sort((a, b) => b.length - a.length)[0];
  if (alias === undefined) throw new Error(`no result named in ${pathInResults}`);
  let value: Json = results[alias];
  for (const key of pathInResults.slice(alias.length).split(".").filter(Boolean)) {
    value = (value as Record<string, Json>)?.[key];
  }
  if (value === undefined) throw new Error(`${pathInResults} is not in the results`);
  return value;
};

const resolve = (value: Json): Json => {
  if (value === null || typeof value !== "object") return value;
  if (Array.isArray(value)) return value.map(resolve);
  if (typeof value.$ref === "string") return lookup(value.$ref);
  return Object.fromEntries(Object.entries(value).map(([key, entry]) => [key, resolve(entry)]));
};

const flagRow = (row: Record<string, unknown>) => ({
  mail_id: row.mail_id as string,
  uid: Number(row.uid),
  read: row.read as boolean,
  saved: row.saved as boolean,
  deleted: row.deleted as boolean,
  draft: row.draft as boolean,
  answered: row.answered as boolean,
  modseq: Number(row.modseq),
});
/** By UID; two rows of a domain view can share one, so then by label. */
const byUid = (a: { uid: number; mail_id: string }, b: { uid: number; mail_id: string }) => {
  if (a.uid !== b.uid) return a.uid - b.uid;
  const [left, right] = [labels.get(a.mail_id) ?? a.mail_id, labels.get(b.mail_id) ?? b.mail_id];
  return left < right ? -1 : left > right ? 1 : 0;
};
const ascending = (a: number, b: number) => a - b;

const reserveUid = (a: Args): Promise<number> => {
  switch (a.kind) {
    case "domain":
      return counters.getDomainUidNext(a.user_id as string, a.sent as boolean);
    case "account":
      return counters.getAccountUidNext(a.user_id as string, a.account as string, a.sent as boolean);
    case "mailbox":
      return counters.getMailboxUidNext(a.user_id as string, a.mailbox as string);
    case "modseq":
      return counters.getNextModseq(a.user_id as string);
    default:
      throw new Error(`no counter ${a.kind}`);
  }
};

const modseqCounter = async (user_id: string): Promise<number> => {
  const result = await pool.query(
    "SELECT last_uid FROM mail_uid_counters WHERE user_id = $1 AND uid_kind = 'modseq'",
    [user_id]
  );
  return Number(result.rows[0]?.last_uid ?? 0);
};

const ops: Record<string, (a: Args) => Promise<unknown>> = {
  sql: async (a) => ({ rowCount: (await pool.query(a.sql as string, a.values as unknown[])).rowCount }),
  writeAttachment: async (a) => {
    fs.mkdirSync("attachments", { recursive: true });
    fs.writeFileSync(path.join("attachments", a.id as string), Buffer.alloc(a.bytes as number, 0x61));
    return null;
  },

  getDomainUidNext: (a) => counters.getDomainUidNext(a.user_id as string, a.sent as boolean),
  getAccountUidNext: (a) => counters.getAccountUidNext(a.user_id as string, a.account as string, a.sent as boolean),
  getMailboxUidNext: (a) => counters.getMailboxUidNext(a.user_id as string, a.mailbox as string),
  getUidNext: (a) => counters.getUidNext(a.user_id as string, a.scope as never),
  getNextModseq: (a) => counters.getNextModseq(a.user_id as string),
  getHighestModseq: (a) => counters.getHighestModseq(a.user_id as string, a.mailbox as string | null, a.sent as boolean),
  deleteMailboxUid: (a) => counters.deleteMailboxUid(a.user_id as string, a.mailbox as string, a.mail_id as string),
  syncMailboxPivot: (a) =>
    counters.syncMailboxPivot(a.user_id as string, a.mailbox as string, a.mail_id as string, a.isPresent as boolean),
  writeMailboxUid: (a) =>
    counters.writeMailboxUid(a.user_id as string, a.mailbox as string, a.mail_id as string, a.uid as number),

  saveMail: async (a) => {
    const saved = await core.saveMail(a as never);
    await refreshLabels();
    if (saved && a.date === undefined) savedNow.add(saved._id);
    return saved;
  },
  getMailById: async (a) => (await core.getMailById(a.user_id as string, a.mail_id as string))?.toJSON(),
  getMailByMessageId: async (a) =>
    (await core.getMailByMessageId(a.user_id as string, a.message_id as string))?.toJSON(),
  findMailByAttachment: async (a) =>
    (
      await mailsTable.queryOne({
        user_id: a.user_id as string,
        attachments: [{ content: { data: a.attachment_id as string } }],
      })
    )?.toJSON(),
  updateRfc822Size: (a) => core.updateRfc822Size(a.user_id as string, a.mail_id as string, a.rfc822_size as number),
  markMailRead: (a) => core.markMailRead(a.user_id as string, a.mail_id as string),
  markMailSaved: (a) => core.markMailSaved(a.user_id as string, a.mail_id as string, a.saved as boolean),
  markMailSpam: (a) => core.markMailSpam(a.user_id as string, a.mail_id as string, a.is_spam as boolean),
  deleteMail: (a) => core.deleteMail(a.user_id as string, a.mail_id as string),
  recordMappings: (a) =>
    core.recordMappings(
      a.user_id as string,
      a.mail_id as string,
      a.writes as never,
      (a.mappedDestination ?? undefined) as string | undefined
    ),
  syncMappedPivotsForRow: (a) =>
    core.syncMappedPivotsForRow(
      a.user_id as string,
      a.mail_id as string,
      (a.saved ?? undefined) as boolean | undefined,
      (a.deleted ?? undefined) as boolean | undefined,
      (a.destMailbox ?? undefined) as string | undefined
    ),

  // An IMAP STORE as `setMailFlags` and its caller run it: the statements the
  // builder returns, one reserved mod-sequence, then the Starred / Trash
  // pivots of every row the STORE touched.
  setFlags: async (a) => {
    const user_id = a.user_id as string;
    const conditional = a.unchangedSince !== null;
    const queries = buildSetMailFlagsQueries(
      user_id,
      a.mailbox as string | null,
      a.sent as boolean,
      a.start as number,
      a.end as number,
      a.useUid as boolean,
      a.setClause as string,
      conditional
    );
    if (!a.setClause) {
      const selected = await pool.query(queries.selectSql, queries.baseValues);
      return { selected: selected.rows.map(flagRow).sort(byUid) };
    }
    const modseq = await counters.getNextModseq(user_id);
    const matched = conditional
      ? (await pool.query(queries.matchedUidSql, queries.baseValues)).rows.map((row) => Number(row.uid)).sort(ascending)
      : null;
    const result = await pool.query(queries.updateSql, [
      ...queries.baseValues,
      modseq,
      ...(conditional ? [a.unchangedSince] : []),
    ]);
    const updated = result.rows.map(flagRow).sort(byUid);
    for (const row of updated) {
      if (a.touchesSaved) await counters.syncMailboxPivot(user_id, "Starred", row.mail_id, row.saved);
      if (a.touchesDeleted) await counters.syncMailboxPivot(user_id, "Trash", row.mail_id, row.deleted);
    }
    return { modseq, matched, updated };
  },

  concurrentReservations: async (a) => {
    const reserved = await Promise.all(Array.from({ length: a.count as number }, () => reserveUid(a)));
    return reserved.sort(ascending);
  },

  // Deliveries of different mails to one account, all at once: each reserves
  // its UIDs and saves, as concurrent SMTP sessions do.
  concurrentSaves: async (a) => {
    const user_id = a.user_id as string;
    const saved = await Promise.all(
      Array.from({ length: a.count as number }, async (_, index) => {
        const [uid_domain, uid_mailbox] = await Promise.all([
          counters.getDomainUidNext(user_id, false),
          counters.getAccountUidNext(user_id, a.account as string, false),
        ]);
        return core.saveMail({
          user_id,
          message_id: `<conc-${index}@example.com>`,
          date: DATE,
          subject: "Concurrent delivery",
          text: "one of several",
          envelope_to: [{ address: a.account as string }],
          saved: a.saved as boolean,
          uid_domain,
          uid_mailbox,
          mailbox: a.mailbox as string,
          domain_mailbox: "INBOX",
        });
      })
    );
    await refreshLabels();
    const rows = await pool.query(
      "SELECT modseq FROM mails WHERE user_id = $1 AND message_id LIKE '<conc-%' ORDER BY modseq",
      [user_id]
    );
    const starred = await pool.query(
      "SELECT x.uid FROM mail_mailbox_uid x JOIN mails m ON m.mail_id = x.mail_id WHERE x.user_id = $1 AND x.mailbox = 'Starred' AND m.message_id LIKE '<conc-%' ORDER BY x.uid",
      [user_id]
    );
    return {
      stored: new Set(saved.map((result) => result?._id)).size,
      uid_domain: saved.map((result) => result?.uid_domain ?? -1).sort(ascending),
      uid_mailbox: saved.map((result) => result?.uid_mailbox ?? -1).sort(ascending),
      modseq: rows.rows.map((row) => Number(row.modseq)),
      starred: starred.rows.map((row) => Number(row.uid)),
    };
  },

  // One Message-ID delivered several times at once: exactly one INSERT wins,
  // the others merge into its row.
  concurrentDuplicates: async (a) => {
    const user_id = a.user_id as string;
    const before = await modseqCounter(user_id);
    const saved = await Promise.all(
      Array.from({ length: a.count as number }, async (_, index) => {
        const uid_mailbox = await counters.getAccountUidNext(user_id, a.account as string, false);
        const result = await core.saveMail({
          user_id,
          message_id: "<race-1@example.com>",
          date: DATE,
          subject: "Raced delivery",
          text: "the same message",
          envelope_to: [{ address: `rcpt-${index}@example.com` }],
          uid_domain: a.uid_domain as number,
          uid_mailbox,
          mailbox: a.mailbox as string,
          domain_mailbox: "INBOX",
        });
        return { reserved: uid_mailbox, result };
      })
    );
    await refreshLabels();
    const after = await modseqCounter(user_id);
    const row = (
      await pool.query(
        "SELECT modseq, uid_domain, jsonb_array_length(envelope_to) AS recipients FROM mails WHERE user_id = $1 AND message_id = '<race-1@example.com>'",
        [user_id]
      )
    ).rows[0];
    const persisted = [...new Set(saved.map(({ result }) => result?.uid_mailbox ?? -1))];
    return {
      rows: new Set(saved.map(({ result }) => result?._id)).size,
      uid_domain: [...new Set(saved.map(({ result }) => result?.uid_domain ?? -1))],
      everyone_reports_one_mailbox_uid: persisted.length === 1,
      that_uid_was_reserved_by_one_of_them: saved.some(({ reserved }) => reserved === persisted[0]),
      modseq_reserved_in_this_batch: Number(row.modseq) > before && Number(row.modseq) <= after,
      modseqs_reserved: after - before,
      stored_uid_domain: Number(row.uid_domain),
      // A merge reads the row, then rewrites its envelope: merges that
      // overlap can each keep only their own recipient.
      recipients_within_bounds: Number(row.recipients) >= 1 && Number(row.recipients) <= (a.count as number),
    };
  },

  getMailboxesByUser: async (a) =>
    (await mailboxes.getMailboxesByUser(a.user_id as string)).map((mailbox) => mailbox.toJSON()),
  getMailboxByName: async (a) => (await mailboxes.getMailboxByName(a.user_id as string, a.name as string))?.toJSON(),
  getMailboxByAddress: async (a) =>
    (await mailboxes.getMailboxByAddress(a.user_id as string, a.address as string))?.toJSON(),
  createMailbox: async (a) => {
    const result = await mailboxes.createMailbox(a as never);
    await refreshLabels();
    return result.status === "created" ? { status: result.status, mailbox: result.mailbox.toJSON() } : result;
  },
  deleteMailboxByName: (a) => mailboxes.deleteMailboxByName(a.user_id as string, a.name as string),
  renameMailbox: (a) => mailboxes.renameMailbox(a.user_id as string, a.oldName as string, a.newName as string),
  setMailboxSubscribed: (a) =>
    mailboxes.setMailboxSubscribed(a.user_id as string, a.name as string, a.subscribed as boolean),
};

const VOLATILE_KEYS = ["created"];
const strip = (value: Json): Json => {
  if (value === null || typeof value !== "object") return value;
  if (Array.isArray(value)) return value.map(strip);
  return Object.fromEntries(
    Object.entries(value)
      .filter(([key]) => !VOLATILE_KEYS.includes(key))
      .map(([key, entry]) => [key, strip(entry)])
  );
};

const recorded: Json[] = [];
for (const [index, { op, args, as }] of steps.entries()) {
  const run = ops[op];
  if (!run) throw new Error(`no op ${op}`);
  let outcome: Json;
  try {
    const returned = await run(resolve(args) as Args);
    // Later steps refer to what this one returned as it was returned; the
    // record holds the normalized form.
    if (as) results[as] = returned === undefined ? null : (JSON.parse(JSON.stringify(returned)) as Json);
    outcome = { ok: strip(normalize(returned)) };
  } catch (error) {
    if (as) results[as] = null;
    const thrown = error as { code?: string; constraint?: string; message?: string; name?: string };
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
  recorded.push({ step: index + 1, op, ...(as ? { as } : {}), ...(outcome as object) } as Json);
}

// --- the tables ----------------------------------------------------------------------

const MAIL_COLUMNS =
  "mail_id, user_id, message_id, subject, date, html, text, from_address, from_text, to_address, to_text, cc_address, cc_text, bcc_address, bcc_text, reply_to_address, reply_to_text, envelope_from, envelope_to, attachments, read, saved, sent, deleted, draft, answered, expunged, insight, uid_domain, modseq, spam_score, spam_reasons, is_spam, rfc822_size, text_line_count, html_line_count, in_reply_to, reference_ids, search_vector::text AS search_vector";

await refreshLabels();
const mails = (await pool.query(`SELECT ${MAIL_COLUMNS} FROM mails ORDER BY user_id, message_id COLLATE "C"`)).rows;
const raced = new Set(mails.filter((row) => isRaced(row.message_id)).map((row) => row.mail_id as string));
const mappings = (
  await pool.query(`SELECT user_id, mailbox, mail_id, uid FROM mail_mailbox_uid ORDER BY user_id, mailbox COLLATE "C", uid`)
).rows.map((row) => ({ ...row, uid: raced.has(row.mail_id) ? "<conc>" : row.uid }));
const counterRows = (
  await pool.query(
    `SELECT user_id, uid_kind, uid_scope, sent, last_uid FROM mail_uid_counters ORDER BY user_id, uid_kind, uid_scope COLLATE "C", sent`
  )
).rows;
const boxes = (
  await pool.query(
    `SELECT mailbox_id, user_id, name, address, parent_id, uid_validity, uid_next, subscribed, special_use FROM mailboxes WHERE name NOT LIKE 'bulk-%' ORDER BY user_id, name COLLATE "C"`
  )
).rows;
const bulk = (await pool.query("SELECT user_id, COUNT(*)::int AS count FROM mailboxes GROUP BY user_id ORDER BY user_id")).rows;

const sortKey = (row: Json) => JSON.stringify(row);
const tables = {
  mails: mails.map(normalize),
  mail_mailbox_uid: mappings.map(normalize).sort((a, b) => (sortKey(a) < sortKey(b) ? -1 : sortKey(a) > sortKey(b) ? 1 : 0)),
  mail_uid_counters: counterRows.map(normalize),
  mailboxes: boxes.map(normalize),
  mailbox_counts: bulk.map(normalize),
};

// ---------------------------------------------------------------------------

const write = (name: string, value: unknown) => fs.writeFileSync(path.join(here, name), `${JSON.stringify(value, null, 1)}\n`);
write("scenario.steps.json", { generatedBy: "src/server-rs/tests/golden/storage/scenario.ts", steps });
write("scenario.expected.json", {
  generatedBy: "src/server-rs/tests/golden/storage/scenario.ts",
  results: recorded,
  tables,
});

await pool.end();
await cleanUp();

const failures = recorded.filter((entry) => "threw" in (entry as object)).length;
say(`${steps.length} steps (${failures} report a failure), ${mails.length} mails, ${mappings.length} mapping rows, ${counterRows.length} counters, ${boxes.length} mailboxes`);
process.exit(0);
