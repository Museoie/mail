import { MailHeaderData, MaskedUser, SignedUser } from "common";
import {
  getThreadCandidates,
  getThreadSeed,
  getThreadSubjectCandidates,
  ThreadRow
} from "../postgres/repositories/mails/thread";
import { getUserDomain } from "../util";
import { toMailHeaderData } from "./headers";
import {
  collectThread,
  hasThreadHeaders,
  IsOwnAddress,
  linkedIds,
  normalizeSubject,
  storedIdForms
} from "./threading";

/** Rounds of widening the id set; each round follows one more link. */
const MAX_ROUNDS = 4;

const resolveUserId = (user: MaskedUser | SignedUser): string | undefined =>
  "id" in user ? user.id : (user as { user_id?: string }).user_id;

export interface ThreadRepository {
  getThreadSeed: typeof getThreadSeed;
  getThreadCandidates: typeof getThreadCandidates;
  getThreadSubjectCandidates: typeof getThreadSubjectCandidates;
}

const defaultRepository: ThreadRepository = {
  getThreadSeed,
  getThreadCandidates,
  getThreadSubjectCandidates
};

/**
 * The user's own addresses: those on the domain their mail arrives at
 * (`*@user.domain`, or `*@domain` for admin). Another user's address on this
 * server is a correspondent like any other.
 */
export const ownAddressesOf = (username: string): IsOwnAddress => {
  const domain = getUserDomain(username).toLowerCase();
  return (address) => address.toLowerCase().split("@").pop() === domain;
};

/**
 * Every message in the conversation `mailId` belongs to, oldest first, the
 * opened mail included. Empty when the mail isn't the user's.
 *
 * Header links are followed round by round. The subject lookup — the only
 * way to connect mail without thread headers — runs once, and only when the
 * conversation found so far holds header-less mail.
 */
export const getThread = async (
  user: MaskedUser | SignedUser,
  mailId: string,
  repository: ThreadRepository = defaultRepository,
  isOwnAddress: IsOwnAddress = ownAddressesOf(user.username ?? "")
): Promise<MailHeaderData[]> => {
  const userId = resolveUserId(user);
  if (!userId) return [];

  const seed = await repository.getThreadSeed(userId, mailId);
  if (!seed) return [];

  const includeSpam = seed.is_spam;
  const subject = normalizeSubject(seed.subject);
  const candidates = new Map<string, ThreadRow>([[seed.mail_id, seed]]);
  const add = (rows: ThreadRow[]) =>
    rows.forEach((row) => candidates.set(row.mail_id, row));

  let ids = new Set(linkedIds(seed));
  let thread: ThreadRow[] = [seed];
  let searchedSubject = false;

  for (let round = 0; round < MAX_ROUNDS; round++) {
    add(
      await repository.getThreadCandidates(
        userId,
        storedIdForms(ids),
        includeSpam
      )
    );
    thread = collectThread(seed, [...candidates.values()], isOwnAddress);

    const wantsSubject =
      !searchedSubject &&
      subject.length >= 3 &&
      thread.some((row) => !hasThreadHeaders(row));
    if (wantsSubject) {
      searchedSubject = true;
      add(
        await repository.getThreadSubjectCandidates(
          userId,
          seed.mail_id,
          includeSpam
        )
      );
      thread = collectThread(seed, [...candidates.values()], isOwnAddress);
    }

    const nextIds = new Set(thread.flatMap(linkedIds));
    if (nextIds.size === ids.size && !wantsSubject) break;
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
