import { useEffect } from "react";
import { Category } from "client";
import { AccountLists, resolveSelectedAccount } from "./selectableAccounts";

/**
 * Keeps `selectedAccount` where the user put it — picking the category's first
 * one on a fresh login, and re-anchoring a name no list contains.
 *
 * A real account the current category cannot list stays selected: changing
 * folders never changes the selected mailbox (design doc §8), and the mail
 * pane shows that folder's empty state for the account. Outside Search the
 * value is an account key, so a name no list holds is unreachable state
 * rather than a selection: a search term left behind by a reload out of
 * Search mode, or an account whose last mail here was deleted. No row
 * highlights, the pane has nothing to render, and no affordance recovers it.
 *
 * Pass `lists` as `undefined` until the payload has loaded, so a pending fetch
 * is never read as an empty list.
 */
export const useAnchoredSelectedAccount = (
  selectedAccount: string,
  selectedCategory: Category,
  lists: AccountLists | undefined,
  setSelectedAccount: (account: string) => void
) => {
  useEffect(() => {
    if (!lists) return;
    const resolved = resolveSelectedAccount(
      selectedAccount,
      selectedCategory,
      lists
    );
    if (resolved !== null) setSelectedAccount(resolved);
  }, [selectedAccount, selectedCategory, lists]);
};
