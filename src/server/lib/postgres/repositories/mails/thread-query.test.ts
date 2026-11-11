import { describe, it, expect } from "bun:test";
import {
  buildThreadCandidatesQuery,
  buildThreadSeedQuery,
  buildThreadSubjectQuery,
  HEADERLESS_SQL,
  NORMALIZED_SUBJECT_SQL,
  THREAD_CANDIDATE_LIMIT,
  THREAD_SUBJECT_LIMIT
} from "./thread";
import { indexSpecs } from "../../initialize";

describe("buildThreadSeedQuery", () => {
  it("scopes the seed to the user and leaves out expunged mail", () => {
    const { sql, values } = buildThreadSeedQuery("u1", "m1");
    expect(values).toEqual(["u1", "m1"]);
    expect(sql).toContain("user_id = $1 AND mail_id = $2");
    expect(sql).toContain("expunged = FALSE");
  });
});

describe("buildThreadCandidatesQuery", () => {
  it("matches ids against message_id, In-Reply-To and References only", () => {
    const { sql, values } = buildThreadCandidatesQuery("u1", ["<a@x>", "a@x"], false);
    expect(values).toEqual(["u1", ["<a@x>", "a@x"]]);
    expect(sql).toContain("message_id = ANY($2::text[])");
    expect(sql).toContain("in_reply_to = ANY($2::text[])");
    expect(sql).toContain("reference_ids ?| $2::text[]");
    expect(sql).not.toContain("regexp_replace");
    expect(sql).toContain(`LIMIT ${THREAD_CANDIDATE_LIMIT}`);
  });

  it("keeps spam and drafts out unless spam is asked for", () => {
    const ham = buildThreadCandidatesQuery("u1", ["<a@x>"], false);
    expect(ham.sql).toContain("is_spam = FALSE");
    expect(ham.sql).toContain("draft = FALSE");
    const spam = buildThreadCandidatesQuery("u1", ["<a@x>"], true);
    expect(spam.sql).not.toContain("is_spam = FALSE");
  });
});

describe("buildThreadSubjectQuery", () => {
  it("looks up header-less mail by normalized subject", () => {
    const { sql, values } = buildThreadSubjectQuery("u1", "plan", false);
    expect(values).toEqual(["u1", "plan"]);
    expect(sql).toContain(HEADERLESS_SQL);
    expect(sql).toContain(`${NORMALIZED_SUBJECT_SQL} = $2`);
    expect(sql).toContain(`LIMIT ${THREAD_SUBJECT_LIMIT}`);
  });

  // The planner only uses the expression index when the query repeats its
  // expression and predicate exactly.
  it("repeats the expression and predicate of its index", () => {
    const spec = indexSpecs().find((s) => s.name === "idx_mails_thread_subject");
    expect(spec?.sql).toContain(`(user_id, (${NORMALIZED_SUBJECT_SQL}))`);
    expect(spec?.sql).toContain(`WHERE ${HEADERLESS_SQL}`);
    expect(spec?.sql).toContain("CONCURRENTLY");
  });
});
