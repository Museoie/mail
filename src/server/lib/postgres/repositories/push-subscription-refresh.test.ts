/**
 * `GET /api/push/refresh/:id` keeps a browser's subscription alive past the
 * seven-day sweep. It used to run an update that set no column, which the
 * query builder turns into no statement at all: the answer was always "not
 * found" and the subscription was swept anyway.
 *
 * The stub is installed through the pool Proxy's set trap rather than
 * `mock.module("server/lib/postgres/pool", ...)` — same seam as
 * `session-destroy-propagation.test.ts`.
 */
import { describe, it, expect, beforeEach, afterAll, mock, spyOn } from "bun:test";
import { logger } from "../../logger";
import { pool, resetPool } from "../client";
import {
  buildRefreshSubscription,
  cleanSubscriptions,
  refreshSubscription,
} from "./push_subscriptions";

let mode: "found" | "none" | "throw" = "found";
const issued: { sql: string; values: unknown[] }[] = [];

const mockQuery = mock(async (sql: string, values: unknown[] = []) => {
  issued.push({ sql, values });
  if (mode === "throw") throw new Error('invalid input syntax for type uuid: "not-a-uuid"');
  const rows = mode === "found" ? [{ push_subscription_id: "sub-1" }] : [];
  return { rows, rowCount: rows.length };
});

const installQueryStub = () => {
  (pool as unknown as { query: typeof mockQuery }).query = mockQuery;
};

resetPool();
installQueryStub();
const loggedError = spyOn(logger, "error").mockImplementation(() => {});

afterAll(() => {
  resetPool();
  loggedError.mockRestore();
});

beforeEach(() => {
  mode = "found";
  issued.length = 0;
  installQueryStub();
});

describe("refreshSubscription", () => {
  it("moves `updated` on the caller's own row, by the database clock", () => {
    expect(buildRefreshSubscription("sub-1", "u1")).toEqual({
      sql: "UPDATE push_subscriptions SET updated = CURRENT_TIMESTAMP WHERE push_subscription_id = $1 AND user_id = $2 RETURNING push_subscription_id",
      values: ["sub-1", "u1"],
    });
  });

  it("issues that statement, and answers true when the row was there", async () => {
    expect(await refreshSubscription("sub-1", "u1")).toBe(true);
    expect(issued).toEqual([buildRefreshSubscription("sub-1", "u1")]);
  });

  it("answers false for an unknown id or someone else's subscription", async () => {
    mode = "none";
    expect(await refreshSubscription("sub-1", "u2")).toBe(false);
    expect(issued).toHaveLength(1);
  });

  it("answers false when the statement fails, as for an id that is not a UUID", async () => {
    mode = "throw";
    expect(await refreshSubscription("not-a-uuid", "u1")).toBe(false);
    expect(loggedError).toHaveBeenCalled();
  });

  it("moves the column the sweep reads", async () => {
    await cleanSubscriptions();
    expect(issued[0]!.sql).toMatch(/WHERE updated < \$1/);
    expect(buildRefreshSubscription("sub-1", "u1").sql).toContain("SET updated = CURRENT_TIMESTAMP");
  });
});
