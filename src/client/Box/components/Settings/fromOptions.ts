import { AccountsGetResponse } from "server";
import { isOnDomains } from "client";

/**
 * The addresses offered as the default From: every account on the user's own
 * domains that has received or sent mail, plus the saved choice so it stays
 * selectable after its mail is gone. Sorted, without duplicates. A domain the
 * user only views is never offered: they cannot send from it.
 */
export const fromOptions = (
  accounts: AccountsGetResponse | undefined,
  domains: string[],
  current: string
): string[] => {
  const keys = new Set<string>();
  for (const { key } of [...(accounts?.received ?? []), ...(accounts?.sent ?? [])]) {
    if (isOnDomains(key, domains)) keys.add(key);
  }
  if (current) keys.add(current);
  return [...keys].sort((a, b) => a.localeCompare(b));
};
