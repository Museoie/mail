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

/** A repository over a fixed set of rows that answers like the SQL does. */
const repositoryOver = (rows: ThreadRow[]) => {
  const calls: { ids: string[]; subject: string | null; spam: boolean }[] = [];
  const repository: ThreadRepository = {
    getThreadSeed: async (_user, mailId) =>
      rows.find((r) => r.mail_id === mailId) ?? null,
    getThreadCandidates: async (_user, ids, subject, includeSpam) => {
      calls.push({ ids: [...ids].sort(), subject, spam: includeSpam });
      return rows.filter(
        (r) =>
          (includeSpam || !r.is_spam) &&
          (ids.includes(r.message_id) ||
            (r.in_reply_to !== null && ids.includes(r.in_reply_to)) ||
            (r.reference_ids ?? []).some((id) => ids.includes(id)))
      );
    }
  };
  return { repository, calls };
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
    const { repository } = repositoryOver([c, a, b, row("z", { subject: "Other" })]);
    const thread = await getThread(USER, "a", repository);
    expect(thread.map((m) => m.id)).toEqual(["a", "b", "c"]);
    expect(thread[1].sent).toBe(true);
    expect(thread[0].snippet).toBe("hello");
  });

  it("follows links found in later rounds", async () => {
    // a ← b ← c, opened from a: the first round only finds b by its
    // In-Reply-To; c names a only through b.
    const a = row("a");
    const b = row("b", { in_reply_to: "<a@x>" });
    const c = row("c", { in_reply_to: "<b@x>" });
    const { repository, calls } = repositoryOver([a, b, c]);
    const thread = await getThread(USER, "a", repository);
    expect(thread.map((m) => m.id).sort()).toEqual(["a", "b", "c"]);
    expect(calls.length).toBeGreaterThan(1);
  });

  it("passes the normalized subject for header-less mail and keeps spam out", async () => {
    const a = row("a", { subject: "Re: Project Plan" });
    const { repository, calls } = repositoryOver([a]);
    await getThread(USER, "a", repository);
    expect(calls[0]).toEqual({
      ids: ["<a@x>"],
      subject: "project plan",
      spam: false
    });
  });

  it("includes spam and flags it when the opened mail is spam", async () => {
    const a = row("a", { is_spam: true });
    const { repository, calls } = repositoryOver([a]);
    const thread = await getThread(USER, "a", repository);
    expect(calls[0].spam).toBe(true);
    expect(thread[0].spam).toBe(true);
  });

  it("is empty for a mail that isn't found", async () => {
    const { repository } = repositoryOver([]);
    expect(await getThread(USER, "missing", repository)).toEqual([]);
  });
});
