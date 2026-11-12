import { AccountsGetResponse } from "server";
import { Category } from "client";
import { accountsForCategory } from "client/Box/components/Accounts/selectableAccounts";

export interface EmptyFolderState {
  message: string;
  /**
   * Other accounts that do have mail in this folder, newest first, so an
   * empty view never hides the fact that the folder's badge counts mail
   * somewhere else.
   */
  others: { key: string; count: number }[];
}

const MAX_OTHERS = 5;

/**
 * What an empty folder says. The selection stays on the account the reader
 * picked (changing folders never changes the mailbox), so the message names
 * that account — "No spam in shopping@…" rather than "No spam — nice." while
 * the Spam badge counts four elsewhere — and lists the accounts that hold
 * this folder's mail.
 */
export const emptyFolderState = (
  category: Category,
  account: string,
  accounts: AccountsGetResponse | undefined,
  allAccounts = false
): EmptyFolderState => {
  if (category === Category.Search) {
    return { message: "No results found.", others: [] };
  }

  // The unified view already spans every account, so there are no "other
  // accounts" left to list — and the message names the view, not the parked
  // account behind it.
  if (allAccounts) {
    return { message: "No mail in all accounts.", others: [] };
  }

  const where = account ? ` in ${account}` : "";
  const message = (() => {
    switch (category) {
      case Category.SavedMails:
        return `No saved mail${where}.`;
      case Category.SentMails:
        return `No sent mail${where}.`;
      case Category.SpamMails:
        return `No spam${where}.`;
      default:
        return `No mail${where}.`;
    }
  })();

  const countKey =
    category === Category.SavedMails ? "saved_doc_count" : "doc_count";

  const others = accounts
    ? accountsForCategory(category, accounts)
        .filter((a) => a.key !== account && a[countKey] > 0)
        .sort((a, b) => +new Date(b.updated) - +new Date(a.updated))
        .slice(0, MAX_OTHERS)
        .map((a) => ({ key: a.key, count: a[countKey] }))
    : [];

  return { message, others };
};
