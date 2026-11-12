import { MailHeaderData } from "common";
import {
  IsOwnAddress,
  ThreadCandidate,
  bareMessageId,
  hasThreadHeaders,
  linkedIds,
  matchesBySubject,
  normalizeSubject
} from "common";

/**
 * One row of a folder list: the messages of one conversation that are in
 * this folder, newest first. The row stands for the newest one.
 */
export interface Conversation {
  /** The newest message: the row's subject, date and snippet, and what opens. */
  latest: MailHeaderData;
  /** Every message of the conversation in this list, newest first. */
  mails: MailHeaderData[];
  unread: boolean;
  saved: boolean;
}

/** Subject buckets larger than this are matched only by headers. */
const MAX_SUBJECT_BUCKET = 200;

const asCandidate = (mail: MailHeaderData): ThreadCandidate => ({
  mail_id: mail.id,
  message_id: mail.messageId ?? "",
  in_reply_to: mail.inReplyTo ?? null,
  reference_ids: mail.references ?? null,
  subject: mail.subject,
  from_address: mail.from?.value,
  to_address: mail.to?.value,
  cc_address: mail.cc?.value
});

/**
 * The user's own addresses as the client knows them: those on the domain
 * their mail arrives at (`*@user.domain`, or `*@domain` for admin).
 */
export const ownAddressesFor = (
  username: string | undefined,
  domainName: string
): IsOwnAddress => {
  const domain = (
    !username || username === "admin" ? domainName : `${username}.${domainName}`
  ).toLowerCase();
  return (address) => address.toLowerCase().split("@").pop() === domain;
};

/**
 * Groups a list into conversations by the same rules as the conversation
 * page (common/threading.ts): thread headers link messages, and header-less
 * messages are matched by subject and a shared correspondent. Conversations
 * come in the order of their newest message, as the list is (newest first).
 */
export const groupConversations = (
  mails: MailHeaderData[],
  isOwn: IsOwnAddress
): Conversation[] => {
  const parent = mails.map((_, i) => i);
  const find = (i: number): number => {
    while (parent[i] !== i) {
      parent[i] = parent[parent[i]];
      i = parent[i];
    }
    return i;
  };
  const union = (a: number, b: number) => {
    const ra = find(a);
    const rb = find(b);
    if (ra !== rb) parent[Math.max(ra, rb)] = Math.min(ra, rb);
  };

  const candidates = mails.map(asCandidate);

  // Header links: any two messages naming the same Message-ID.
  const byId = new Map<string, number>();
  candidates.forEach((candidate, i) => {
    for (const id of linkedIds(candidate)) {
      const key = bareMessageId(id);
      const seen = byId.get(key);
      if (seen === undefined) byId.set(key, i);
      else union(seen, i);
    }
  });

  // Subject matches, between header-less messages only.
  const bySubject = new Map<string, number[]>();
  candidates.forEach((candidate, i) => {
    if (hasThreadHeaders(candidate)) return;
    const subject = normalizeSubject(candidate.subject);
    if (subject.length < 3) return;
    const bucket = bySubject.get(subject) ?? [];
    bucket.push(i);
    bySubject.set(subject, bucket);
  });
  bySubject.forEach((bucket) => {
    if (bucket.length < 2 || bucket.length > MAX_SUBJECT_BUCKET) return;
    for (let a = 0; a < bucket.length; a++) {
      for (let b = a + 1; b < bucket.length; b++) {
        if (find(bucket[a]) === find(bucket[b])) continue;
        if (matchesBySubject(candidates[bucket[a]], candidates[bucket[b]], isOwn)) {
          union(bucket[a], bucket[b]);
        }
      }
    }
  });

  const groups = new Map<number, MailHeaderData[]>();
  mails.forEach((mail, i) => {
    const root = find(i);
    const group = groups.get(root) ?? [];
    group.push(mail);
    groups.set(root, group);
  });

  const byNewest = (a: MailHeaderData, b: MailHeaderData) =>
    +new Date(b.date) - +new Date(a.date);
  return [...groups.values()]
    .map((group) => {
      const sorted = [...group].sort(byNewest);
      return {
        latest: sorted[0],
        mails: sorted,
        unread: sorted.some((m) => !m.read),
        saved: sorted.some((m) => m.saved)
      };
    })
    .sort((a, b) => byNewest(a.latest, b.latest));
};

/**
 * The row's sender line: each sender once, oldest first, the way the
 * conversation reads, with the user's own messages as "me".
 */
export const conversationSenders = (
  conversation: Conversation,
  isSent: (mail: MailHeaderData) => boolean
): string => {
  const names: string[] = [];
  [...conversation.mails].reverse().forEach((mail) => {
    const name = isSent(mail)
      ? "me"
      : mail.from?.value?.[0]?.name || mail.from?.value?.[0]?.address || "";
    if (name && !names.includes(name)) names.push(name);
  });
  return names.join(", ");
};
