import { describe, it, expect } from "bun:test";
import {
  buildThreadCandidatesQuery,
  buildThreadSeedQuery,
  THREAD_CANDIDATE_LIMIT
} from "./thread";

describe("buildThreadSeedQuery", () => {
  it("scopes the seed to the user and leaves out expunged mail", () => {
    const { sql, values } = buildThreadSeedQuery("u1", "m1");
    expect(values).toEqual(["u1", "m1"]);
    expect(sql).toContain("user_id = $1 AND mail_id = $2");
    expect(sql).toContain("expunged = FALSE");
  });
});

describe("buildThreadCandidatesQuery", () => {
  it("matches ids against message_id, In-Reply-To and References", () => {
    const { sql, values } = buildThreadCandidatesQuery("u1", ["<a@x>"], null, false);
    expect(values).toEqual(["u1", ["<a@x>"]]);
    expect(sql).toContain("message_id = ANY($2::text[])");
    expect(sql).toContain("in_reply_to = ANY($2::text[])");
    expect(sql).toContain("reference_ids ?| $2::text[]");
    expect(sql).toContain(`LIMIT ${THREAD_CANDIDATE_LIMIT}`);
  });

  it("adds the subject match only when a subject is given", () => {
    const none = buildThreadCandidatesQuery("u1", ["<a@x>"], null, false);
    expect(none.sql).not.toContain("$3");
    const withSubject = buildThreadCandidatesQuery("u1", ["<a@x>"], "plan", false);
    expect(withSubject.values).toEqual(["u1", ["<a@x>"], "plan"]);
    expect(withSubject.sql).toContain("= $3");
  });

  it("keeps spam and drafts out unless spam is asked for", () => {
    const ham = buildThreadCandidatesQuery("u1", ["<a@x>"], null, false);
    expect(ham.sql).toContain("is_spam = FALSE");
    expect(ham.sql).toContain("draft = FALSE");
    const spam = buildThreadCandidatesQuery("u1", ["<a@x>"], null, true);
    expect(spam.sql).not.toContain("is_spam = FALSE");
  });
});
