/**
 * Which stored mails belong to one conversation.
 *
 * Mail that carries `In-Reply-To`/`References` is threaded by those headers:
 * two mails are in one thread when one's Message-ID appears in the other's
 * headers, directly or through other mails in between. Mail stored before
 * those headers were kept has none, so for it a reply is also matched by
 * subject — the same subject once `Re:`/`Fwd:` prefixes are stripped, at
 * least one of the two marked as a reply or forward, and at least one shared
 * participant. Two unrelated newsletters with one subject don't match: neither
 * is a reply.
 */

/** A stored mail, as far as threading needs it. */
export interface ThreadCandidate {
  mail_id: string;
  message_id: string;
  in_reply_to?: string | null;
  reference_ids?: string[] | null;
  subject: string;
  from_address?: unknown;
  to_address?: unknown;
  cc_address?: unknown;
}

const REPLY_PREFIX = /^\s*((re|fwd?|aw|wg|sv|vs|antw|tr|rv)\s*(\[\d+\])?\s*[:：]\s*)+/i;

/** The subject without leading reply/forward markers, case-folded. */
export const normalizeSubject = (subject: string): string =>
  subject.replace(REPLY_PREFIX, "").trim().replace(/\s+/g, " ").toLowerCase();

/** Whether the subject marks a reply or a forward. */
export const isReplySubject = (subject: string): boolean =>
  REPLY_PREFIX.test(subject);

/** The Message-IDs a mail names: its own and the ones it answers. */
export const linkedIds = (mail: ThreadCandidate): string[] =>
  [mail.message_id, mail.in_reply_to, ...(mail.reference_ids ?? [])].filter(
    (id): id is string => !!id
  );

const addressesOf = (value: unknown): string[] => {
  const list = Array.isArray(value) ? value : value ? [value] : [];
  return list
    .map((e) => (e as { address?: unknown })?.address)
    .filter((a): a is string => typeof a === "string" && !!a)
    .map((a) => a.toLowerCase());
};

/** Every address on a mail's From, To and Cc. */
export const participantsOf = (mail: ThreadCandidate): Set<string> =>
  new Set([
    ...addressesOf(mail.from_address),
    ...addressesOf(mail.to_address),
    ...addressesOf(mail.cc_address)
  ]);

/** Whether two mails without thread headers read as one conversation. */
export const matchesBySubject = (
  a: ThreadCandidate,
  b: ThreadCandidate
): boolean => {
  const subject = normalizeSubject(a.subject);
  if (subject.length < 3 || subject !== normalizeSubject(b.subject)) return false;
  if (!isReplySubject(a.subject) && !isReplySubject(b.subject)) return false;
  const shared = participantsOf(a);
  for (const address of participantsOf(b)) if (shared.has(address)) return true;
  return false;
};

/** Whether one mail names the other in its thread headers. */
export const linkedByHeaders = (
  a: ThreadCandidate,
  b: ThreadCandidate
): boolean => {
  const ids = new Set(linkedIds(a));
  return linkedIds(b).some((id) => ids.has(id));
};

/**
 * The mails in `candidates` connected to `seed`, the seed included, by
 * headers or (for header-less mail) by subject, following chains.
 */
export const collectThread = <T extends ThreadCandidate>(
  seed: T,
  candidates: T[]
): T[] => {
  const thread = new Map<string, T>([[seed.mail_id, seed]]);
  const queue: T[] = [seed];
  while (queue.length) {
    const current = queue.shift() as T;
    for (const other of candidates) {
      if (thread.has(other.mail_id)) continue;
      if (
        linkedByHeaders(current, other) ||
        matchesBySubject(current, other)
      ) {
        thread.set(other.mail_id, other);
        queue.push(other);
      }
    }
  }
  return [...thread.values()];
};
