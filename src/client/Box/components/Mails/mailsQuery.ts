import { Category } from "client";

export const getMailsQueryUrl = (
  account: string,
  category: Category,
  allAccounts = false
) => {
  if (category === Category.Search) {
    return `/api/mails/search/${encodeURIComponent(account)}`;
  }

  // The unified list is the only URL that names no account: one server-side
  // query across every alias (design doc §8). Search stays per-account even
  // from the unified view — the search branch wins over the flag — so
  // entering search parks the account as usual and clearing it returns to
  // the view.
  if (allAccounts) return "/api/mails/unified";

  let queryOption: string;

  switch (category) {
    case Category.SpamMails:
      queryOption = "?spam=1";
      break;
    case Category.SentMails:
      queryOption = "?sent=1";
      break;
    case Category.SavedMails:
      queryOption = "?saved=1";
      break;
    default:
      queryOption = "";
  }

  return `/api/mails/headers/${account}${queryOption}`;
};
