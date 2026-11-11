/** The address of a mail's own page. */
export const mailPagePath = (mailId: string) =>
  `/mail/${encodeURIComponent(mailId)}`;

/**
 * History state set when the app opens a mail page from its own list, so
 * Back knows a step back returns there. A page reached any other way — a
 * reload, a shared link — goes to the list instead of leaving the app.
 */
export const FROM_LIST_STATE = { fromList: true } as const;

export const openedFromList = (state: unknown): boolean =>
  !!state && (state as { fromList?: unknown }).fromList === true;
