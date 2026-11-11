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

/** Candidates fetched per round; a thread is never this long in practice. */
export const THREAD_CANDIDATE_LIMIT = 300;

/**
 * Postgres twin of `normalizeSubject` in mails/threading.ts: the subject with
 * leading reply/forward markers removed, whitespace collapsed, lower-cased.
 */
export const NORMALIZED_SUBJECT_SQL = `lower(btrim(regexp_replace(regexp_replace(subject, '^\\s*((re|fwd?|aw|wg|sv|vs|antw|tr|rv)\\s*(\\[\\d+\\])?\\s*[:：]\\s*)+', '', 'i'), '\\s+', ' ', 'g')))`;

export const buildThreadSeedQuery = (
  user_id: string,
  mail_id: string
): { sql: string; values: ParamValue[] } => ({
  sql: `SELECT ${THREAD_COLUMNS} FROM mails
    WHERE user_id = $1 AND mail_id = $2 AND expunged = FALSE`,
  values: [user_id, mail_id]
});

/**
 * Mails that may share a thread with the ids and subject given: anything
 * whose own id, In-Reply-To or References overlaps `ids`, or (when a subject
 * is given) whose normalized subject equals it. Spam stays out unless the
 * thread being opened is spam itself.
 */
export const buildThreadCandidatesQuery = (
  user_id: string,
  ids: string[],
  normalizedSubject: string | null,
  includeSpam: boolean
): { sql: string; values: ParamValue[] } => {
  const values: ParamValue[] = [user_id, ids];
  let subjectCondition = "";
  if (normalizedSubject) {
    values.push(normalizedSubject);
    subjectCondition = ` OR ${NORMALIZED_SUBJECT_SQL} = $3`;
  }
  const sql = `SELECT ${THREAD_COLUMNS} FROM mails
    WHERE user_id = $1
      AND expunged = FALSE
      AND draft = FALSE
      ${includeSpam ? "" : "AND is_spam = FALSE"}
      AND (
        message_id = ANY($2::text[])
        OR in_reply_to = ANY($2::text[])
        OR (reference_ids IS NOT NULL AND reference_ids ?| $2::text[])
        ${subjectCondition}
      )
    ORDER BY date DESC
    LIMIT ${THREAD_CANDIDATE_LIMIT}`;
  return { sql, values };
};

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
  normalizedSubject: string | null,
  includeSpam: boolean
): Promise<ThreadRow[]> => {
  const { sql, values } = buildThreadCandidatesQuery(
    user_id,
    ids,
    normalizedSubject,
    includeSpam
  );
  const result = await pool.query(sql, values);
  return result.rows as ThreadRow[];
};
