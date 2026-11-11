/**
 * Which stored mails belong to one conversation.
 *
 * Mail that carries `In-Reply-To`/`References` is threaded by those headers
 * alone: two mails are in one thread when one's Message-ID appears in the
 * other's headers, directly or through other mails in between.
 *
 * Mail with neither header — everything stored before these columns existed,
 * and mail from clients that don't send them — is matched by subject instead,
 * but only against other header-less mail: the same subject once `Re:`/`Fwd:`
 * prefixes are stripped, at least one of the two marked as a reply or forward,
 * and at least one shared participant other than the user's own addresses.
 * The user is on every mail they store, so their own address says nothing
 * about which conversation a mail belongs to; two vendors' unrelated
 * "Re: Invoice" mails share only that, and stay apart.
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

/** Whether `address` is one of the user's own (on this server's domain). */
export type IsOwnAddress = (address: string) => boolean;

const REPLY_PREFIX = /^\s*((re|fwd?|aw|wg|sv|vs|antw|tr|rv)\s*(\[\d+\])?\s*[:：]\s*)+/i;

/** The subject without leading reply/forward markers, case-folded. */
export const normalizeSubject = (subject: string): string =>
  subject.replace(REPLY_PREFIX, "").trim().replace(/\s+/g, " ").toLowerCase();

/** Whether the subject marks a reply or a forward. */
export const isReplySubject = (subject: string): boolean =>
  REPLY_PREFIX.test(subject);

/**
 * A Message-ID without its angle brackets. Rows stored over SMTP keep the
 * brackets; rows appended over IMAP store the bare id. Comparing the bare form
 * matches both.
 */
export const bareMessageId = (id: string): string =>
  id.trim().replace(/^</, "").replace(/>$/, "");

/** Both stored spellings of each id, for matching against the columns. */
export const storedIdForms = (ids: Iterable<string>): string[] => {
  const forms = new Set<string>();
  for (const id of ids) {
    const bare = bareMessageId(id);
    if (!bare) continue;
    forms.add(bare);
    forms.add(`<${bare}>`);
  }
  return [...forms];
};

/** Whether a mail carries either threading header. */
export const hasThreadHeaders = (mail: ThreadCandidate): boolean =>
  !!mail.in_reply_to || !!mail.reference_ids?.length;

/** The Message-IDs a mail names, bare: its own and the ones it answers. */
export const linkedIds = (mail: ThreadCandidate): string[] =>
  [mail.message_id, mail.in_reply_to, ...(mail.reference_ids ?? [])]
    .filter((id): id is string => !!id)
    .map(bareMessageId)
    .filter((id) => !!id);

const addressesOf = (value: unknown): string[] => {
  const list = Array.isArray(value) ? value : value ? [value] : [];
  return list
    .map((e) => (e as { address?: unknown })?.address)
    .filter((a): a is string => typeof a === "string" && !!a)
    .map((a) => a.toLowerCase());
};

/** Every address on a mail's From, To and Cc that isn't the user's own. */
export const otherParticipantsOf = (
  mail: ThreadCandidate,
  isOwn: IsOwnAddress
): Set<string> =>
  new Set(
    [
      ...addressesOf(mail.from_address),
      ...addressesOf(mail.to_address),
      ...addressesOf(mail.cc_address)
    ].filter((address) => !isOwn(address))
  );

/** Whether two header-less mails read as one conversation. */
export const matchesBySubject = (
  a: ThreadCandidate,
  b: ThreadCandidate,
  isOwn: IsOwnAddress
): boolean => {
  if (hasThreadHeaders(a) || hasThreadHeaders(b)) return false;
  const subject = normalizeSubject(a.subject);
  if (subject.length < 3 || subject !== normalizeSubject(b.subject)) return false;
  if (!isReplySubject(a.subject) && !isReplySubject(b.subject)) return false;
  const shared = otherParticipantsOf(a, isOwn);
  for (const address of otherParticipantsOf(b, isOwn)) {
    if (shared.has(address)) return true;
  }
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
 * headers or (between header-less mails) by subject, following chains.
 */
export const collectThread = <T extends ThreadCandidate>(
  seed: T,
  candidates: T[],
  isOwn: IsOwnAddress
): T[] => {
  const thread = new Map<string, T>([[seed.mail_id, seed]]);
  const queue: T[] = [seed];
  while (queue.length) {
    const current = queue.shift() as T;
    for (const other of candidates) {
      if (thread.has(other.mail_id)) continue;
      if (
        linkedByHeaders(current, other) ||
        matchesBySubject(current, other, isOwn)
      ) {
        thread.set(other.mail_id, other);
        queue.push(other);
      }
    }
  }
  return [...thread.values()];
};
