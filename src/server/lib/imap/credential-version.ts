import { getUser } from "server";

/**
 * The credential row's current password_version for `username` — the
 * generation the revocation fence compares a grant's stamped
 * version against.
 *
 * Rows that predate the fence's migration read back null and count as
 * generation 1, the same default the migration's DEFAULT 1 gives every
 * existing row. A missing row is null: the account was deleted (or
 * soft-deleted, which the read leaves out), and every grant it issued is
 * revoked with it.
 */
export const getCredentialPasswordVersion = async (
  username: string
): Promise<number | null> => {
  const user = await getUser({ username });
  if (!user) return null;
  return user.passwordVersion ?? 1;
};
