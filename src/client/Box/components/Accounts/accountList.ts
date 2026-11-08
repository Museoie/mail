import { Account } from "common";
import { AccountsGetResponse } from "server";
import { Category } from "client";

/**
 * The display name for an account row: the local part of the address
 * ("annie" for "annie@x.com"), or "Unknown" when the address has none.
 */
export const displayNameFor = (key: string): string =>
  key.split("@")[0] || "Unknown";

/**
 * The display names that appear more than once in `accounts` — the rows whose
 * second line must carry the full address so two "annie" entries can be told
 * apart (design doc §8). Computed over the visible list, so a filter that
 * hides one of a pair needs no disambiguation.
 */
export const duplicatedDisplayNames = (accounts: Account[]): Set<string> => {
  const counts = new Map<string, number>();
  for (const account of accounts) {
    const name = displayNameFor(account.key);
    counts.set(name, (counts.get(name) ?? 0) + 1);
  }
  return new Set(
    [...counts.entries()].filter(([, n]) => n > 1).map(([name]) => name)
  );
};

/**
 * The accounts whose key contains `filter` (case-insensitive substring), in
 * their existing order. An empty filter matches everything — filtering never
 * disturbs the selection or the folder, it only narrows the visible rows.
 */
export const filterAccounts = (
  accounts: Account[],
  filter: string
): Account[] => {
  const needle = filter.trim().toLowerCase();
  if (!needle) return accounts;
  return accounts.filter((a) => a.key.toLowerCase().includes(needle));
};

/**
 * The count badge for one account row under `category` (design doc §8):
 * saved for Saved, unread everywhere else. This deliberately differs from
 * the list header for Sent and Spam, where the header shows `doc_count`
 * totals; the badge is always a per-account unread (or saved) count. The
 * row already holds the account, so this reads the field directly instead
 * of re-finding it in the payload.
 */
export const accountBadgeCount = (
  account: Account,
  category: Category
): number => {
  if (category === Category.SavedMails) return account.saved_doc_count;
  return account.unread_doc_count;
};

/**
 * The folder-level badge totals for the narrow-nav Folders sheet (design
 * doc §4, option A): the same per-account rule as `accountBadgeCount` —
 * saved for Saved, unread everywhere else — summed over the bucket each
 * folder draws from. Reads the accounts payload already in hand; no extra
 * fetch. New and All both draw from `received`, so they share its unread
 * total, matching the list header's "unread" noun for both.
 */
export const folderBadgeCounts = (
  data: AccountsGetResponse | undefined
): {
  [Category.NewMails]: number;
  [Category.AllMails]: number;
  [Category.SavedMails]: number;
  [Category.SentMails]: number;
  [Category.SpamMails]: number;
} => {
  const sum = (
    accounts: Account[],
    key: "unread_doc_count" | "saved_doc_count"
  ) => accounts.reduce((acc, a) => acc + a[key], 0);
  const received = data?.received ?? [];
  const sent = data?.sent ?? [];
  const spam = data?.spam ?? [];
  const unreadReceived = sum(received, "unread_doc_count");
  return {
    [Category.NewMails]: unreadReceived,
    [Category.AllMails]: unreadReceived,
    [Category.SavedMails]: sum(received, "saved_doc_count"),
    [Category.SentMails]: sum(sent, "unread_doc_count"),
    [Category.SpamMails]: sum(spam, "unread_doc_count")
  };
};
