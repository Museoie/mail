import { randomBytes } from "crypto";
import { pool } from "../client";
import { SOFT_DELETE_CONDITION, type PreparedQuery } from "../database";
import { hashPassword } from "../../verify-password";
import {
  MaskedUser,
  User,
  UserModel,
  usersTable,
  USER_ID,
  USERNAME,
  PASSWORD,
  PASSWORD_VERSION,
  TOKEN,
  EXPIRY,
  EMAIL,
  IMAP_UID_VALIDITY,
  USERS,
  AUTH_SUB,
  IS_DELETED,
  DB_NOW,
} from "../models";

export type IndexUserInput = Omit<User, "user_id" | "password"> & {
  user_id?: string;
  /**
   * Omit to leave an existing row's stored password untouched: the column is
   * dropped from the INSERT, and `buildUpsert` carries onto the conflict clause
   * only the columns the INSERT wrote. A row created without one has no
   * password and cannot sign in until one is set.
   */
  password?: string;
};
/** `password` is the plain text to hash; an update never clears one. */
export type PartialUser = { user_id: string } & Partial<Omit<User, "password">> & {
  password?: string;
};

export const maskUser = (user: User): MaskedUser => {
  const { user_id, username, email } = user;
  return { user_id, username, email };
};

export const writeUser = async (
  user: IndexUserInput
): Promise<{ _id: string } | undefined> => {
  const { user_id, username, password, email, password_version } = user;
  const hashedPassword = password ? await hashPassword(password) : undefined;

  const row: Record<string, unknown> = { username, password: hashedPassword };
  if (user_id) row.user_id = user_id;
  if (email) row.email = email;
  // Carried onto the conflict clause only when the caller passes it, so a
  // boot-path rotation can bump the generation in the SAME upsert that
  // writes the new hash. Callers that omit it leave the stored generation
  // alone (a new row takes the column DEFAULT 1).
  if (password_version !== undefined) row[PASSWORD_VERSION] = password_version;

  const result = await usersTable.upsert(row);
  if (result) return { _id: result.user_id as string };
  return undefined;
};

/**
 * Rotate a user's credential and bump its generation atomically.
 *
 * The query builder only binds values, so a `SET password_version = ... + 1`
 * cannot go through `usersTable.update` — this is the direct-SQL exception,
 * like the mailbox lookups that need a comparison the builder cannot express.
 * The bump, the new hash, and the token/expiry clearing are one UPDATE, so
 * two concurrent rotations cannot both read generation N and both write N+1
 * with different hashes. Returns the generation the row now carries (the
 * caller signs with this, never with a value it computed before the write),
 * or null when the row does not exist. Rows that predate the fence read
 * back null, and COALESCE treats those as generation 1, so their first
 * bumped generation is 2.
 */
export const rotateUserPassword = async (
  user_id: string,
  updates: {
    passwordHash: string;
    username: string;
    token: string | null;
    expiry: string | null;
  }
): Promise<number | null> => {
  const result = await pool.query(
    `UPDATE ${USERS} SET ${PASSWORD} = $2, ${PASSWORD_VERSION} = COALESCE(${PASSWORD_VERSION}, 1) + 1, ${USERNAME} = $3, ${TOKEN} = $4, ${EXPIRY} = $5, updated = CURRENT_TIMESTAMP WHERE ${USER_ID} = $1 RETURNING ${PASSWORD_VERSION}`,
    [
      user_id,
      updates.passwordHash,
      updates.username,
      updates.token,
      updates.expiry,
    ]
  );
  const version = result.rows[0]?.[PASSWORD_VERSION];
  return typeof version === "number" ? version : null;
};

export const searchUser = async (
  user: Partial<MaskedUser>
): Promise<User | undefined> => {
  const filters: Record<string, unknown> = {};
  if (user.user_id) filters[USER_ID] = user.user_id;
  if (user.username) filters.username = user.username;
  if (user.email) filters[EMAIL] = user.email;

  if (Object.keys(filters).length === 0) return undefined;

  const model = await usersTable.queryOne(filters);
  return model?.toUser();
};

export const updateUser = async (user: PartialUser): Promise<boolean> => {
  if (!user) return false;
  const { user_id, username, password, email } = user;

  const updates: Record<string, unknown> = {};
  if (username !== undefined) updates.username = username;
  if (password !== undefined)
    updates.password = await hashPassword(password);
  if (email !== undefined) updates.email = email;

  if (Object.keys(updates).length === 0) return false;

  const model = await usersTable.update(user_id, updates);
  return model !== null;
};

export const getUserById = async (
  user_id: string
): Promise<User | undefined> => {
  const model = await usersTable.queryOne({ [USER_ID]: user_id });
  return model?.toUser();
};

export const deleteUser = async (user_id: string): Promise<boolean> => {
  return await usersTable.softDelete(user_id);
};

export const getUserByEmail = async (
  email: string
): Promise<User | undefined> => {
  const model = await usersTable.queryOne({ [EMAIL]: email });
  return model?.toUser();
};

/**
 * Get the IMAP UIDVALIDITY for a user.
 * Per RFC 3501, UIDVALIDITY is a value that, combined with UIDs, uniquely
 * identifies messages. If it changes, clients must discard cached message state.
 *
 * On first IMAP access, initializes UIDVALIDITY to current Unix timestamp.
 * This ensures uniqueness and that the value only increases over time.
 */
export const getImapUidValidity = async (user_id: string): Promise<number> => {
  const model = await usersTable.queryOne({ [USER_ID]: user_id });
  if (!model) {
    throw new Error(`User not found: ${user_id}`);
  }

  // If already set, return the stored value
  if (model.imap_uid_validity !== null) {
    return model.imap_uid_validity;
  }

  // Initialize to current Unix timestamp (seconds since epoch)
  // This ensures uniqueness and monotonically increasing values
  const uidValidity = Math.floor(Date.now() / 1000);

  await usersTable.update(user_id, { [IMAP_UID_VALIDITY]: uidValidity });

  return uidValidity;
};

/** The account a single sign-on subject is linked to, if any. */
export const findUserByAuthSub = async (sub: string): Promise<UserModel | null> =>
  usersTable.queryOne({ [AUTH_SUB]: sub });

/** The account with this username, an unfinished sign-up included, as a model. */
export const findUserByUsername = async (username: string): Promise<UserModel | null> =>
  usersTable.queryOne({ [USERNAME]: username });

/**
 * Direct SQL: the builder compares with `=` only, and this needs the address
 * matched ignoring case and the link column matched by `IS NULL`. An account
 * with no password is a sign-up still waiting for its link, not an account.
 * Two rows are enough to tell "exactly one" from "more than one".
 */
export const buildFindUnlinkedUsersByEmail = (email: string): PreparedQuery => ({
  sql: `SELECT * FROM ${USERS} WHERE LOWER(${EMAIL}) = LOWER($1) AND ${AUTH_SUB} IS NULL AND ${PASSWORD} IS NOT NULL AND ${SOFT_DELETE_CONDITION} LIMIT 2`,
  values: [email],
});

export const findUnlinkedUsersByEmail = async (email: string): Promise<UserModel[]> => {
  const { sql, values } = buildFindUnlinkedUsersByEmail(email);
  const result = await pool.query(sql, values);
  return result.rows.map((row: unknown) => new UserModel(row));
};

/**
 * The link's guard is `auth_sub IS NULL`, which keeps two sign-ins from
 * linking one account to two subjects; the column's UNIQUE constraint keeps
 * one subject off two accounts. A soft-deleted account is never linked.
 */
const linkArgs = (userId: string, sub: string): Parameters<typeof usersTable.updateWhere> => [
  { [USER_ID]: userId, [AUTH_SUB]: { op: "IS NULL" }, [IS_DELETED]: { op: "IS NOT TRUE" } },
  { [AUTH_SUB]: sub, updated: DB_NOW },
  ["*"],
];

/** The statement `linkUserAuthSub` runs, from the same arguments. */
export const buildLinkUserAuthSub = (userId: string, sub: string): PreparedQuery =>
  usersTable.prepareUpdateWhere(...linkArgs(userId, sub))!;

const isUniqueViolation = (error: unknown): boolean =>
  (error as { code?: unknown } | null)?.code === "23505";

/**
 * Links an account that is not linked yet.
 *
 * @returns the linked account, or null when another sign-in linked the
 * account, or this subject, first.
 */
export const linkUserAuthSub = async (userId: string, sub: string): Promise<UserModel | null> => {
  try {
    const [row] = await usersTable.updateWhere(...linkArgs(userId, sub));
    return row ? new UserModel(row) : null;
  } catch (error) {
    if (isUniqueViolation(error)) return null;
    throw error;
  }
};

interface SsoUserRow {
  username: string;
  email: string;
  sub: string;
  passwordHash: string;
}

/**
 * `ON CONFLICT DO NOTHING` covers both unique columns: a taken username and a
 * subject another sign-in has just linked both leave the table untouched and
 * return no row.
 */
const insertArgs = (account: SsoUserRow): Parameters<typeof usersTable.insert> => [
  {
    [USERNAME]: account.username,
    [PASSWORD]: account.passwordHash,
    [EMAIL]: account.email,
    [AUTH_SUB]: account.sub,
  },
  ["*"],
  { onConflictDoNothing: true },
];

/** The statement `insertSsoUser` runs, from the same arguments. */
export const buildInsertSsoUser = (account: SsoUserRow): PreparedQuery =>
  usersTable.prepareInsert(...insertArgs(account));

/**
 * Creates the account a first single sign-on makes. Its password is a hash of
 * a secret that exists nowhere, as for a revoked read-only account: no
 * password sign-in can present it, and every surface that reads the row as a
 * signed-up account keeps working.
 *
 * @returns the new account, or null when the username or the subject is taken.
 */
export const insertSsoUser = async (account: {
  username: string;
  email: string;
  sub: string;
}): Promise<UserModel | null> => {
  const passwordHash = await hashPassword(randomBytes(32).toString("hex"));
  const row = await usersTable.insert(...insertArgs({ ...account, passwordHash }));
  return row ? new UserModel(row) : null;
};

export const updateUserEmail = async (userId: string, email: string): Promise<void> => {
  await usersTable.update(userId, { [EMAIL]: email });
};

/**
 * Moves the linked account's credential generation on, the same fence a
 * password change moves: every HTTP session, IMAP connection and SMTP
 * connection stamped with an older generation fails its next check, in every
 * process. Direct SQL for the atomic increment, as in `rotateUserPassword`.
 */
export const buildBumpPasswordVersionByAuthSub = (sub: string): PreparedQuery => ({
  sql: `UPDATE ${USERS} SET ${PASSWORD_VERSION} = COALESCE(${PASSWORD_VERSION}, 1) + 1, updated = CURRENT_TIMESTAMP WHERE ${AUTH_SUB} = $1 RETURNING ${USER_ID}, ${USERNAME}, ${PASSWORD_VERSION}`,
  values: [sub],
});

/** @returns the account whose generation moved, or null when none is linked. */
export const bumpPasswordVersionByAuthSub = async (
  sub: string
): Promise<{ userId: string; username: string } | null> => {
  const { sql, values } = buildBumpPasswordVersionByAuthSub(sub);
  const result = await pool.query(sql, values);
  const row = result.rows[0];
  return row ? { userId: row[USER_ID], username: row[USERNAME] } : null;
};
