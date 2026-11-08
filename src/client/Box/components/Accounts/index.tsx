import {
  useState,
  useRef,
  useContext,
  useEffect,
  Dispatch,
  SetStateAction,
  ChangeEventHandler,
  KeyboardEventHandler
} from "react";
import { useQuery } from "react-query";

import {
  SkeletonAccount,
  SkeletonCategory,
  SearchIcon,
  RefreshIcon,
  LogoutIcon,
  SettingsIcon,
  InboxIcon,
  NewMailIcon,
  SortDownIcon,
  SortUpIcon,
  Allowlist
} from "./components";
import { EmptyStarIcon, BanIcon } from "../Mails/components";
import { SendIcon } from "../Writer/components";
import WriteIcon from "../../../Header/components/RightMenu/components/WriteIcon";

import { Account } from "common";
import {
  AccountsGetResponse,
  LoginDeleteResponse,
  SearchAccountsGetResponse
} from "server";
import {
  Context,
  Category,
  useLocalStorage,
  QueryCache,
  call,
  onKeyboardActivate,
  clearCachedQueries,
  unregisterServiceWorker,
  getLocalStorageItem,
  useIsOnline
} from "client";
import { MailsSynchronizer } from "client/Box";
import {
  accountsForCategory,
  categoryForAccount
} from "./selectableAccounts";
import {
  accountBadgeCount,
  displayNameFor,
  duplicatedDisplayNames,
  filterAccounts
} from "./accountList";
import { useAnchoredSelectedAccount } from "./useAnchoredSelectedAccount";

import "./index.scss";

export const accountsQueryUrl = "/api/mails/accounts";

export class AccountsCache extends QueryCache<AccountsGetResponse> {
  constructor() {
    super(accountsQueryUrl);
  }
}

// Shared by the Accounts sidebar and by the list header, which subscribes to
// the same query key (enabled: false — it never fetches, it only re-renders
// when the sidebar's data changes) to show the selected account's count.
export const fetchAccounts = async () => {
  const { status, body, message } = await call.get<AccountsGetResponse>(
    accountsQueryUrl
  );
  if (status === "success") return body as AccountsGetResponse;
  else throw new Error(message);
};

let isFetched = false;

enum SortBy {
  Date = "Sort by Date",
  Size = "Sort by Size",
  Name = "Sort by Name"
}

let searchDelay: NodeJS.Timeout;

const Accounts = ({
  setPage
}: {
  setPage: Dispatch<SetStateAction<number>>;
}) => {
  const [searchInputDom, setSearchInputDom] = useState<HTMLInputElement | null>(
    null
  );
  const preSearchAccount = useRef<string>("");
  const [sortBy, setSortBy] = useLocalStorage<SortBy>("sortBy", SortBy.Name);
  const [sortAscending, setSortAscending] = useLocalStorage(
    "sortAscending",
    true
  );
  const [showSortOptions, setShowSortOptions] = useState(false);
  const [isAllowlistOpen, setIsAllowlistOpen] = useState(false);
  // Substring filter for the accounts list (design doc §8): local view state
  // only — it never touches the selection or the folder.
  const [accountFilter, setAccountFilter] = useState("");

  const {
    viewSize,
    setUserInfo,
    selectedAccount,
    setSelectedAccount,
    selectedCategory,
    setSelectedCategory,
    isAccountsOpen,
    setIsAccountsOpen,
    isWriterOpen,
    setIsWriterOpen,
    newMailsTotal,
    setNewMailsTotal,
    hasDraft
  } = useContext(Context);

  const { isOnline } = useIsOnline();

  const getAccounts = async () => {
    isFetched = false;
    return fetchAccounts();
  };

  const onSuccess = (data: AccountsGetResponse) => {
    const newMails = data.received.reduce(
      (acc, e) => acc + e.unread_doc_count,
      0
    );

    setNewMailsTotal(newMails);

    if (!isFetched) {
      const sync = new MailsSynchronizer(selectedAccount, selectedCategory);
      if (sync.difference > 0) sync.refetchMails();
      isFetched = true;
    }
  };

  const query = useQuery<AccountsGetResponse>(accountsQueryUrl, getAccounts, {
    onSuccess,
    cacheTime: Infinity,
    refetchInterval: 1000 * 60 * 10,
    refetchIntervalInBackground: true,
    refetchOnMount: false,
    refetchOnWindowFocus: "always",
    refetchOnReconnect: false,
    retry: false
  });

  // In search mode `selectedAccount` holds the search term. Fetch the accounts
  // that own a matching mail so the side-tab can list them (clicking one jumps
  // to that account's All view). Empty term → disabled, so the tab stays empty
  // until the user types.
  const searchValue =
    selectedCategory === Category.Search ? selectedAccount : "";
  const searchAccountsQuery = useQuery<SearchAccountsGetResponse>(
    ["/api/mails/search-accounts", searchValue],
    async () => {
      const { status, body, message } =
        await call.get<SearchAccountsGetResponse>(
          `/api/mails/search-accounts/${encodeURIComponent(searchValue)}`
        );
      if (status === "success") return body as SearchAccountsGetResponse;
      throw new Error(message);
    },
    { enabled: !!searchValue, cacheTime: 0, retry: false }
  );

  // Spam is grouped per account (like received/sent); the category-tab badge
  // sums unread across the spam accounts, mirroring how newMailsTotal sums the
  // received accounts. Both ride the accounts payload — no extra fetch.
  const spamUnread = (query.data?.spam ?? []).reduce(
    (acc, e) => acc + e.unread_doc_count,
    0
  );

  useEffect(() => {
    if (searchInputDom && isAccountsOpen && !isWriterOpen)
      searchInputDom.focus();
  }, [searchInputDom, isAccountsOpen, isWriterOpen]);

  useAnchoredSelectedAccount(
    selectedAccount,
    selectedCategory,
    query.isSuccess ? query.data : undefined,
    setSelectedAccount
  );

  const touchStartHandler = () => setShowSortOptions(false);

  useEffect(() => {
    window.addEventListener("touchstart", touchStartHandler);
    return () => {
      window.removeEventListener("touchstart", touchStartHandler);
    };
  }, []);

  if (query.isLoading) {
    return (
      <div className="tab-holder">
        <div className="categories skeleton">
          <div>
            <SkeletonCategory />
            <SkeletonCategory />
            <SkeletonCategory />
            <SkeletonCategory />
            <SkeletonCategory />
            <SkeletonCategory />
          </div>
          <div>
            <SkeletonCategory />
            <SkeletonCategory />
          </div>
        </div>
        <div className="accounts">
          <div className="sort_box">
            <div />
          </div>
          <SkeletonAccount />
          <SkeletonAccount />
          <SkeletonAccount />
          <SkeletonAccount />
          <SkeletonAccount />
          <SkeletonAccount />
          <SkeletonAccount />
        </div>
      </div>
    );
  }

  if (query.error) {
    return (
      <div className="tab-holder">
        <div className="categories"></div>
        <div className="accounts">Accounts List Request Failed</div>
      </div>
    );
  }

  if (query.isSuccess) {
    const { received = [], sent = [], spam = [] } = query.data || {};

    const renderAccount = (data: Account, i: number) => {
      const accountName = data.key;
      const badgeCount = accountBadgeCount(data, selectedCategory);
      const displayName = displayNameFor(accountName);
      const onClickAccount = () => {
        // A found account in the search side-tab jumps to the category that
        // lists it (the search term lives in selectedAccount, so we must
        // switch category as well as the account).
        if (selectedCategory === Category.Search) {
          setPage(1);
          setSelectedCategory(
            categoryForAccount(accountName, { received, sent, spam })
          );
          setSelectedAccount(accountName);
          if (viewSize.width <= 1050) setIsAccountsOpen(false);
          return;
        }
        if (selectedAccount !== accountName) {
          setPage(1);
          setSelectedAccount(accountName);
          if (viewSize.width <= 1050) setIsAccountsOpen(false);
        }
      };

      const classes = ["tag"];
      if (selectedAccount === accountName) classes.push("clicked");
      else classes.push("cursor");

      return (
        <div key={i}>
          <div
            className={classes.join(" ")}
            role="button"
            tabIndex={0}
            onClick={onClickAccount}
            onKeyDown={onKeyboardActivate(onClickAccount)}
          >
            <span className="account-name">{displayName}</span>
            {duplicatedNames.has(displayName) ? (
              <span className="account-address">{accountName}</span>
            ) : null}
            {badgeCount ? (
              <div className="numberBall">{badgeCount}</div>
            ) : null}
          </div>
        </div>
      );
    };

    // The rows rendered here and the selections `resolveSelectedAccount`
    // accepts have to be the same set, or a highlighted name sits over an empty
    // pane again. Search is the one list the resolver does not decide: its
    // side-tab is fed by the search query, and `selectedAccount` holds the term.
    const sortedAccountData: Account[] =
      selectedCategory === Category.Search
        ? searchAccountsQuery.data || []
        : accountsForCategory(selectedCategory, { received, sent, spam });

    const sortingFactor = 2 * +sortAscending - 1;

    if (sortBy === SortBy.Name) {
      sortedAccountData.sort(
        (a, b) => (2 * +(a.key > b.key) - 1) * sortingFactor
      );
    } else if (sortBy === SortBy.Date) {
      sortedAccountData.sort(
        (a, b) => (+new Date(a.updated) - +new Date(b.updated)) * sortingFactor
      );
    } else if (sortBy === SortBy.Size) {
      const sortKey =
        selectedCategory === Category.NewMails
          ? "unread_doc_count"
          : selectedCategory === Category.SavedMails
          ? "saved_doc_count"
          : "doc_count";
      sortedAccountData.sort(
        (a, b) => (a[sortKey] - b[sortKey]) * sortingFactor
      );
    }

    // The filter narrows the visible rows only; the selection, the sort, and
    // the folder are untouched. Duplicates are judged over the visible list,
    // so a filter that hides one of a pair needs no disambiguation.
    const filteredAccountData = filterAccounts(
      sortedAccountData,
      accountFilter
    );
    const duplicatedNames = duplicatedDisplayNames(filteredAccountData);
    const accountComponents = filteredAccountData.map(renderAccount);

    const categoryIcons = {
      [Category.NewMails]: NewMailIcon,
      [Category.AllMails]: InboxIcon,
      [Category.SavedMails]: EmptyStarIcon,
      [Category.SentMails]: SendIcon,
      [Category.SpamMails]: BanIcon,
      [Category.Search]: SearchIcon
    };

    const categoryComponents = Object.values(Category).map((e, i) => {
      const onClickCategory = () => {
        if (e === Category.Search) {
          // Entering search: remember the current account to restore on exit,
          // and start from an empty term so the side-tab (found accounts) and
          // the mails list stay empty until the user types a keyword.
          preSearchAccount.current = selectedAccount;
          setSelectedCategory(e);
          setSelectedAccount("");
          return;
        }

        // Leaving search restores the pre-search account (selectedAccount
        // currently holds the search term, so we can't read it here).
        // Otherwise the selection is left alone: changing folders never
        // changes the selected mailbox (design doc §8). A name no list holds
        // is cleaned up by the anchor hook, not by re-anchoring here.
        if (selectedCategory === Category.Search && preSearchAccount.current) {
          setSelectedAccount(preSearchAccount.current);
        }
        setSelectedCategory(e);
      };
      const classes = ["rail-item"];
      if (selectedCategory === e) classes.push("clicked");

      const Icon = categoryIcons[e];
      const showDot =
        e !== selectedCategory &&
        ((e === Category.NewMails && newMailsTotal) ||
          (e === Category.SpamMails && spamUnread));

      return (
        <div
          key={i}
          className={classes.join(" ")}
          role="tab"
          tabIndex={0}
          aria-selected={selectedCategory === e}
          onClick={onClickCategory}
          onKeyDown={onKeyboardActivate(onClickCategory)}
        >
          <Icon />
          <span className="rail-label">{e.split(" ")[0]}</span>
          {showDot ? <div className="numberBall" /> : null}
        </div>
      );
    });

    const sortOptionComponents = Object.values(SortBy).map((e, i) => {
      const onClickSortOption = () => {
        if (e !== sortBy) {
          setSortBy(e);
          if (e === SortBy.Name) setSortAscending(true);
          else setSortAscending(false);
        }
        setShowSortOptions(false);
      };

      return (
        <div
          key={i}
          className="sort_option cursor"
          onClick={onClickSortOption}
          onTouchStart={(e) => e.stopPropagation()}
        >
          {e}
        </div>
      );
    });

    const onChangeSearch: ChangeEventHandler<HTMLInputElement> = (e) => {
      clearTimeout(searchDelay);
      searchDelay = setTimeout(() => {
        setSelectedAccount(e.target.value);
      }, 500);
    };

    const onKeyDownSearch: KeyboardEventHandler<HTMLInputElement> = (e) => {
      if (e.key === "Enter") {
        const target = e.target as HTMLInputElement;
        // Search immediately instead of waiting out the debounce.
        clearTimeout(searchDelay);
        setSelectedAccount(target.value);
        if (viewSize.width <= 1050) setIsAccountsOpen(false);
      }
    };

    const onClickCompose = () => {
      // One overlay at a time on narrow screens: the writer docks over the
      // list, so the accounts overlay (which also covers the list) closes.
      if (!isWriterOpen && isAccountsOpen && viewSize.width <= 1050)
        setIsAccountsOpen(false);
      setIsWriterOpen(!isWriterOpen);
    };

    // Refresh re-fetches without moving the selection: the account, folder,
    // scroll position, and open/closed state all stay where they were
    // (design doc §8). `query.isFetching` drives the spinner on the rail
    // item while the round-trip runs.
    const onClickRefresh = () => {
      query.refetch();
    };

    const onClickLogout = async () => {
      const confirmed = window.confirm("Are you sure you want to log out?");
      if (!confirmed) return;
      // Drop the server-side push subscription while the session is still live —
      // the unsubscribe route is authenticated, so this must run before the
      // logout call below tears the session down. Best-effort: call.delete never
      // throws.
      const pushSubscriptionId = getLocalStorageItem("push_subscription_id");
      if (pushSubscriptionId) {
        await call.delete("/api/push/subscribe/" + pushSubscriptionId);
      }
      const response = await call.delete<LoginDeleteResponse>(
        "/api/users/login"
      );
      if (response.status !== "success") return;
      await clearCachedQueries();
      await unregisterServiceWorker();
      [
        "name",
        "to",
        "cc",
        "bcc",
        "subject",
        "sender",
        "initialContent",
        "originalMessage",
        "originalMessageMeta",
        "isCcOpen",
        "push_subscription_id"
      ].forEach((key) => localStorage.removeItem(key));
      setUserInfo(undefined);
      setSelectedAccount("");
    };

    return (
      <div className="tab-holder">
        <div className="categories">
          <div>
            {categoryComponents}
            <div
              className="rail-item"
              role="button"
              tabIndex={0}
              aria-pressed={isWriterOpen}
              onClick={onClickCompose}
              onKeyDown={onKeyboardActivate(onClickCompose)}
            >
              <WriteIcon />
              <span className="rail-label">Compose</span>
              {hasDraft ? <div className="numberBall" /> : null}
            </div>
          </div>
          <div>
            <div
              className="rail-item"
              role="button"
              tabIndex={0}
              aria-label="Settings"
              onClick={() => setIsAllowlistOpen(true)}
              onKeyDown={onKeyboardActivate(() => setIsAllowlistOpen(true))}
            >
              <SettingsIcon />
              <span className="rail-label">Settings</span>
            </div>
            <div
              className={"rail-item" + (query.isFetching ? " refreshing" : "")}
              role="button"
              tabIndex={0}
              aria-label="Refresh"
              aria-busy={query.isFetching}
              onClick={onClickRefresh}
              onKeyDown={onKeyboardActivate(onClickRefresh)}
            >
              <RefreshIcon />
              <span className="rail-label">Refresh</span>
            </div>
          </div>
        </div>
        {isAllowlistOpen ? (
          <Allowlist onClose={() => setIsAllowlistOpen(false)} />
        ) : null}
        <div className="accounts">
          <div className="accounts_header">
            <div className="filter_box">
              <input
                type="search"
                value={accountFilter}
                onChange={(e) => setAccountFilter(e.target.value)}
                placeholder="Filter accounts…"
                aria-label="Filter accounts"
                autoComplete="off"
                autoCorrect="off"
                autoCapitalize="off"
                spellCheck="false"
              />
              {accountFilter ? (
                <button
                  type="button"
                  className="filter_clear"
                  aria-label="Clear filter"
                  onClick={() => setAccountFilter("")}
                >
                  ×
                </button>
              ) : null}
            </div>
            <div className="sort_box">
            {showSortOptions ? (
              <></>
            ) : (
              <div
                className="sort_icon cursor"
                onClick={() => setSortAscending(!sortAscending)}
              >
                <div>
                  <SortDownIcon className={sortAscending ? "" : "highlight"} />
                  <SortUpIcon className={sortAscending ? "highlight" : ""} />
                </div>
              </div>
            )}
            <div className="sort_text">
              {showSortOptions ? (
                <></>
              ) : (
                <div
                  className="sort_current cursor"
                  onClick={() => setShowSortOptions(true)}
                >
                  {sortBy.split(" ").pop()}
                </div>
              )}
              {showSortOptions ? (
                <div
                  className="sort_select"
                  onMouseLeave={() => setShowSortOptions(false)}
                >
                  {sortOptionComponents}
                </div>
              ) : (
                <></>
              )}
            </div>
          </div>
          </div>
          <div className="tags_container">
            {!showSortOptions && selectedCategory === Category.Search ? (
              <div className="search_container">
                <div className="fieldName">
                  <span>Search This:</span>
                </div>
                <input
                  type="search"
                  autoComplete="off"
                  autoCorrect="off"
                  autoCapitalize="off"
                  spellCheck="false"
                  onChange={onChangeSearch}
                  onKeyDown={onKeyDownSearch}
                  ref={(e) => e && setSearchInputDom(e)}
                />
              </div>
            ) : null}
            {accountComponents.length ? (
              accountComponents
            ) : accountFilter ? (
              <p className="accounts_empty">
                No accounts match “{accountFilter}”.
              </p>
            ) : null}
          </div>
          <div className="accounts_footer">
            <button
              type="button"
              className="accounts-logout"
              onClick={onClickLogout}
              aria-label="Logout"
              disabled={!isOnline}
              title={
                isOnline
                  ? "Logout"
                  : "You're offline — reconnect to log out"
              }
            >
              <LogoutIcon />
              <span>Logout</span>
            </button>
          </div>
        </div>
      </div>
    );
  }

  return <></>;
};

export default Accounts;
