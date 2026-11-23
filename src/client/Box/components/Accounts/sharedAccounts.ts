import { AccountsGetResponse } from "server";

/**
 * The accounts payload with the accounts of the domains the user views folded
 * into `received` and `sent`, so every list, selection and count treats them
 * like the user's own. `shared` stays as it came, which is how a row tells a
 * viewed account (read-only) from an owned one.
 */
export const withSharedAccounts = (data: AccountsGetResponse): AccountsGetResponse => {
  const shared = data.shared ?? [];
  if (!shared.length) return { ...data, shared };
  return {
    ...data,
    received: [...data.received, ...shared.flatMap((domain) => domain.received)],
    sent: [...data.sent, ...shared.flatMap((domain) => domain.sent)],
    shared,
  };
};

/** The keys of the accounts that belong to a domain the user only views. */
export const sharedAccountKeys = (data: AccountsGetResponse | undefined): Set<string> =>
  new Set(
    (data?.shared ?? []).flatMap((domain) =>
      [...domain.received, ...domain.sent].map((account) => account.key)
    )
  );
