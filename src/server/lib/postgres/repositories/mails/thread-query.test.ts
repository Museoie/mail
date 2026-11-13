import { describe, it, expect } from "bun:test";
import {
  buildThreadCandidatesQuery,
  buildThreadSeedQuery,
  buildThreadSubjectQuery,
  COPY_MESSAGE_ID_SUFFIX,
  HEADERLESS_SQL,
  NORMALIZED_SUBJECT_SQL,
  SUBJECT_KEY_SQL,
  THREAD_CANDIDATE_LIMIT,
  THREAD_SUBJECT_LIMIT
} from "./thread";
import { indexSpecs } from "../../initialize";
import { deriveCopyMessageId } from "../../../imap/util";

describe("buildThreadSeedQuery", () => {
  it("scopes the seed to the user and leaves out expunged mail", () => {
    const { sql, values } = buildThreadSeedQuery("u1", "m1");
    expect(values).toEqual(["u1", "m1"]);
    expect(sql).toContain("user_id = $1 AND mail_id = $2");
    expect(sql).toContain("expunged = FALSE");
  });

  it("selects envelope_to so a thread row carries its account", () => {
    // A thread opened from an envelope-only (BCC) delivery resolves its
    // account from the envelope recipient, like the list rows do.
    expect(buildThreadSeedQuery("u1", "m1").sql).toContain("envelope_to");
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
  it("looks up header-less mail by the seed's normalized subject, computed in SQL", () => {
    const { sql, values } = buildThreadSubjectQuery("u1", "seed-1", false);
    expect(values).toEqual(["u1", "seed-1"]);
    expect(sql).toContain(HEADERLESS_SQL);
    expect(sql).toContain("WHERE user_id = $1 AND mail_id = $2");
    expect(sql).toContain(`${SUBJECT_KEY_SQL} = md5((SELECT subject FROM seed))`);
    expect(sql).toContain(`${NORMALIZED_SUBJECT_SQL} = (SELECT subject FROM seed)`);
    expect(sql).toContain(`LIMIT ${THREAD_SUBJECT_LIMIT}`);
  });

  it("leaves IMAP copies out of both lookups", () => {
    const like = `message_id NOT LIKE '%${COPY_MESSAGE_ID_SUFFIX}'`;
    expect(buildThreadSubjectQuery("u1", "s", false).sql).toContain(like);
    expect(buildThreadCandidatesQuery("u1", ["<a@x>"], false).sql).toContain(like);
  });

  it("matches the suffix IMAP COPY gives a copy", () => {
    expect(deriveCopyMessageId("<a@x>", "Archive").endsWith(COPY_MESSAGE_ID_SUFFIX)).toBe(
      true
    );
  });

  // The planner only uses the expression index when the query repeats its
  // expression and predicate exactly. The key is a digest, so a subject of
  // any length fits a btree entry.
  it("repeats the digest key and predicate of its index", () => {
    const spec = indexSpecs().find((s) => s.name === "idx_mails_thread_subject_key");
    expect(SUBJECT_KEY_SQL).toBe(`md5(${NORMALIZED_SUBJECT_SQL})`);
    expect(spec?.sql).toContain(`(user_id, (${SUBJECT_KEY_SQL}))`);
    expect(spec?.sql).toContain(`WHERE ${HEADERLESS_SQL}`);
    expect(spec?.sql).toContain("CONCURRENTLY");
  });
});
