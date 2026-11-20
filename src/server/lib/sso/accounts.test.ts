import { describe, expect, it } from "bun:test";
import { getUserDomain } from "../util";
import {
  resolveSsoAccount,
  type AccountDirectory,
  type LocalAccount,
  type SsoIdentity,
} from "./accounts";

interface Row extends LocalAccount {
  /** False for a sign-up still waiting for its link: it has no password. */
  signedUp: boolean;
  deleted: boolean;
}

const view = ({ signedUp: _signedUp, deleted: _deleted, ...account }: Row): LocalAccount =>
  account;
const viewOf = (row: Row | undefined): LocalAccount | null => (row ? view(row) : null);

/**
 * The users table as the rules see it, with the same conditional writes:
 * a link only lands on an account that is not linked yet, and a subject or
 * username is unique.
 */
class MemoryDirectory implements AccountDirectory {
  readonly rows: Row[] = [];
  readonly writes: string[] = [];
  private next = 1;

  add(row: Partial<Row> & { username: string }): Row {
    const full: Row = {
      userId: `u${this.next++}`,
      email: null,
      authSub: null,
      passwordVersion: 1,
      signedUp: true,
      deleted: false,
      ...row,
    };
    this.rows.push(full);
    return full;
  }

  findBySub = async (sub: string) => viewOf(this.rows.find((row) => row.authSub === sub));
  findByUsername = async (username: string) =>
    viewOf(this.rows.find((row) => row.username === username));
  findUnlinkedByEmail = async (email: string, excluded: string[]) =>
    this.rows
      .filter(
        (row) =>
          row.email?.toLowerCase() === email.toLowerCase() &&
          row.authSub === null &&
          row.signedUp &&
          !row.deleted &&
          !excluded.includes(row.username)
      )
      .slice(0, 2)
      .map(view);
  link = async (userId: string, sub: string) => {
    this.writes.push(`link ${userId} ${sub}`);
    const row = this.rows.find((r) => r.userId === userId);
    if (!row || row.authSub !== null || this.rows.some((r) => r.authSub === sub)) return null;
    row.authSub = sub;
    return view(row);
  };
  create = async (account: { username: string; email: string; sub: string }) => {
    this.writes.push(`create ${account.username}`);
    if (this.rows.some((r) => r.username === account.username || r.authSub === account.sub)) {
      return null;
    }
    return view(this.add({ username: account.username, email: account.email, authSub: account.sub }));
  };
  updateEmail = async (userId: string, email: string) => {
    this.writes.push(`email ${userId} ${email}`);
    const row = this.rows.find((r) => r.userId === userId);
    if (row) row.email = email;
  };
}

const ALICE: SsoIdentity = {
  sub: "usr_alice",
  username: "alice",
  email: "alice@example.com",
  emailVerified: true,
};

const NO_LINKS = new Map<string, string>();

describe("resolveSsoAccount", () => {
  it("1. signs in the account already linked to the subject", async () => {
    const dir = new MemoryDirectory();
    const linked = dir.add({ username: "alice", authSub: "usr_alice", email: "alice@example.com" });
    dir.add({ username: "other", email: "alice@example.com" });
    const result = await resolveSsoAccount(ALICE, NO_LINKS, dir);
    expect(result).toEqual({ ok: true, account: view(linked), outcome: "existing" });
    expect(dir.writes).toEqual([]);
  });

  it("2. links the account AUTH_LINK_USERS names for the auth username", async () => {
    const dir = new MemoryDirectory();
    const admin = dir.add({ username: "admin", email: "admin@example.test" });
    const result = await resolveSsoAccount(ALICE, new Map([["alice", "admin"]]), dir);
    expect(result.ok && result.account.userId).toBe(admin.userId);
    expect(result.ok && result.outcome).toBe("linked");
    expect(admin.authSub).toBe("usr_alice");
    // The reserved account keeps the address its boot seeds.
    expect(admin.email).toBe("admin@example.test");
  });

  it("2. skips a named account that is linked already, and goes on to the next rule", async () => {
    const dir = new MemoryDirectory();
    dir.add({ username: "admin", authSub: "usr_someone" });
    const bob = dir.add({ username: "bob", email: "ALICE@example.com" });
    const result = await resolveSsoAccount(ALICE, new Map([["alice", "admin"]]), dir);
    expect(result.ok && result.account.userId).toBe(bob.userId);
  });

  it("3. links the one signed-up account with the verified address, ignoring case", async () => {
    const dir = new MemoryDirectory();
    const bob = dir.add({ username: "bob", email: "Alice@Example.com" });
    const result = await resolveSsoAccount(ALICE, NO_LINKS, dir);
    expect(result).toEqual({
      ok: true,
      account: { ...view(bob), email: "alice@example.com" },
      outcome: "linked",
    });
    expect(dir.writes).toEqual([`link ${bob.userId} usr_alice`, `email ${bob.userId} alice@example.com`]);
  });

  it("3. links by an address the provider has not verified, keeping the account's own", async () => {
    for (const emailVerified of [false, undefined]) {
      const dir = new MemoryDirectory();
      const bob = dir.add({ username: "bob", email: "Alice@Example.com" });
      const result = await resolveSsoAccount({ ...ALICE, emailVerified }, NO_LINKS, dir);
      expect(result).toEqual({
        ok: true,
        account: {
          userId: bob.userId,
          username: "bob",
          email: "Alice@Example.com",
          authSub: "usr_alice",
          passwordVersion: 1,
        },
        outcome: "linked",
      });
      expect(dir.writes).toEqual([`link ${bob.userId} usr_alice`]);
    }
  });

  it("3. does not guess between two accounts with the address, verified or not", async () => {
    for (const emailVerified of [true, false]) {
      const dir = new MemoryDirectory();
      dir.add({ username: "bob", email: "alice@example.com" });
      dir.add({ username: "carol", email: "ALICE@example.com" });
      const result = await resolveSsoAccount({ ...ALICE, emailVerified }, NO_LINKS, dir);
      expect(result.ok && result.outcome).toBe("created");
      expect(dir.rows.filter((r) => r.authSub === "usr_alice").map((r) => r.username)).toEqual([
        "alice",
      ]);
    }
  });

  it("3. never links a reserved, unfinished or deleted account, verified or not", async () => {
    const candidates = [
      { username: "admin" },
      { username: "admin-ro" },
      { username: "user_1234abcd", signedUp: false },
      { username: "dave", deleted: true },
    ];
    for (const emailVerified of [true, false]) {
      for (const candidate of candidates) {
        const dir = new MemoryDirectory();
        dir.add({ ...candidate, email: "alice@example.com" });
        const result = await resolveSsoAccount({ ...ALICE, emailVerified }, NO_LINKS, dir);
        expect(result.ok && result.outcome).toBe("created");
        expect(dir.rows.filter((r) => r.authSub).map((r) => r.username)).toEqual(["alice"]);
      }
    }
  });

  it("4. creates an account named after the username, with the verified address", async () => {
    const dir = new MemoryDirectory();
    const result = await resolveSsoAccount(ALICE, NO_LINKS, dir);
    expect(result).toEqual({
      ok: true,
      outcome: "created",
      account: {
        userId: "u1",
        username: "alice",
        email: "alice@example.com",
        authSub: "usr_alice",
        passwordVersion: 1,
      },
    });
  });

  it("4. gives a new account the address the provider has not verified", async () => {
    for (const emailVerified of [false, undefined]) {
      const dir = new MemoryDirectory();
      const result = await resolveSsoAccount({ ...ALICE, emailVerified }, NO_LINKS, dir);
      expect(result.ok && result.account.email).toBe("alice@example.com");
      expect(dir.rows.map((r) => [r.username, r.email])).toEqual([["alice", "alice@example.com"]]);
      expect(dir.writes).toEqual(["create alice"]);
    }
  });

  it("4. gives an account without a usable address its own mailbox as its address", async () => {
    for (const email of [null, undefined, "", "not an address", `${"b".repeat(244)}@example.com`]) {
      const dir = new MemoryDirectory();
      const result = await resolveSsoAccount(
        { sub: "usr_bob", username: "bob", email, emailVerified: false },
        NO_LINKS,
        dir
      );
      expect(result.ok && result.account.email).toBe(`bob@${getUserDomain("bob")}`);
    }
  });

  it("4. refuses a username that cannot name a mailbox", async () => {
    for (const username of ["Alice", "al_ice", "-alice", "alice-", "a".repeat(64), "admin-ro"]) {
      const dir = new MemoryDirectory();
      const result = await resolveSsoAccount({ ...ALICE, username }, NO_LINKS, dir);
      expect(result).toEqual({ ok: false, reason: "invalid_username" });
      expect(dir.writes).toEqual([]);
    }
  });

  it("4. refuses to take over an account that has the username but is not linked", async () => {
    for (const username of ["alice", "admin"]) {
      const dir = new MemoryDirectory();
      dir.add({ username, email: "someone@example.com" });
      const result = await resolveSsoAccount({ ...ALICE, username }, NO_LINKS, dir);
      expect(result).toEqual({ ok: false, reason: "account_exists" });
      expect(dir.writes).toEqual([]);
    }
  });

  it("takes the winner of a race to link the same subject", async () => {
    const dir = new MemoryDirectory();
    const bob = dir.add({ username: "bob", email: "alice@example.com" });
    dir.link = async () => {
      bob.authSub = "usr_alice";
      return null;
    };
    const result = await resolveSsoAccount(ALICE, NO_LINKS, dir);
    expect(result.ok && result.account.userId).toBe(bob.userId);
  });

  it("takes the winner of a race to create the account, or refuses when another took the name", async () => {
    const dir = new MemoryDirectory();
    dir.create = async () => {
      dir.add({ username: "alice", authSub: "usr_alice" });
      return null;
    };
    const won = await resolveSsoAccount(ALICE, NO_LINKS, dir);
    expect(won.ok && won.outcome).toBe("existing");

    const lost = new MemoryDirectory();
    lost.create = async () => null;
    expect(await resolveSsoAccount(ALICE, NO_LINKS, lost)).toEqual({
      ok: false,
      reason: "account_exists",
    });
  });

  it("copies a changed verified address at each sign-in, and nothing unverified", async () => {
    const dir = new MemoryDirectory();
    const alice = dir.add({ username: "alice", authSub: "usr_alice", email: "old@example.com" });
    await resolveSsoAccount({ ...ALICE, emailVerified: false, email: "new@example.com" }, NO_LINKS, dir);
    expect(alice.email).toBe("old@example.com");
    await resolveSsoAccount({ ...ALICE, email: "new@example.com" }, NO_LINKS, dir);
    expect(alice.email).toBe("new@example.com");
    await resolveSsoAccount({ ...ALICE, email: "not an address" }, NO_LINKS, dir);
    expect(alice.email).toBe("new@example.com");
  });
});
