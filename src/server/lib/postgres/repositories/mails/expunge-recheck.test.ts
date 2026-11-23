/**
 * EXPUNGE looks its `\Deleted` mails up, then marks them. Another client can
 * clear `\Deleted` in between, so the UPDATE has to ask again rather than
 * trust the lookup's list of ids.
 */
import { describe, it, expect, mock, beforeAll, afterAll } from "bun:test";
import { restoreLeaves } from "test-helpers";

const USER = "11111111-1111-4111-8111-111111111111";

type Call = { sql: string; values: unknown[] };
const calls: Call[] = [];

const mockQuery = mock(async (sql: string, values: unknown[] = []) => {
  calls.push({ sql, values });
  if (/^\s*UPDATE mails\b/i.test(sql)) {
    // Stands in for a mail the other client restored: only one row matches.
    return { rows: [{ mail_id: "still-deleted" }], rowCount: 1 };
  }
  if (/next_uid/i.test(sql)) return { rows: [{ next_uid: "7" }], rowCount: 1 };
  return {
    rows: [
      { mail_id: "still-deleted", uid: 11 },
      { mail_id: "restored-meanwhile", uid: 12 },
    ],
    rowCount: 2,
  };
});

class FakePool {
  query = mockQuery;
  end = async () => {};
  connect = async () => ({ query: mockQuery, release: () => {} });
  on() {}
}

const pgMock = () => ({
  Pool: FakePool,
  types: { setTypeParser: () => {}, builtins: {}, getTypeParser: () => null },
  default: { Pool: FakePool, types: { setTypeParser: () => {} } },
});

mock.module("server/lib/postgres/pool", pgMock);

const { expungeDeletedMails } = await import("./imap");
const { resetPool } = await import("../../client");

beforeAll(() => {
  mock.module("server/lib/postgres/pool", pgMock);
  resetPool();
});

afterAll(() => {
  restoreLeaves();
  resetPool();
});

/** The value bound to `column = $n` in the WHERE clause, or a marker when it is not bound. */
const boundTo = ({ sql, values }: Call, column: string): unknown => {
  const where = sql.slice(sql.search(/\bWHERE\b/i));
  const match = new RegExp(`\\b${column}\\s*=\\s*\\$(\\d+)`).exec(where);
  return match ? values[Number(match[1]) - 1] : "<not in the WHERE clause>";
};

describe("expungeDeletedMails asks again at write time", () => {
  for (const mailbox of ["INBOX", "Drafts", "Junk", "Work"]) {
    it(`${mailbox}: the UPDATE re-checks owner, \\Deleted and not-yet-expunged`, async () => {
      calls.length = 0;
      const uids = await expungeDeletedMails(USER, mailbox, false);

      const update = calls.find((call) => /^\s*UPDATE mails\b/i.test(call.sql));
      expect(update).toBeDefined();
      expect({
        user_id: boundTo(update!, "user_id"),
        deleted: boundTo(update!, "deleted"),
        expunged: boundTo(update!, "expunged"),
      }).toEqual({ user_id: USER, deleted: true, expunged: false });
      // Only the row the UPDATE returned is reported expunged.
      expect(uids).toEqual([11]);
    });
  }
});
