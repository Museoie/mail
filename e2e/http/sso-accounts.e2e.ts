/**
 * Which local account a single sign-on gets: the one already linked to the
 * person, one `AUTH_LINK_USERS` names, the one with their email address, or a
 * new one — and the two refusals, `account_exists` and `invalid_username`.
 */
import { afterAll, beforeAll, beforeEach, describe, expect, test } from "bun:test";
import { createUser, startServer, waitFor, type ApiClient, type ApiResult, type TestServer } from "../harness";
import { person, signIn, startIssuer, type FakeIssuer, type Identity } from "../harness/issuer";

const CLEAR_LOGIN_COOKIE = "__Host-login=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0";

let issuer: FakeIssuer;
let server: TestServer;

beforeAll(async () => {
  issuer = await startIssuer();
  server = await startServer({
    env: issuer.env({
      // Spaces and empty entries are allowed; the local account comes first.
      AUTH_LINK_USERS: "admin=boss, carol = carol-sso ,,ghost=ghost-sso,",
      ADMIN_RO_PASSWORD: "read-only-password-e2e",
    }),
  });
}, 90_000);
afterAll(async () => {
  await server?.stop();
  await issuer?.stop();
});

beforeEach(async () => {
  await server.db.query("DELETE FROM sessions");
  await server.db.query("DELETE FROM users WHERE username NOT IN ('admin', 'admin-ro')");
  await server.db.query("UPDATE users SET auth_sub = NULL");
});

interface UserRow {
  user_id: string;
  username: string;
  email: string | null;
  auth_sub: string | null;
  password: string | null;
  password_version: number | null;
  token: string | null;
  is_deleted: boolean | null;
  updated: Date;
}

const userNamed = async (username: string) =>
  (await server.db.query<UserRow>("SELECT * FROM users WHERE username = $1", [username]))[0];
const usernames = async () =>
  (await server.db.query<{ username: string }>("SELECT username FROM users ORDER BY username")).map(
    (row) => row.username
  );
/** `username → subject` for every linked account. */
const links = async () =>
  Object.fromEntries(
    (
      await server.db.query<UserRow>("SELECT username, auth_sub FROM users WHERE auth_sub IS NOT NULL ORDER BY username")
    ).map((row) => [row.username, row.auth_sub])
  );
const sessions = () => server.db.query("SELECT session_user_id, session_username, session_email FROM sessions");

/** Signs `who` in and returns who the session says they are. */
const signedInAs = async (who: Identity, client: ApiClient = server.client()) => {
  const { done } = await signIn(client, issuer, { who });
  expect({ sub: who.sub, location: done.headers.get("location") }).toEqual({ sub: who.sub, location: "/" });
  const me = await client.get("/api/users/login");
  return me.body.body.user as { id: string; username: string; email: string; passwordVersion: number };
};

const expectRefusal = (done: ApiResult, code: string) => {
  expect({ status: done.status, location: done.headers.get("location") }).toEqual({
    status: 302,
    location: `/sign-in?sso_error=${code}`,
  });
  expect(done.headers.get("cache-control")).toBe("no-store");
  expect(done.setCookies).toEqual([CLEAR_LOGIN_COOKIE]);
};

describe("1. the account already linked to the person", () => {
  test("is signed in again, whatever else has changed since", async () => {
    const first = await signedInAs(person("alice"));
    expect(first.username).toBe("alice");

    // Another account now has her address, the link list now names her
    // username, and the provider renamed her: the subject still decides.
    await createUser(server, "lookalike", { email: "alice@people.example" });
    await createUser(server, "carol", { email: "carol@elsewhere.example" });
    const again = await signedInAs(person("carol-sso", { sub: "usr_alice", email: "alice@people.example" }));
    expect(again).toEqual(first);
    expect(await links()).toEqual({ alice: "usr_alice" });
    expect(await usernames()).toEqual(["admin", "admin-ro", "alice", "carol", "lookalike"]);
  });

  test("is found by subject alone, exactly as the provider spells it", async () => {
    await signedInAs(person("alice"));
    const other = await signedInAs(person("alice2", { sub: "USR_ALICE", email: null }));
    expect(other.username).toBe("alice2");
    expect(await links()).toEqual({ alice: "usr_alice", alice2: "USR_ALICE" });
  });
});

describe("2. the account AUTH_LINK_USERS names for the provider username", () => {
  test("is linked on the first sign-in, and is the account from then on", async () => {
    const admin = await userNamed("admin");
    const user = await signedInAs(person("boss"));
    expect(user).toEqual({ id: admin!.user_id, username: "admin", email: "admin@mail.test", passwordVersion: 1 });
    expect(await links()).toEqual({ admin: "usr_boss" });
    expect(await usernames()).toEqual(["admin", "admin-ro"]);
    // The reserved account keeps the address the boot gave it, verified or not.
    expect((await userNamed("admin"))!.email).toBe("admin@mail.test");

    expect((await signedInAs(person("boss"))).id).toBe(admin!.user_id);
  });

  test("wins over an account that has the person's address", async () => {
    const carol = await createUser(server, "carol", { email: "carol@elsewhere.example" });
    await createUser(server, "erin", { email: "carol-sso@people.example" });
    const user = await signedInAs(person("carol-sso"));
    expect(user.id).toBe(carol.id);
    expect(user.username).toBe("carol");
    expect(await links()).toEqual({ carol: "usr_carol-sso" });
  });

  test("is skipped when it is linked to someone else already", async () => {
    await createUser(server, "carol");
    await server.db.query("UPDATE users SET auth_sub = 'usr_someone' WHERE username = 'carol'");
    const erin = await createUser(server, "erin", { email: "CAROL-SSO@people.example" });
    // The next rule applies: the account with her address.
    expect((await signedInAs(person("carol-sso"))).id).toBe(erin.id);
    expect(await links()).toEqual({ carol: "usr_someone", erin: "usr_carol-sso" });
  });

  test("is skipped when no such account exists, or it is deleted", async () => {
    const created = await signedInAs(person("ghost-sso"));
    expect(created.username).toBe("ghost-sso");
    expect(await usernames()).toEqual(["admin", "admin-ro", "ghost-sso"]);

    await createUser(server, "carol");
    await server.db.query("UPDATE users SET is_deleted = TRUE WHERE username = 'carol'");
    expect((await signedInAs(person("carol-sso"))).username).toBe("carol-sso");
    expect((await userNamed("carol"))!.auth_sub).toBeNull();
  });

  test("goes by the provider username to the letter", async () => {
    // "Boss" is not "boss": no link, and not a name a mailbox can have either.
    const { done } = await signIn(server.client(), issuer, { who: person("Boss") });
    expectRefusal(done, "invalid_username");
    expect(await links()).toEqual({});
  });
});

describe("3. the one account with the person's email address", () => {
  test("is linked, ignoring case, and takes the address as the provider verified it", async () => {
    const bob = await createUser(server, "bob", { email: "Alice@People.Example" });
    const user = await signedInAs(person("alice"));
    expect(user).toEqual({ id: bob.id, username: "bob", email: "alice@people.example", passwordVersion: 1 });
    expect(await links()).toEqual({ bob: "usr_alice" });
    expect((await userNamed("bob"))!.email).toBe("alice@people.example");
    expect(await usernames()).toEqual(["admin", "admin-ro", "bob"]);
    expect(await sessions()).toEqual([
      { session_user_id: bob.id, session_username: "bob", session_email: "alice@people.example" },
    ]);
  });

  test("is linked by an address the provider has not verified, and keeps its own", async () => {
    for (const emailVerified of [false, undefined]) {
      await server.db.query("DELETE FROM users WHERE username = 'bob'");
      const bob = await createUser(server, "bob", { email: "Alice@People.Example" });
      const user = await signedInAs(person("alice", { emailVerified }));
      expect(user).toEqual({ id: bob.id, username: "bob", email: "Alice@People.Example", passwordVersion: 1 });
      expect((await userNamed("bob"))!.email).toBe("Alice@People.Example");
      expect(await links()).toEqual({ bob: "usr_alice" });
    }
  });

  test("is not guessed between two accounts with the address: a new account is made", async () => {
    for (const emailVerified of [true, false]) {
      await server.db.query("DELETE FROM users WHERE username IN ('alice', 'bob', 'carla')");
      await createUser(server, "bob", { email: "alice@people.example" });
      await createUser(server, "carla", { email: "ALICE@people.example" });
      const user = await signedInAs(person("alice", { emailVerified }));
      expect(user.username).toBe("alice");
      expect(await links()).toEqual({ alice: "usr_alice" });
    }
  });

  test("is never a reserved account", async () => {
    const boss = await signedInAs(person("zed", { email: "admin@mail.test" }));
    expect(boss.username).toBe("zed");
    const readOnly = await signedInAs(person("yan", { email: "ADMIN-RO@mail.test" }));
    expect(readOnly.username).toBe("yan");
    expect(await links()).toEqual({ yan: "usr_yan", zed: "usr_zed" });
    // With one other account sharing a reserved account's address, that one is the only candidate.
    const twin = await createUser(server, "twin", { email: "admin@mail.test" });
    expect((await signedInAs(person("walt", { email: "admin@mail.test" }))).id).toBe(twin.id);
  });

  test("is never an unfinished sign-up, a deleted account, or one linked already", async () => {
    await server.db.query(
      "INSERT INTO users (username, email, token, expiry) VALUES ('user_1234abcd', 'alice@people.example', 'tok', now() + interval '1 hour')"
    );
    await createUser(server, "gone", { email: "alice@people.example" });
    await server.db.query("UPDATE users SET is_deleted = TRUE WHERE username = 'gone'");
    await createUser(server, "taken", { email: "alice@people.example" });
    await server.db.query("UPDATE users SET auth_sub = 'usr_other' WHERE username = 'taken'");

    const user = await signedInAs(person("alice"));
    expect(user.username).toBe("alice");
    expect(await links()).toEqual({ alice: "usr_alice", taken: "usr_other" });
  });

  test("needs an address in the token: none, or one that is not an address, matches nothing", async () => {
    await createUser(server, "bob", { email: "not an address" });
    await createUser(server, "empty", { email: "" });
    for (const [username, email] of [
      ["n1", null],
      ["n2", undefined],
      ["n3", ""],
      ["n4", "not an address"],
    ] as const) {
      const user = await signedInAs(person(username, { email }));
      expect({ username, got: user.username }).toEqual({ username, got: username });
    }
    expect(await links()).toEqual({ n1: "usr_n1", n2: "usr_n2", n3: "usr_n3", n4: "usr_n4" });
  });
});

describe("4. a new account named after the provider username", () => {
  test("is created with the token's address, verified or not", async () => {
    for (const [username, emailVerified] of [
      ["vera", true],
      ["ulla", false],
      ["tess", undefined],
    ] as const) {
      const user = await signedInAs(person(username, { emailVerified }));
      const row = await userNamed(username);
      expect(user).toEqual({
        id: row!.user_id,
        username,
        email: `${username}@people.example`,
        passwordVersion: 1,
      });
      expect(row!.auth_sub).toBe(`usr_${username}`);
      expect(row!.email).toBe(`${username}@people.example`);
      expect(row!.password_version).toBe(1);
      expect(row!.token).toBeNull();
      expect(row!.is_deleted).not.toBe(true);
      // It reads as a signed-up account: it has a password, one nobody knows.
      expect(row!.password).toMatch(/^\$2[aby]\$/);
    }
  });

  test("takes its own mailbox as its address when the token has no usable one", async () => {
    const tooLong = `${"b".repeat(244)}@example.com`;
    expect(tooLong.length).toBe(256);
    for (const [username, email] of [
      ["m1", null],
      ["m2", undefined],
      ["m3", ""],
      ["m4", "not an address"],
      ["m5", "two@at@signs.example"],
      ["m6", "nodot@localhost"],
      ["m7", tooLong],
      ["m8", 0],
      ["m9", false],
    ] as const) {
      const user = await signedInAs(person(username, { email: email as string | null | undefined }));
      expect({ username, email: user.email }).toEqual({ username, email: `${username}@${username}.mail.test` });
      expect((await userNamed(username))!.email).toBe(`${username}@${username}.mail.test`);
    }
    // The longest address the column takes is used as it is.
    const longest = `${"b".repeat(243)}@example.com`;
    expect((await signedInAs(person("m10", { email: longest }))).email).toBe(longest);
  });

  test("cannot be signed in to with a password", async () => {
    await signedInAs(person("vera"));
    await server.restart({ AUTH_LOCAL_LOGIN: "true" });
    try {
      for (const password of ["", "vera", "password", "usr_vera"]) {
        const attempt = await server.client().post("/api/users/login", { username: "vera", password });
        expect(attempt.body).toEqual({ status: "failed", message: "Invalid credentials." });
      }
    } finally {
      await server.restart({ AUTH_LOCAL_LOGIN: undefined });
    }
  }, 90_000);

  test("is refused as `invalid_username` when the name cannot be a mailbox", async () => {
    for (const username of [
      "Alice",
      "al_ice",
      "al.ice",
      "-alice",
      "alice-",
      "al ice",
      "alice@people.example",
      "ålice",
      "",
      "a".repeat(64),
      "admin-ro",
    ]) {
      const { done } = await signIn(server.client(), issuer, { who: person(username, { sub: "usr_odd" }) });
      expect({ username, location: done.headers.get("location") }).toEqual({
        username,
        location: "/sign-in?sso_error=invalid_username",
      });
      expectRefusal(done, "invalid_username");
    }
    expect(await usernames()).toEqual(["admin", "admin-ro"]);
    expect(await links()).toEqual({});
    expect(await sessions()).toEqual([]);

    // The longest and the shortest names a mailbox can have are fine.
    expect((await signedInAs(person("a".repeat(63), { email: null }))).username).toBe("a".repeat(63));
    expect((await signedInAs(person("7", { email: null }))).username).toBe("7");
    expect((await signedInAs(person("a-1", { email: null }))).username).toBe("a-1");
  });

  test("is refused as `account_exists` when an account has the name but is not theirs", async () => {
    await createUser(server, "alice", { email: "someone@elsewhere.example" });
    const unlinked = await signIn(server.client(), issuer, { who: person("alice") });
    expectRefusal(unlinked.done, "account_exists");

    // The built-in admin without a link entry, an account linked to someone
    // else, and a deleted account that still holds the name.
    const admin = await signIn(server.client(), issuer, { who: person("admin", { email: null }) });
    expectRefusal(admin.done, "account_exists");
    await server.db.query("UPDATE users SET auth_sub = 'usr_other' WHERE username = 'alice'");
    const linked = await signIn(server.client(), issuer, { who: person("alice") });
    expectRefusal(linked.done, "account_exists");
    await createUser(server, "gone");
    await server.db.query("UPDATE users SET is_deleted = TRUE WHERE username = 'gone'");
    const deleted = await signIn(server.client(), issuer, { who: person("gone") });
    expectRefusal(deleted.done, "account_exists");

    expect(await links()).toEqual({ alice: "usr_other" });
    expect(await usernames()).toEqual(["admin", "admin-ro", "alice", "gone"]);
    expect(await sessions()).toEqual([]);
    expect((await userNamed("alice"))!.email).toBe("someone@elsewhere.example");
  });
});

describe("the account's address at each sign-in", () => {
  test("follows a changed address only when the provider verified it", async () => {
    const first = await signedInAs(person("alice", { email: "old@people.example" }));
    expect(first.email).toBe("old@people.example");

    const unverified = await signedInAs(person("alice", { email: "new@people.example", emailVerified: false }));
    expect(unverified.email).toBe("old@people.example");
    expect((await userNamed("alice"))!.email).toBe("old@people.example");

    const missingFlag = await signedInAs(person("alice", { email: "new@people.example", emailVerified: undefined }));
    expect(missingFlag.email).toBe("old@people.example");

    const verified = await signedInAs(person("alice", { email: "new@people.example" }));
    expect(verified.email).toBe("new@people.example");
    expect((await userNamed("alice"))!.email).toBe("new@people.example");

    for (const email of ["not an address", "", null, `${"b".repeat(244)}@example.com`]) {
      const kept = await signedInAs(person("alice", { email }));
      expect({ email, kept: kept.email }).toEqual({ email, kept: "new@people.example" });
    }
    expect((await userNamed("alice"))!.email).toBe("new@people.example");
    expect(verified.id).toBe(first.id);
  });

  test("is only believed verified when the claim is exactly `true`", async () => {
    await signedInAs(person("alice", { email: "old@people.example" }));
    for (const emailVerified of ["true", 1, {}] as unknown as boolean[]) {
      const user = await signedInAs(person("alice", { email: "new@people.example", emailVerified }));
      expect(user.email).toBe("old@people.example");
    }
  });

  test("leaves the row untouched when the address is the same", async () => {
    await signedInAs(person("alice"));
    const before = await userNamed("alice");
    await signedInAs(person("alice"));
    expect((await userNamed("alice"))!.updated).toEqual(before!.updated);
  });

  test("never changes on a reserved account", async () => {
    await signedInAs(person("boss", { email: "boss@people.example" }));
    const again = await signedInAs(person("boss", { email: "boss2@people.example" }));
    expect(again.email).toBe("admin@mail.test");
    expect((await userNamed("admin"))!.email).toBe("admin@mail.test");
  });

  // NOTE: an `email` claim that is neither a string nor empty-ish (a number, a
  // list, an object, `true`) is not treated as "no address": the sign-in fails
  // with `failed` and raises the alarm, because the address check calls a
  // string method on it. `0`, `false`, `null` and `""` do read as no address.
  test("an email claim of another JSON type fails the sign-in with `failed`", async () => {
    for (const email of [42, true, ["alice@people.example"], { address: "alice@people.example" }]) {
      const { done } = await signIn(server.client(), issuer, {
        who: person("alice", { email: email as unknown as string }),
      });
      expect({ email, location: done.headers.get("location") }).toEqual({
        email,
        location: "/sign-in?sso_error=failed",
      });
      expectRefusal(done, "failed");
    }
    expect(await usernames()).toEqual(["admin", "admin-ro"]);
    expect(await sessions()).toEqual([]);
    const alarm = await waitFor(() => server.alarms.alarms[0], { what: "the sign-on alarm" });
    expect(alarm.title).toBe("🚨 Mail Server Error: Single sign-on failed");
  });
});
