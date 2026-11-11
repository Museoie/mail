import { useContext } from "react";
import { useQuery } from "react-query";
import { AccountsGetResponse } from "server";
import { Category, Context } from "client";
import {
  accountsQueryUrl,
  fetchAccounts
} from "client/Box/components/Accounts";
import LeftMenu from "./components/LeftMenu";
import "./index.scss";

/**
 * What the header shows: the view's name and the unread count that belongs
 * to it. The count follows the title — an account's own unread count next
 * to that account, the all-accounts total next to anything else — so the
 * header never pairs one mailbox's name with another number.
 */
export const headerTitleAndCount = ({
  selectedAccount,
  selectedCategory,
  isSettingsOpen,
  isNarrow,
  fullEmailHostName,
  newMailsTotal,
  accounts
}: {
  selectedAccount: string;
  selectedCategory: Category;
  isSettingsOpen: boolean;
  isNarrow: boolean;
  fullEmailHostName: string;
  newMailsTotal: number;
  accounts: AccountsGetResponse | undefined;
}): { title: string; fullTitle: string; count: number; countLabel: string } => {
  const total = {
    count: newMailsTotal,
    countLabel: `${newMailsTotal} unread in all accounts`
  };
  if (isSettingsOpen) return { title: "Settings", fullTitle: "Settings", ...total };
  // In search `selectedAccount` holds the search term, not an account.
  if (selectedCategory === Category.Search) {
    return { title: "Search", fullTitle: "Search", ...total };
  }
  if (!selectedAccount) {
    const name = "@" + fullEmailHostName;
    return { title: name, fullTitle: name, ...total };
  }
  const unread =
    accounts?.received.find((a) => a.key === selectedAccount)
      ?.unread_doc_count ?? 0;
  return {
    title: isNarrow ? selectedAccount.split("@")[0] : selectedAccount,
    fullTitle: selectedAccount,
    count: unread,
    countLabel: `${unread} unread in ${selectedAccount}`
  };
};

const Header = () => {
  const {
    viewSize,
    domainName,
    userInfo,
    selectedAccount,
    selectedCategory,
    isSettingsOpen,
    newMailsTotal
  } = useContext(Context);

  // Subscribe to the sidebar's accounts query without fetching, like the
  // list header does, so the count re-renders with every accounts update.
  const accountsQuery = useQuery<AccountsGetResponse>(
    accountsQueryUrl,
    fetchAccounts,
    { enabled: false }
  );

  const username = userInfo?.username;
  const fullEmailHostName =
    username === "admin"
      ? domainName
      : [username, domainName].filter((e) => e).join(".");

  const { title, fullTitle, count, countLabel } = headerTitleAndCount({
    selectedAccount,
    selectedCategory,
    isSettingsOpen: !!isSettingsOpen,
    isNarrow: viewSize.width <= 750,
    fullEmailHostName,
    newMailsTotal,
    accounts: accountsQuery.data
  });

  // Search and Compose live in the rail (the bottom bar on a phone), which
  // is always on screen, so the header no longer repeats them. That leaves
  // the title the full width between the menu and the edge.
  return (
    <header id="title_bar" className="sticky">
      {userInfo ? <LeftMenu /> : null}
      <h1 title={fullTitle}>{title}</h1>
      {userInfo && count > 0 ? (
        <span
          className="header-unread-count"
          title={countLabel}
          aria-label={countLabel}
        >
          {count}
        </span>
      ) : null}
    </header>
  );
};

export default Header;
