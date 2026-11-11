import { pool } from "../../client";
import { ParamValue } from "../../database";

/**
 * The columns a thread view needs: the header fields of each message, its
 * threading headers, and a bounded body prefix for the collapsed preview.
 * Bodies are fetched per message when one is expanded.
 */
const THREAD_COLUMNS = [
  "mail_id",
  "message_id",
  "in_reply_to",
  "reference_ids",
  "subject",
  "date",
  "from_address",
  "from_text",
  "to_address",
  "to_text",
  "cc_address",
  "cc_text",
  "bcc_address",
  "bcc_text",
  "read",
  "saved",
  "sent",
  "is_spam",
  "insight",
  "left(text, 400) AS text_prefix"
].join(", ");

export interface ThreadRow {
  mail_id: string;
  message_id: string;
  in_reply_to: string | null;
  reference_ids: string[] | null;
  subject: string;
  date: string;
  from_address: object | null;
  from_text: string | null;
  to_address: object | null;
  to_text: string | null;
  cc_address: object | null;
  cc_text: string | null;
  bcc_address: object | null;
  bcc_text: string | null;
  read: boolean;
  saved: boolean;
  sent: boolean;
  is_spam: boolean;
  insight: object | null;
  text_prefix: string | null;
}

/** Rows fetched by header links per round; a thread is never this long. */
export const THREAD_CANDIDATE_LIMIT = 300;

/** Rows fetched by subject; the newest are the likeliest conversation. */
export const THREAD_SUBJECT_LIMIT = 100;

/**
 * Postgres twin of `normalizeSubject` in mails/threading.ts: the subject with
 * leading reply/forward markers removed, whitespace collapsed, lower-cased.
 * The expression index `idx_mails_thread_subject` (postgres/initialize.ts) is
 * built on exactly this text, so the two must stay identical.
 */
export const NORMALIZED_SUBJECT_SQL = `lower(btrim(regexp_replace(regexp_replace(subject, '^\\s*((re|fwd?|aw|wg|sv|vs|antw|tr|rv)\\s*(\\[\\d+\\])?\\s*[:：]\\s*)+', '', 'i'), '\\s+', ' ', 'g')))`;

/** Header-less mail, the only rows the subject match applies to. */
export const HEADERLESS_SQL = "in_reply_to IS NULL AND reference_ids IS NULL";

export const buildThreadSeedQuery = (
  user_id: string,
  mail_id: string
): { sql: string; values: ParamValue[] } => ({
  sql: `SELECT ${THREAD_COLUMNS} FROM mails
    WHERE user_id = $1 AND mail_id = $2 AND expunged = FALSE`,
  values: [user_id, mail_id]
});

/**
 * Mails whose own id, In-Reply-To or References overlaps `ids` (pass both the
 * bracketed and the bare spelling of each id). Spam stays out unless the
 * thread being opened is spam itself.
 */
export const buildThreadCandidatesQuery = (
  user_id: string,
  ids: string[],
  includeSpam: boolean
): { sql: string; values: ParamValue[] } => ({
  sql: `SELECT ${THREAD_COLUMNS} FROM mails
    WHERE user_id = $1
      AND expunged = FALSE
      AND draft = FALSE
      ${includeSpam ? "" : "AND is_spam = FALSE"}
      AND (
        message_id = ANY($2::text[])
        OR in_reply_to = ANY($2::text[])
        OR (reference_ids IS NOT NULL AND reference_ids ?| $2::text[])
      )
    ORDER BY date DESC
    LIMIT ${THREAD_CANDIDATE_LIMIT}`,
  values: [user_id, ids]
});

/**
 * Header-less mails with this normalized subject — the candidates for the
 * subject match. Served by `idx_mails_thread_subject`, whose predicate and
 * expression this query repeats.
 */
export const buildThreadSubjectQuery = (
  user_id: string,
  normalizedSubject: string,
  includeSpam: boolean
): { sql: string; values: ParamValue[] } => ({
  sql: `SELECT ${THREAD_COLUMNS} FROM mails
    WHERE user_id = $1
      AND ${HEADERLESS_SQL}
      AND ${NORMALIZED_SUBJECT_SQL} = $2
      AND expunged = FALSE
      AND draft = FALSE
      ${includeSpam ? "" : "AND is_spam = FALSE"}
    ORDER BY date DESC
    LIMIT ${THREAD_SUBJECT_LIMIT}`,
  values: [user_id, normalizedSubject]
});

/** The References of the user's mail with `message_id`, if it is stored. */
export const getReferenceIdsByMessageId = async (
  user_id: string,
  message_id: string
): Promise<string[] | null> => {
  const result = await pool.query(
    `SELECT reference_ids FROM mails
      WHERE user_id = $1 AND message_id = $2 AND expunged = FALSE
      LIMIT 1`,
    [user_id, message_id]
  );
  const row = result.rows[0] as { reference_ids: string[] | null } | undefined;
  return row?.reference_ids ?? null;
};

export const getThreadSeed = async (
  user_id: string,
  mail_id: string
): Promise<ThreadRow | null> => {
  const { sql, values } = buildThreadSeedQuery(user_id, mail_id);
  const result = await pool.query(sql, values);
  return (result.rows[0] as ThreadRow | undefined) ?? null;
};

export const getThreadCandidates = async (
  user_id: string,
  ids: string[],
  includeSpam: boolean
): Promise<ThreadRow[]> => {
  const { sql, values } = buildThreadCandidatesQuery(user_id, ids, includeSpam);
  const result = await pool.query(sql, values);
  return result.rows as ThreadRow[];
};

export const getThreadSubjectCandidates = async (
  user_id: string,
  normalizedSubject: string,
  includeSpam: boolean
): Promise<ThreadRow[]> => {
  const { sql, values } = buildThreadSubjectQuery(
    user_id,
    normalizedSubject,
    includeSpam
  );
  const result = await pool.query(sql, values);
  return result.rows as ThreadRow[];
};
