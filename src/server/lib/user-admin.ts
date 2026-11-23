/**
 * Creating accounts and setting their passwords from the command line, for a
 * server that runs without single sign-on. Nothing seeds an account at boot:
 * the operator creates the first one (and any other) with `scripts/user.ts`.
 */

import { randomBytes } from "crypto";
import { isValidEmail } from "./users";
import { isRoutableUsername } from "./util";
import { accountDomain, normalizeDomainName } from "./domains";
import { hashPassword } from "./verify-password";
import {
  findUserByUsername,
  rotateUserPassword,
  writeUser,
} from "./postgres/repositories/users";
import { deleteSessionsForUser } from "./postgres/repositories/sessions";
import { assignDomain } from "./postgres/repositories/domains";

/** Shortest password the script accepts. */
export const MIN_PASSWORD_LENGTH = 8;

export interface CreateUserInput {
  username: string;
  password: string;
  /** The address password resets go to. Default: the account's own mailbox. */
  email?: string;
  /**
   * Domains to give the account besides its own name under the served zone,
   * without a DNS check: the operator vouches for them. For example the bare
   * served domain, for the account that receives `*@<EMAIL_DOMAIN>`.
   */
  domains?: string[];
}

/** The reads and writes the commands make, so their rules run without a database. */
export interface UserAdminStore {
  findUserByUsername(username: string): Promise<{ user_id: string; username: string } | null>;
  createUser(user: { username: string; password: string; email: string }): Promise<string | undefined>;
  assignDomain(userId: string, name: string): Promise<boolean>;
  rotatePassword(userId: string, username: string, password: string): Promise<number | null>;
  deleteSessions(userId: string): Promise<number>;
}

export const postgresUserAdminStore: UserAdminStore = {
  findUserByUsername,
  createUser: async (user) => (await writeUser(user))?._id,
  assignDomain,
  rotatePassword: async (userId, username, password) =>
    rotateUserPassword(userId, {
      passwordHash: await hashPassword(password),
      username,
      token: null,
      expiry: null,
    }),
  deleteSessions: deleteSessionsForUser,
};

export class UserAdminError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "UserAdminError";
  }
}

const checkPassword = (password: string) => {
  if (password.length < MIN_PASSWORD_LENGTH) {
    throw new UserAdminError(`The password must be at least ${MIN_PASSWORD_LENGTH} characters.`);
  }
};

export interface CreatedUser {
  userId: string;
  username: string;
  email: string;
  domains: string[];
}

/**
 * Creates an account that signs in with a password, and gives it its own name
 * under the served zone plus any `domains` asked for. Refuses a username that
 * cannot be a mailbox name, one that is taken, and a domain someone else owns.
 */
export const createUser = async (
  input: CreateUserInput,
  store: UserAdminStore = postgresUserAdminStore
): Promise<CreatedUser> => {
  const username = input.username.trim();
  if (!isRoutableUsername(username)) {
    throw new UserAdminError(
      "The username must be one lower-case DNS label: letters, digits and hyphens."
    );
  }
  checkPassword(input.password);

  const own = accountDomain(username);
  const email = input.email?.trim() || `${username}@${own}`;
  if (!isValidEmail(email)) throw new UserAdminError(`${email} is not an email address.`);

  const extra: string[] = [];
  for (const raw of input.domains ?? []) {
    const name = normalizeDomainName(raw);
    if (!name) throw new UserAdminError(`${raw} is not a domain name.`);
    if (name !== own && !extra.includes(name)) extra.push(name);
  }

  if (await store.findUserByUsername(username)) {
    throw new UserAdminError(`An account named ${username} already exists.`);
  }
  const userId = await store.createUser({ username, password: input.password, email });
  if (!userId) throw new UserAdminError("The account could not be created.");

  const domains: string[] = [];
  const taken: string[] = [];
  for (const name of [own, ...extra]) {
    if (await store.assignDomain(userId, name)) domains.push(name);
    else taken.push(name);
  }
  if (taken.length) {
    throw new UserAdminError(
      `${username} was created, but ${taken.join(", ")} already belong to another account.`
    );
  }
  return { userId, username, email, domains };
};

/**
 * Replaces an account's password, the recovery path for a lost one. The
 * credential generation moves in the same write and the account's web
 * sessions are deleted, so everything signed in with the old password has to
 * sign in again; mail apps lose their connection at their next command.
 */
export const setPassword = async (
  username: string,
  password: string,
  store: UserAdminStore = postgresUserAdminStore
): Promise<void> => {
  checkPassword(password);
  const user = await store.findUserByUsername(username.trim());
  if (!user) throw new UserAdminError(`There is no account named ${username}.`);
  await store.deleteSessions(user.user_id);
  const version = await store.rotatePassword(user.user_id, user.username, password);
  if (version === null) throw new UserAdminError(`There is no account named ${username}.`);
};

/**
 * Disables an account's password sign-in: the password is replaced with one
 * that exists nowhere, in the same write that moves the credential generation,
 * and the account's web sessions are deleted. Its mail apps lose their
 * connection at their next command. The account and its mail stay;
 * `setPassword` brings it back. (This is how the read-only `admin-ro` login an
 * earlier version seeded is retired.)
 */
export const disableUser = async (
  username: string,
  store: UserAdminStore = postgresUserAdminStore
): Promise<void> => {
  const user = await store.findUserByUsername(username.trim());
  if (!user) throw new UserAdminError(`There is no account named ${username}.`);
  await store.deleteSessions(user.user_id);
  const version = await store.rotatePassword(
    user.user_id,
    user.username,
    randomBytes(32).toString("hex")
  );
  if (version === null) throw new UserAdminError(`There is no account named ${username}.`);
};

/**
 * Gives an existing account a domain without a DNS check, as `createUser`
 * does for a new one. Refuses a domain someone else owns.
 */
export const addDomain = async (
  username: string,
  rawDomain: string,
  store: UserAdminStore = postgresUserAdminStore
): Promise<string> => {
  const name = normalizeDomainName(rawDomain);
  if (!name) throw new UserAdminError(`${rawDomain} is not a domain name.`);
  const user = await store.findUserByUsername(username.trim());
  if (!user) throw new UserAdminError(`There is no account named ${username}.`);
  if (!(await store.assignDomain(user.user_id, name))) {
    throw new UserAdminError(`${name} already belongs to another account.`);
  }
  return name;
};
