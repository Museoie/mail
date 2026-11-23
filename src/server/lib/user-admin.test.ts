import { afterAll, beforeEach, describe, expect, it } from "bun:test";
import {
  addDomain,
  createUser,
  disableUser,
  setPassword,
  UserAdminError,
  type UserAdminStore,
} from "./user-admin";

/** The accounts and domain rows the commands touch, in memory, with the same conditional writes. */
class MemoryStore implements UserAdminStore {
  users = new Map<string, { user_id: string; username: string; password: string; email: string }>();
  domains = new Map<string, string>();
  sessionsDeleted: string[] = [];
  rotations: Array<[string, string, string]> = [];

  findUserByUsername = async (username: string) => this.users.get(username) ?? null;
  createUser = async (user: { username: string; password: string; email: string }) => {
    const row = { user_id: `u-${user.username}`, ...user };
    this.users.set(user.username, row);
    return row.user_id;
  };
  assignDomain = async (userId: string, name: string) => {
    if (!this.domains.has(name)) this.domains.set(name, userId);
    return this.domains.get(name) === userId;
  };
  rotatePassword = async (userId: string, username: string, password: string) => {
    this.rotations.push([userId, username, password]);
    return 2;
  };
  deleteSessions = async (userId: string) => {
    this.sessionsDeleted.push(userId);
    return 1;
  };
}

const originalDomain = process.env.EMAIL_DOMAIN;
afterAll(() => {
  if (originalDomain === undefined) delete process.env.EMAIL_DOMAIN;
  else process.env.EMAIL_DOMAIN = originalDomain;
});

let store: MemoryStore;
beforeEach(() => {
  process.env.EMAIL_DOMAIN = "example.com";
  store = new MemoryStore();
});

describe("createUser", () => {
  it("creates a password account that owns its own name under the zone", async () => {
    const created = await createUser({ username: "alice", password: "long-enough" }, store);

    expect(created).toEqual({
      userId: "u-alice",
      username: "alice",
      email: "alice@alice.example.com",
      domains: ["alice.example.com"],
    });
    // The password goes to the store as given; `writeUser` hashes it.
    expect(store.users.get("alice")?.password).toBe("long-enough");
    expect(store.domains.get("alice.example.com")).toBe("u-alice");
  });

  it("gives the account the domains asked for, such as the bare zone", async () => {
    const created = await createUser(
      { username: "alice", password: "long-enough", email: "alice@example.org", domains: ["Example.com", "example.org"] },
      store
    );
    expect(created.email).toBe("alice@example.org");
    expect(created.domains).toEqual(["alice.example.com", "example.com", "example.org"]);
  });

  it("refuses an unroutable or taken username, a short password and a bad address", async () => {
    await createUser({ username: "alice", password: "long-enough" }, store);
    const attempts = [
      { username: "Alice", password: "long-enough" },
      { username: "al_ice", password: "long-enough" },
      { username: "alice", password: "long-enough" },
      { username: "bob", password: "short" },
      { username: "bob", password: "long-enough", email: "not an address" },
      { username: "bob", password: "long-enough", domains: ["not a domain"] },
    ];
    for (const input of attempts) {
      await expect(createUser(input, store)).rejects.toBeInstanceOf(UserAdminError);
    }
    expect([...store.users.keys()]).toEqual(["alice"]);
  });

  it("says which domains already belong to someone else", async () => {
    store.domains.set("example.org", "u-someone");
    await expect(
      createUser({ username: "alice", password: "long-enough", domains: ["example.org"] }, store)
    ).rejects.toThrow("example.org already belong to another account");
    expect(store.domains.get("example.org")).toBe("u-someone");
  });
});

describe("setPassword", () => {
  it("deletes the account's sessions and rotates its password", async () => {
    await createUser({ username: "alice", password: "long-enough" }, store);
    await setPassword("alice", "another-one", store);
    expect(store.sessionsDeleted).toEqual(["u-alice"]);
    expect(store.rotations).toEqual([["u-alice", "alice", "another-one"]]);
  });

  it("refuses a missing account and a short password", async () => {
    await expect(setPassword("nobody", "long-enough", store)).rejects.toBeInstanceOf(UserAdminError);
    await createUser({ username: "alice", password: "long-enough" }, store);
    await expect(setPassword("alice", "short", store)).rejects.toBeInstanceOf(UserAdminError);
    expect(store.rotations).toEqual([]);
  });
});

describe("addDomain", () => {
  it("gives an existing account a domain nobody owns", async () => {
    await createUser({ username: "alice", password: "long-enough" }, store);
    expect(await addDomain("alice", "Example.NET", store)).toBe("example.net");
    expect(store.domains.get("example.net")).toBe("u-alice");
  });

  it("refuses a domain someone else owns, and a missing account", async () => {
    await createUser({ username: "alice", password: "long-enough" }, store);
    store.domains.set("example.net", "u-someone");
    await expect(addDomain("alice", "example.net", store)).rejects.toBeInstanceOf(UserAdminError);
    await expect(addDomain("nobody", "example.org", store)).rejects.toBeInstanceOf(UserAdminError);
  });
});

describe("disableUser", () => {
  it("replaces the password with one nobody knows and ends the sessions", async () => {
    await createUser({ username: "admin-ro", password: "long-enough" }, store);
    await disableUser("admin-ro", store);
    expect(store.sessionsDeleted).toEqual(["u-admin-ro"]);
    const [[userId, username, password]] = store.rotations;
    expect([userId, username]).toEqual(["u-admin-ro", "admin-ro"]);
    expect(password).toMatch(/^[0-9a-f]{64}$/);
  });

  it("refuses a missing account", async () => {
    await expect(disableUser("nobody", store)).rejects.toBeInstanceOf(UserAdminError);
    expect(store.rotations).toEqual([]);
  });
});
