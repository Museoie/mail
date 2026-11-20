/**
 * The shape of the single sign-on account statements, pinned without a pool so
 * every run checks the guards on the email match and those that keep a link
 * and a create race-safe. What Postgres does with them is in
 * `sso-users.test.ts`, an opt-in probe.
 *
 * Each statement is compared whole, so a guard dropped from the WHERE clause,
 * or a clause appended after it, fails here.
 */
import { describe, expect, it } from "bun:test";
import { buildInsert } from "../database";
import { buildFilterClauses, resolveMutationFilters } from "../models";
import {
  buildFindUnlinkedUsersByEmail,
  buildInsertSsoUser,
  buildLinkUserAuthSub,
} from "./users";

describe("nullary filter conditions", () => {
  it("bind no value, and the placeholders after them keep their numbers", () => {
    const entries = resolveMutationFilters("updateWhere", {
      auth_sub: { op: "IS NULL" },
      is_deleted: { op: "IS NOT TRUE" },
      user_id: "u1",
    });
    expect(buildFilterClauses(entries, 3)).toEqual({
      whereSql: "auth_sub IS NULL AND is_deleted IS NOT TRUE AND user_id = $3",
      values: ["u1"],
    });
  });

  it("are a predicate of their own, never a missing value", () => {
    expect(() => resolveMutationFilters("deleteWhere", { auth_sub: { op: "IS NULL" } })).not.toThrow();
  });
});

describe("buildInsert onConflictDoNothing", () => {
  it("skips a conflicting row, ahead of RETURNING", () => {
    expect(buildInsert("t", { a: 1 }, ["*"], { stampUpdated: false, onConflictDoNothing: true })).toEqual({
      sql: "INSERT INTO t (a) VALUES ($1) ON CONFLICT DO NOTHING RETURNING *",
      values: [1],
    });
  });

  it("adds nothing without the option", () => {
    expect(buildInsert("t", { a: 1 }, ["*"], { stampUpdated: false }).sql).toBe(
      "INSERT INTO t (a) VALUES ($1) RETURNING *"
    );
  });
});

describe("buildFindUnlinkedUsersByEmail", () => {
  it("matches the address ignoring case, on signed-up accounts that are not linked, deleted or excluded", () => {
    expect(buildFindUnlinkedUsersByEmail("Alice@Example.com", ["admin", "admin-ro"])).toEqual({
      sql: "SELECT * FROM users WHERE LOWER(email) = LOWER($1) AND auth_sub IS NULL AND password IS NOT NULL AND username <> ALL($2::text[]) AND (is_deleted IS NULL OR is_deleted = FALSE) LIMIT 2",
      values: ["Alice@Example.com", ["admin", "admin-ro"]],
    });
  });
});

describe("buildLinkUserAuthSub", () => {
  it("links only an account no subject holds yet, and never a deleted one", () => {
    expect(buildLinkUserAuthSub("u1", "usr_alice")).toEqual({
      sql: "UPDATE users SET auth_sub = $1, updated = CURRENT_TIMESTAMP WHERE user_id = $2 AND auth_sub IS NULL AND is_deleted IS NOT TRUE RETURNING *",
      values: ["usr_alice", "u1"],
    });
  });
});

describe("buildInsertSsoUser", () => {
  it("inserts nothing when the username or the subject is taken", () => {
    expect(
      buildInsertSsoUser({ username: "alice", passwordHash: "$2b$hash", email: "alice@example.com", sub: "usr_alice" })
    ).toEqual({
      sql: "INSERT INTO users (updated, username, password, email, auth_sub) VALUES (CURRENT_TIMESTAMP, $1, $2, $3, $4) ON CONFLICT DO NOTHING RETURNING *",
      values: ["alice", "$2b$hash", "alice@example.com", "usr_alice"],
    });
  });
});
