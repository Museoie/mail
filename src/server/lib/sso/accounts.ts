import { ADMIN_RO_USERNAME, ADMIN_USERNAME, isReservedUsername } from "../read-only";
import { getUserDomain, isRoutableUsername } from "../util";
import { isValidEmail } from "../users";
import {
  findUnlinkedUsersByEmail,
  findUserByAuthSub,
  findUserByUsername,
  insertSsoUser,
  linkUserAuthSub,
  updateUserEmail,
} from "../postgres/repositories/users";
import type { UserModel } from "../postgres/models";

/** A local account as the sign-in callback sees it. */
export interface LocalAccount {
  userId: string;
  username: string;
  email: string | null;
  authSub: string | null;
  passwordVersion: number;
}

/** The account reads and writes the callback makes, so the rules run without a database. */
export interface AccountDirectory {
  findBySub(sub: string): Promise<LocalAccount | null>;
  /** Any account with this username, linked or not. */
  findByUsername(username: string): Promise<LocalAccount | null>;
  /**
   * Signed-up accounts with this address ignoring case, not linked or deleted,
   * minus `excludedUsernames`.
   */
  findUnlinkedByEmail(email: string, excludedUsernames: string[]): Promise<LocalAccount[]>;
  /** Null when the account or the subject was linked by another sign-in first. */
  link(userId: string, sub: string): Promise<LocalAccount | null>;
  /** Null when the username or the subject is taken. */
  create(account: { username: string; email: string; sub: string }): Promise<LocalAccount | null>;
  updateEmail(userId: string, email: string): Promise<void>;
}

/** Who the provider says signed in: the access token's claims and the ID token's address. */
export interface SsoIdentity {
  sub: string;
  username: string;
  email?: string | null;
  emailVerified?: boolean;
}

export type SsoAccountRefusal = "account_exists" | "invalid_username";

export type SsoAccountResult =
  | { ok: true; account: LocalAccount; outcome: "existing" | "linked" | "created" }
  | { ok: false; reason: SsoAccountRefusal };

const EMAIL_MAX_LENGTH = 255;

const emailOf = ({ email }: SsoIdentity): string | undefined => {
  if (!email || email.length > EMAIL_MAX_LENGTH || !isValidEmail(email)) return undefined;
  return email;
};

/**
 * Finds the local account a single sign-on belongs to, linking or creating it
 * on the first one:
 *
 * 1. the account already linked to the subject;
 * 2. the account `linkUsers` names for the provider username, if not linked yet;
 * 3. the one signed-up account, not linked yet, whose address is the ID
 *    token's email, verified or not. This app's sign-up proved that address,
 *    and the provider sets an address a person chose only after they confirm
 *    it (one it has not verified was entered by its administrator, on account
 *    creation or import), so the match is as trustworthy as `linkUsers`. A
 *    provider that lets people claim addresses they have not confirmed must
 *    not be used this way. The reserved accounts never link this way:
 *    `admin-ro` is a local account only, and mail to `admin`'s address is
 *    readable by the read-only `admin-ro` login.
 * 4. a new account named after the provider username, unless that is not a
 *    routable mailbox name or an account by that name exists: guessing
 *    whose it is would hand someone else's mail over.
 *
 * A new account takes the token's email, verified or not. Each sign-in copies
 * a changed address onto the account only when the token marks it verified.
 *
 * Every link is a conditional write, so a sign-in that loses a race to link
 * the same account or subject takes the winner's result instead.
 */
export const resolveSsoAccount = async (
  identity: SsoIdentity,
  linkUsers: ReadonlyMap<string, string>,
  directory: AccountDirectory
): Promise<SsoAccountResult> => {
  const { sub, username } = identity;
  const email = emailOf(identity);
  const verifiedEmail = identity.emailVerified === true ? email : undefined;

  const withCurrentEmail = async (account: LocalAccount): Promise<LocalAccount> => {
    if (!verifiedEmail || account.email === verifiedEmail) return account;
    // The reserved accounts' address is derived from EMAIL_DOMAIN at boot.
    if (isReservedUsername(account.username)) return account;
    await directory.updateEmail(account.userId, verifiedEmail);
    return { ...account, email: verifiedEmail };
  };

  const found = async (account: LocalAccount, outcome: "existing" | "linked" | "created") => ({
    ok: true as const,
    account: await withCurrentEmail(account),
    outcome,
  });

  const tryLink = async (candidate: LocalAccount | null | undefined) => {
    if (!candidate || candidate.authSub !== null) return null;
    return (await directory.link(candidate.userId, sub)) ?? (await directory.findBySub(sub));
  };

  const existing = await directory.findBySub(sub);
  if (existing) return found(existing, "existing");

  const mapped = linkUsers.get(username);
  if (mapped !== undefined) {
    const linked = await tryLink(await directory.findByUsername(mapped));
    if (linked) return found(linked, "linked");
  }

  if (email) {
    const candidates = await directory.findUnlinkedByEmail(email, [
      ADMIN_USERNAME,
      ADMIN_RO_USERNAME,
    ]);
    if (candidates.length === 1) {
      const linked = await tryLink(candidates[0]);
      if (linked) return found(linked, "linked");
    }
  }

  if (!isRoutableUsername(username) || username === ADMIN_RO_USERNAME) {
    return { ok: false, reason: "invalid_username" };
  }
  if (await directory.findByUsername(username)) return { ok: false, reason: "account_exists" };
  const created = await directory.create({
    username,
    // Without an address in the token the account's own mailbox stands in:
    // the reset mail then reaches only its owner.
    email: email ?? `${username}@${getUserDomain(username)}`,
    sub,
  });
  if (created) return found(created, "created");
  const raced = await directory.findBySub(sub);
  return raced ? found(raced, "existing") : { ok: false, reason: "account_exists" };
};

const toAccount = (model: UserModel): LocalAccount => ({
  userId: model.user_id,
  username: model.username,
  email: model.email,
  authSub: model.auth_sub,
  passwordVersion: model.password_version ?? 1,
});

const toFoundAccount = (model: UserModel | null): LocalAccount | null =>
  model ? toAccount(model) : null;

export const postgresAccountDirectory: AccountDirectory = {
  findBySub: async (sub) => toFoundAccount(await findUserByAuthSub(sub)),
  findByUsername: async (username) => toFoundAccount(await findUserByUsername(username)),
  findUnlinkedByEmail: async (email, excludedUsernames) =>
    (await findUnlinkedUsersByEmail(email, excludedUsernames)).map(toAccount),
  link: async (userId, sub) => toFoundAccount(await linkUserAuthSub(userId, sub)),
  create: async (account) => toFoundAccount(await insertSsoUser(account)),
  updateEmail: updateUserEmail,
};
