import { describe, it, expect } from "bun:test";
import { SignedUser } from "common";
import { getThread, ThreadRepository } from "./thread";
import type { ThreadRow } from "../postgres/repositories/mails/thread";

const USER = new SignedUser({
  id: "11111111-2222-3333-4444-555555555555",
  username: "admin",
  email: "admin@example.com"
});

const row = (id: string, fields: Partial<ThreadRow> = {}): ThreadRow => ({
  mail_id: id,
  message_id: `<${id}@x>`,
  in_reply_to: null,
  reference_ids: null,
  subject: "Plan",
  date: "2026-01-01T00:00:00.000Z",
  from_address: [{ address: "alice@x.com" }],
  from_text: "alice@x.com",
  to_address: [{ address: "me@inbox.test" }],
  to_text: "me@inbox.test",
  cc_address: null,
  cc_text: null,
  bcc_address: null,
  bcc_text: null,
  read: true,
  saved: false,
  sent: false,
  is_spam: false,
  insight: null,
  text_prefix: "hello",
  ...fields
});

const isOwn = (address: string) => address.endsWith("@inbox.test");

const normalized = (subject: string) =>
  subject
    .replace(/^\s*((re|fwd?)\s*:\s*)+/i, "")
    .trim()
    .toLowerCase();

/** A repository over a fixed set of rows that answers like the SQL does. */
const repositoryOver = (rows: ThreadRow[]) => {
  const calls: { ids: string[]; spam: boolean }[] = [];
  const subjectCalls: { subject: string; spam: boolean }[] = [];
  const repository: ThreadRepository = {
    getThreadSeed: async (_user, mailId) =>
      rows.find((r) => r.mail_id === mailId) ?? null,
    getThreadCandidates: async (_user, ids, includeSpam) => {
      calls.push({ ids: [...ids].sort(), spam: includeSpam });
      return rows.filter(
        (r) =>
          (includeSpam || !r.is_spam) &&
          (ids.includes(r.message_id) ||
            (r.in_reply_to !== null && ids.includes(r.in_reply_to)) ||
            (r.reference_ids ?? []).some((id) => ids.includes(id)))
      );
    },
    getThreadSubjectCandidates: async (_user, subject, includeSpam) => {
      subjectCalls.push({ subject, spam: includeSpam });
      return rows.filter(
        (r) =>
          (includeSpam || !r.is_spam) &&
          r.in_reply_to === null &&
          r.reference_ids === null &&
          normalized(r.subject) === subject
      );
    }
  };
  return { repository, calls, subjectCalls };
};

const thread = (mailId: string, rows: ThreadRow[]) => {
  const { repository } = repositoryOver(rows);
  return getThread(USER, mailId, repository, isOwn);
};


describe("getThread", () => {
  it("returns the whole conversation oldest first from any message in it", async () => {
    const a = row("a", { date: "2026-01-01T00:00:00.000Z" });
    const b = row("b", {
      date: "2026-01-02T00:00:00.000Z",
      in_reply_to: "<a@x>",
      reference_ids: ["<a@x>"],
      sent: true
    });
    const c = row("c", {
      date: "2026-01-03T00:00:00.000Z",
      in_reply_to: "<b@x>",
      reference_ids: ["<a@x>", "<b@x>"]
    });
    const result = await thread("a", [c, a, b, row("z", { subject: "Other" })]);
    expect(result.map((m) => m.id)).toEqual(["a", "b", "c"]);
    expect(result[1].sent).toBe(true);
    expect(result[0].snippet).toBe("hello");
  });

  it("follows links found in later rounds", async () => {
    // a ← b ← c, opened from a: c names a only through b.
    const a = row("a");
    const b = row("b", { in_reply_to: "<a@x>" });
    const c = row("c", { in_reply_to: "<b@x>" });
    const { repository, calls } = repositoryOver([a, b, c]);
    const result = await getThread(USER, "a", repository, isOwn);
    expect(result.map((m) => m.id).sort()).toEqual(["a", "b", "c"]);
    expect(calls.length).toBeGreaterThan(1);
  });

  it("asks for both stored spellings of each id", async () => {
    const { repository, calls } = repositoryOver([row("a")]);
    await getThread(USER, "a", repository, isOwn);
    expect(calls[0].ids).toEqual(["<a@x>", "a@x"]);
  });

  it("finds a reply to an original appended over IMAP without brackets", async () => {
    const original = row("a", { message_id: "a@x" });
    const reply = row("b", { in_reply_to: "<a@x>" });
    expect((await thread("b", [original, reply])).map((m) => m.id).sort()).toEqual([
      "a",
      "b"
    ]);
  });

  // Two vendors answer two separate quotes to the same mailbox, with the
  // same subject. Their headers point at different conversations.
  it("keeps apart two conversations that share a subject and a mailbox", async () => {
    const a = row("a", {
      subject: "Re: Invoice",
      in_reply_to: "<q1@inbox.test>",
      from_address: [{ address: "billing@vendor-a.com" }]
    });
    const b = row("b", {
      subject: "RE: invoice",
      in_reply_to: "<q2@inbox.test>",
      from_address: [{ address: "ar@vendor-b.com" }],
      read: false
    });
    expect((await thread("a", [a, b])).map((m) => m.id)).toEqual(["a"]);
  });

  it("keeps apart header-less mail whose only shared address is the user's", async () => {
    const a = row("a", {
      subject: "Re: Invoice",
      from_address: [{ address: "billing@vendor-a.com" }]
    });
    const b = row("b", {
      subject: "Re: Invoice",
      from_address: [{ address: "ar@vendor-b.com" }]
    });
    expect((await thread("a", [a, b])).map((m) => m.id)).toEqual(["a"]);
  });

  it("joins header-less replies with one correspondent by subject", async () => {
    const original = row("a", { subject: "Plan" });
    const reply = row("b", {
      subject: "Re: Plan",
      date: "2026-01-02T00:00:00.000Z",
      from_address: [{ address: "me@inbox.test" }],
      to_address: [{ address: "alice@x.com" }],
      sent: true
    });
    expect((await thread("a", [original, reply])).map((m) => m.id)).toEqual([
      "a",
      "b"
    ]);
  });

  it("looks up by subject once, and only when the thread has header-less mail", async () => {
    const headerOnly = repositoryOver([
      row("a", { in_reply_to: "<q@x>" }),
      row("q", { in_reply_to: "<p@x>" })
    ]);
    await getThread(USER, "a", headerOnly.repository, isOwn);
    expect(headerOnly.subjectCalls).toEqual([]);

    const legacy = repositoryOver([row("a", { subject: "Re: Project Plan" })]);
    await getThread(USER, "a", legacy.repository, isOwn);
    expect(legacy.subjectCalls).toEqual([
      { subject: "project plan", spam: false }
    ]);
  });

  it("includes spam and flags it when the opened mail is spam", async () => {
    const { repository, calls } = repositoryOver([row("a", { is_spam: true })]);
    const result = await getThread(USER, "a", repository, isOwn);
    expect(calls[0].spam).toBe(true);
    expect(result[0].spam).toBe(true);
  });

  it("is empty for a mail that isn't found", async () => {
    expect(await thread("missing", [])).toEqual([]);
  });
});
