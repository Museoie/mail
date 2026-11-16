/**
 * Mark-read requests a list response may predate. Opening a mail shows it
 * read at once, but a list requested around the same moment can be read from
 * the database before the mark lands and arrive after the row was updated,
 * putting the unread copy back until the next refetch.
 */

import { MailHeaderData } from "common";

/** When each mark-read request succeeded; null while it is in flight. */
const marks = new Map<string, number | null>();
const fetchesInFlight = new Set<{ startedAt: number }>();

const dropMarksEveryFetchHasSeen = () => {
  const oldestStart = Math.min(
    ...[...fetchesInFlight].map((f) => f.startedAt)
  );
  marks.forEach((succeededAt, id) => {
    if (succeededAt !== null && succeededAt < oldestStart) marks.delete(id);
  });
};

/** Keep `id` read in list responses that may not include `request`'s write. */
export const trackMarkRead = (
  id: string,
  request: Promise<{ status: string }>
) => {
  marks.set(id, null);
  const settle = (succeeded: boolean) => {
    if (succeeded) marks.set(id, Date.now());
    else marks.delete(id);
    dropMarksEveryFetchHasSeen();
  };
  request.then(
    ({ status }) => settle(status === "success"),
    () => settle(false)
  );
};

export const pendingReadCount = () => marks.size;

/**
 * Runs a list fetch and marks read every mail whose mark-read request was
 * still in flight, or succeeded, after the fetch began.
 */
export const withPendingReads = async (
  fetchMails: () => Promise<MailHeaderData[]>
) => {
  const fetch = { startedAt: Date.now() };
  fetchesInFlight.add(fetch);
  try {
    const mails = await fetchMails();
    return mails.map((mail) => {
      if (mail.read) return mail;
      const succeededAt = marks.get(mail.id);
      const predatesMark =
        succeededAt === null ||
        (succeededAt !== undefined && succeededAt >= fetch.startedAt);
      return predatesMark ? new MailHeaderData({ ...mail, read: true }) : mail;
    });
  } finally {
    fetchesInFlight.delete(fetch);
    dropMarksEveryFetchHasSeen();
  }
};
