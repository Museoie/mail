import { Category } from "client";

/** Widths at or below this use the bottom bar and the Folders sheet. */
export const NARROW_MAX_WIDTH = 750;

/**
 * Whether moving from folder `from` to folder `to` opens the accounts menu.
 * On the narrow layout the account list sits behind the header, so a folder
 * change opens it to pick the account next. Wider layouts, Search, and
 * re-picking the current folder leave it closed.
 */
export const opensAccountsAfterFolderChange = (
  width: number,
  from: Category,
  to: Category
): boolean =>
  width <= NARROW_MAX_WIDTH && to !== Category.Search && from !== to;
