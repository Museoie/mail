import { MailHeaderData, MaskedUser, SignedUser } from "common";
import {
  getThreadCandidates,
  getThreadSeed,
  ThreadRow
} from "../postgres/repositories/mails/thread";
import { toMailHeaderData } from "./headers";
import { collectThread, linkedIds, normalizeSubject } from "./threading";

/** Rounds of widening the id set; each round follows one more link. */
const MAX_ROUNDS = 4;

const resolveUserId = (user: MaskedUser | SignedUser): string | undefined =>
  "id" in user ? user.id : (user as { user_id?: string }).user_id;

/**
 * Every message in the conversation `mailId` belongs to, oldest first, the
 * opened mail included. Empty when the mail isn't the user's.
 */
export interface ThreadRepository {
  getThreadSeed: typeof getThreadSeed;
  getThreadCandidates: typeof getThreadCandidates;
}

const defaultRepository: ThreadRepository = {
  getThreadSeed,
  getThreadCandidates
};

export const getThread = async (
  user: MaskedUser | SignedUser,
  mailId: string,
  repository: ThreadRepository = defaultRepository
): Promise<MailHeaderData[]> => {
  const userId = resolveUserId(user);
  if (!userId) return [];

  const seed = await repository.getThreadSeed(userId, mailId);
  if (!seed) return [];

  const subject = normalizeSubject(seed.subject);
  const candidates = new Map<string, ThreadRow>([[seed.mail_id, seed]]);
  let ids = new Set(linkedIds(seed));
  let thread: ThreadRow[] = [seed];

  for (let round = 0; round < MAX_ROUNDS; round++) {
    const rows = await repository.getThreadCandidates(
      userId,
      [...ids],
      subject.length >= 3 ? subject : null,
      seed.is_spam
    );
    rows.forEach((row) => candidates.set(row.mail_id, row));
    thread = collectThread(seed, [...candidates.values()]);
    const nextIds = new Set(thread.flatMap(linkedIds));
    if (nextIds.size === ids.size) break;
    ids = nextIds;
  }

  return thread
    .sort((a, b) => +new Date(a.date) - +new Date(b.date))
    .map((row) => {
      const header = toMailHeaderData({ ...row, user_id: userId });
      if (row.is_spam) header.spam = true;
      return header;
    });
};
