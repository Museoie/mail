import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { addr, missingId, startServer, waitFor, type TestServer } from "../harness";
import {
  BODY_ETAG,
  INTERNAL_ERROR,
  JSON_TYPE,
  decodeCursor,
  encodeCursor,
  failed,
  headerOf,
  ids,
  newMailbox,
  subjects,
  type Mailbox,
} from "./mails-helpers";

let server: TestServer;

beforeAll(async () => {
  server = await startServer();
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

const BAD_LIMIT = failed("Invalid `limit` parameter: expected 1-10000.");
const BAD_CURSOR = failed("Invalid `cursor` parameter.");
const CURSOR_NEEDS_LIMIT = failed("`cursor` needs `limit`.");

const page = async (box: Mailbox, query: string, account = box.address()) => {
  const { status, body } = await box.client.get(`/api/mails/headers/${account}?${query}`);
  expect(status).toBe(200);
  expect(body.status).toBe("success");
  return body.body as { next_cursor: string | null; headers: Record<string, any>[] };
};

/** Walks every page of `limit` rows and returns the pages' subjects. */
const walk = async (box: Mailbox, limit: number, extra = "", account = box.address()) => {
  const pages: string[][] = [];
  let cursor: string | null = null;
  do {
    const result = await page(box, `limit=${limit}${cursor ? `&cursor=${cursor}` : ""}${extra}`, account);
    pages.push(subjects(result.headers as { subject: string }[]));
    cursor = result.next_cursor;
  } while (cursor && pages.length < 100);
  return pages;
};

describe("GET /api/mails/headers/:account?limit= — the response", () => {
  test("an empty page has a null cursor and an exact body", async () => {
    const box = await newMailbox(server);
    const { status, text, headers } = await box.client.get(`/api/mails/headers/${box.address()}?limit=5`);
    expect(status).toBe(200);
    expect(text).toBe('{"status":"success","body":{"next_cursor":null,"headers":[]}}');
    expect(headers.get("content-type")).toBe(JSON_TYPE);
    expect(headers.get("etag")).toBe('W/"3d-xaLYkRxFwldd65XSt+xcFxXkCKE"');
  });

  test("a page is an object: next_cursor first, then the rows of the full list", async () => {
    const box = await newMailbox(server);
    const newer = await box.mail({ subject: "newer", date: "2026-01-02T00:00:00Z" });
    const older = await box.mail({ subject: "older", date: "2026-01-01T00:00:00Z" });
    const full = await box.client.get(`/api/mails/headers/${box.address()}`);
    const paged = await box.client.get(`/api/mails/headers/${box.address()}?limit=10`);
    expect(paged.text).toBe(`{"status":"success","body":{"next_cursor":null,"headers":${JSON.stringify(full.body.body)}}}`);
    expect(paged.body.body.headers).toEqual([headerOf(newer), headerOf(older)]);
    expect(paged.headers.get("etag")).toMatch(BODY_ETAG);
    expect(paged.headers.get("etag")).not.toBe(full.headers.get("etag"));
  });

  test("the cursor is base64url JSON of the last row's stored date and id", async () => {
    const box = await newMailbox(server);
    const last = await box.mail({ subject: "first page", date: "2026-05-01 10:00:02.123457+00" });
    await box.mail({ subject: "second page", date: "2026-05-01 10:00:02.123456+00" });
    const first = await page(box, "limit=1");
    expect(subjects(first.headers as any)).toEqual(["first page"]);
    expect(first.next_cursor).toBe(encodeCursor({ date: "2026-05-01 10:00:02.123457+00", id: last.mail_id }));
    expect(first.next_cursor).toMatch(/^[A-Za-z0-9_-]+$/);
    expect(Object.keys(decodeCursor(first.next_cursor!))).toEqual(["date", "id"]);
    // The row itself shows the date cut to milliseconds; the cursor keeps the microseconds.
    expect(first.headers[0]!.date).toBe("2026-05-01T10:00:02.123Z");
    const second = await page(box, `limit=1&cursor=${first.next_cursor}`);
    expect(subjects(second.headers as any)).toEqual(["second page"]);
    expect(second.next_cursor).toBeNull();
  });

  test("the cursor date is the database's text form in UTC, without trailing zeros", async () => {
    const box = await newMailbox(server);
    const cases: [string, string][] = [
      ["2026-05-01 10:00:00+00", "2026-05-01 10:00:00+00"],
      ["2026-05-01 19:00:00.5+09", "2026-05-01 10:00:00.5+00"],
      ["2026-05-01 10:00:00.120000+00", "2026-05-01 10:00:00.12+00"],
      ["2026-05-01 10:00:00.000001+00", "2026-05-01 10:00:00.000001+00"],
    ];
    for (const [stored, expected] of cases) {
      const account = box.address(`c${cases.findIndex(([s]) => s === stored)}`);
      const mail = await box.mail({ to: [addr(account)], date: stored });
      await box.mail({ to: [addr(account)], date: "2020-01-01T00:00:00Z" });
      const result = await page(box, "limit=1", account);
      expect({ stored, cursor: decodeCursor(result.next_cursor!) }).toEqual({
        stored,
        cursor: { date: expected, id: mail.mail_id },
      });
    }
  });
});

describe("GET /api/mails/headers/:account?limit= — walking pages", () => {
  let box: Mailbox;
  const order = ["m7", "m6", "micro", "tie-high", "tie-mid", "tie-low", "m1", "m0"];
  beforeAll(async () => {
    box = await newMailbox(server);
    await box.mail({ subject: "m0", date: "2026-05-01 10:00:00+00" });
    await box.mail({ subject: "m1", date: "2026-05-01 10:00:01+00" });
    await box.mail({ subject: "tie-low", date: "2026-05-01 10:00:02.123456+00", mail_id: "00000000-0000-4000-8000-0000000000aa" });
    await box.mail({ subject: "tie-mid", date: "2026-05-01 10:00:02.123456+00", mail_id: "80000000-0000-4000-8000-0000000000aa" });
    await box.mail({ subject: "tie-high", date: "2026-05-01 10:00:02.123456+00", mail_id: "ffffffff-ffff-4fff-8fff-ffffffffffaa" });
    await box.mail({ subject: "micro", date: "2026-05-01 10:00:02.123457+00" });
    await box.mail({ subject: "m6", date: "2026-05-01 10:00:06+00", read: true });
    await box.mail({ subject: "m7", date: "2026-05-01 10:00:07+00" });
  });

  test("pages follow the full list's order: date, then id, descending", async () => {
    const full = await box.client.get(`/api/mails/headers/${box.address()}`);
    expect(subjects(full.body.body)).toEqual(order);
    expect((await walk(box, 3)).flat()).toEqual(order);
  });

  test("limit=1 yields one row per page and ends with a null cursor", async () => {
    const pages = await walk(box, 1);
    expect(pages).toEqual(order.map((subject) => [subject]));
  });

  test("a page boundary inside a run of equal dates loses and repeats nothing", async () => {
    expect(await walk(box, 4)).toEqual([order.slice(0, 4), order.slice(4, 8)]);
    expect(await walk(box, 5)).toEqual([order.slice(0, 5), order.slice(5, 8)]);
    expect(await walk(box, 3)).toEqual([order.slice(0, 3), order.slice(3, 6), order.slice(6, 8)]);
  });

  test("a page of exactly the remaining rows has a null cursor; one fewer has a cursor", async () => {
    expect((await page(box, "limit=8")).next_cursor).toBeNull();
    expect((await page(box, "limit=9")).next_cursor).toBeNull();
    expect((await page(box, "limit=10000")).next_cursor).toBeNull();
    const seven = await page(box, "limit=7");
    expect(seven.headers).toHaveLength(7);
    expect(decodeCursor(seven.next_cursor!).date).toBe("2026-05-01 10:00:01+00");
    const rest = await page(box, `limit=7&cursor=${seven.next_cursor}`);
    expect(subjects(rest.headers as any)).toEqual(["m0"]);
    expect(rest.next_cursor).toBeNull();
  });

  test("limit may change between pages", async () => {
    const first = await page(box, "limit=2");
    const second = await page(box, `limit=5&cursor=${first.next_cursor}`);
    expect(subjects(second.headers as any)).toEqual(order.slice(2, 7));
    const third = await page(box, `limit=10000&cursor=${second.next_cursor}`);
    expect(subjects(third.headers as any)).toEqual(order.slice(7));
    expect(third.next_cursor).toBeNull();
  });

  test("a mail arriving between two pages does not shift the rows still to come", async () => {
    const fresh = await newMailbox(server);
    for (let i = 0; i < 4; i++) await fresh.mail({ subject: `p${i}`, date: `2026-05-01 10:00:0${i}+00` });
    const first = await page(fresh, "limit=2");
    expect(subjects(first.headers as any)).toEqual(["p3", "p2"]);
    await fresh.mail({ subject: "arrived", date: "2026-06-01 00:00:00+00" });
    await fresh.mail({ subject: "backdated", date: "2026-05-01 10:00:00.5+00" });
    const second = await page(fresh, `limit=2&cursor=${first.next_cursor}`);
    expect(subjects(second.headers as any)).toEqual(["p1", "backdated"]);
    const third = await page(fresh, `limit=2&cursor=${second.next_cursor}`);
    expect(subjects(third.headers as any)).toEqual(["p0"]);
  });

  test("a row expunged or marked spam between pages just drops out", async () => {
    const fresh = await newMailbox(server);
    const mails = [];
    for (let i = 0; i < 5; i++) mails.push(await fresh.mail({ subject: `p${i}`, date: `2026-05-01 10:00:0${i}+00` }));
    const first = await page(fresh, "limit=2");
    await server.db.query("UPDATE mails SET expunged = TRUE WHERE mail_id = $1", [mails[2]!.mail_id]);
    await server.db.query("UPDATE mails SET is_spam = TRUE WHERE mail_id = $1", [mails[1]!.mail_id]);
    // The cursor row itself may be gone as well: the position is a value, not a row.
    await server.db.query("DELETE FROM mails WHERE mail_id = $1", [mails[3]!.mail_id]);
    const second = await page(fresh, `limit=2&cursor=${first.next_cursor}`);
    expect(subjects(second.headers as any)).toEqual(["p0"]);
    expect(second.next_cursor).toBeNull();
  });
});

describe("GET /api/mails/headers/:account?limit= — views", () => {
  test("pages the sent, saved and spam views with the view's own rows", async () => {
    const box = await newMailbox(server);
    const me = box.address();
    for (let i = 0; i < 3; i++) {
      const date = `2026-05-01 10:00:0${i}+00`;
      await box.mail({ subject: `received ${i}`, date });
      await box.mail({ subject: `sent ${i}`, from: [addr(me)], to: [addr("x@remote.example")], envelope_to: [], sent: true, date });
      await box.mail({ subject: `saved ${i}`, saved: true, date });
      await box.mail({ subject: `spam ${i}`, is_spam: true, date });
    }
    await box.mail({ subject: "draft", draft: true });
    await box.mail({ subject: "expunged", expunged: true });
    expect(await walk(box, 2, "&sent=1")).toEqual([["sent 2", "sent 1"], ["sent 0"]]);
    expect(await walk(box, 2, "&saved=1")).toEqual([["saved 2", "saved 1"], ["saved 0"]]);
    expect(await walk(box, 2, "&spam=1")).toEqual([["spam 2", "spam 1"], ["spam 0"]]);
    const received = (await walk(box, 4)).flat();
    expect(received).toHaveLength(6);
    expect(received.filter((s) => s.startsWith("spam") || s === "draft" || s === "expunged" || s.startsWith("sent"))).toEqual([]);
  });

  test("a cursor is only a position: it works in any view and for any account", async () => {
    const box = await newMailbox(server);
    const me = box.address();
    await box.mail({ subject: "received old", date: "2026-05-01 10:00:00+00" });
    await box.mail({ subject: "received new", date: "2026-05-01 10:00:09+00" });
    await box.mail({ subject: "spam old", is_spam: true, date: "2026-05-01 10:00:01+00" });
    await box.mail({ subject: "spam new", is_spam: true, date: "2026-05-01 10:00:08+00" });
    const cursor = encodeCursor({ date: "2026-05-01 10:00:05+00", id: missingId() });
    expect(subjects((await page(box, `limit=5&cursor=${cursor}`)).headers as any)).toEqual(["received old"]);
    expect(subjects((await page(box, `limit=5&spam=1&cursor=${cursor}`)).headers as any)).toEqual(["spam old"]);
    expect(await page(box, `limit=5&cursor=${cursor}`, box.address("empty"))).toEqual({ next_cursor: null, headers: [] });
    expect(me).toBe(box.address());
  });

  test("the account check applies to a paged request", async () => {
    const box = await newMailbox(server);
    const { body } = await box.client.get("/api/mails/headers/hello@mail.test?limit=5");
    expect(body).toEqual(failed("Invalid request. You may not look at other users' emails."));
  });
});

describe("GET /api/mails/headers/:account — limit and cursor validation", () => {
  let box: Mailbox;
  const good = encodeCursor({ date: "2026-05-01 10:00:05+00", id: "11111111-1111-4111-8111-111111111111" });
  beforeAll(async () => {
    box = await newMailbox(server);
    await box.mail({ subject: "only", date: "2026-05-01 10:00:00+00" });
  });
  const get = (query: string) => box.client.get(`/api/mails/headers/${box.address()}?${query}`);

  test("limit accepts 1 to 10000 written in digits", async () => {
    for (const limit of ["1", "2", "10000", "01", "0010000", "00000000000000000001"]) {
      const { body } = await get(`limit=${limit}`);
      expect({ limit, outcome: body.status, rows: body.body?.headers?.length }).toEqual({ limit, outcome: "success", rows: 1 });
    }
  });

  test("any other limit is refused", async () => {
    for (const limit of [
      "0", "10001", "-1", "+1", "1.5", "1.0", "1e3", "0x10", "abc", "", " 1", "1 ", "１", "९", "99999999999999999999",
      "NaN", "Infinity", "null", "true", "1,2",
    ]) {
      const { status, body } = await get(`limit=${encodeURIComponent(limit)}`);
      expect({ limit, status, body }).toEqual({ limit, status: 200, body: BAD_LIMIT });
    }
  });

  test("a bare or repeated limit is refused", async () => {
    for (const query of ["limit", "limit=1&limit=2", "limit=1&limit=1", "limit=&limit=5"]) {
      const { body } = await get(query);
      expect({ query, body }).toEqual({ query, body: BAD_LIMIT });
    }
  });

  test("the refusal is exact", async () => {
    const { status, text, headers } = await get("limit=0");
    expect(status).toBe(200);
    expect(text).toBe('{"status":"failed","message":"Invalid `limit` parameter: expected 1-10000."}');
    expect(headers.get("content-type")).toBe(JSON_TYPE);
  });

  test("a cursor without a limit is refused, whatever the cursor is", async () => {
    for (const query of ["cursor=abc", `cursor=${good}`, "cursor=", "cursor", "cursor=a&cursor=b"]) {
      const { status, body } = await get(query);
      expect({ query, status, body }).toEqual({ query, status: 200, body: CURSOR_NEEDS_LIMIT });
    }
  });

  test("a bad limit is reported before a bad cursor", async () => {
    expect((await get("limit=0&cursor=abc")).body).toEqual(BAD_LIMIT);
    expect((await get("limit=1&cursor=abc")).body).toEqual(BAD_CURSOR);
  });

  test("a cursor that is not base64url JSON of {date, id} is refused", async () => {
    const id = "11111111-1111-4111-8111-111111111111";
    const date = "2026-05-01 10:00:05+00";
    const cases: [string, string][] = [
      ["empty", ""],
      ["not base64", "!!!"],
      ["base64 of text", "abc"],
      ["base64 of invalid JSON", Buffer.from("{").toString("base64url")],
      ["JSON null", encodeCursor(null)],
      ["JSON array", encodeCursor([date, id])],
      ["JSON string", encodeCursor("x")],
      ["JSON number", encodeCursor(5)],
      ["JSON true", encodeCursor(true)],
      ["empty object", encodeCursor({})],
      ["no date", encodeCursor({ id })],
      ["no id", encodeCursor({ date })],
      ["date as a number", encodeCursor({ date: 1777629605, id })],
      ["date null", encodeCursor({ date: null, id })],
      ["ISO date", encodeCursor({ date: "2026-05-01T10:00:05.000Z", id })],
      ["date with T", encodeCursor({ date: "2026-05-01T10:00:05+00", id })],
      ["date without zone", encodeCursor({ date: "2026-05-01 10:00:05", id })],
      ["date with Z", encodeCursor({ date: "2026-05-01 10:00:05Z", id })],
      ["date only", encodeCursor({ date: "2026-05-01", id })],
      ["seven fraction digits", encodeCursor({ date: "2026-05-01 10:00:05.1234567+00", id })],
      ["fraction dot without digits", encodeCursor({ date: "2026-05-01 10:00:05.+00", id })],
      ["BC suffix", encodeCursor({ date: "2026-05-01 10:00:05+00 BC", id })],
      ["five-digit year", encodeCursor({ date: "12026-05-01 10:00:05+00", id })],
      ["leading space", encodeCursor({ date: " 2026-05-01 10:00:05+00", id })],
      ["trailing newline", encodeCursor({ date: "2026-05-01 10:00:05+00\n", id })],
      ["single-digit zone", encodeCursor({ date: "2026-05-01 10:00:05+0", id })],
      ["id not a UUID", encodeCursor({ date, id: "abc" })],
      ["id as a number", encodeCursor({ date, id: 5 })],
      ["id null", encodeCursor({ date, id: null })],
      ["id without dashes", encodeCursor({ date, id: id.replace(/-/g, "") })],
      ["id in braces", encodeCursor({ date, id: `{${id}}` })],
      ["SQL in the date", encodeCursor({ date: "2026-05-01 10:00:05+00'; DROP TABLE mails; --", id })],
    ];
    for (const [label, cursor] of cases) {
      const { status, body } = await get(`limit=5&cursor=${encodeURIComponent(cursor)}`);
      expect({ label, status, body }).toEqual({ label, status: 200, body: BAD_CURSOR });
    }
    const repeated = await get(`limit=5&cursor=${good}&cursor=${good}`);
    expect(repeated.body).toEqual(BAD_CURSOR);
    expect((await server.db.query("SELECT count(*) AS n FROM mails WHERE user_id = $1", [box.id]))[0]!.n).toBe("1");
  });

  test("a cursor is accepted with any zone offset, an upper-case id, extra keys or standard base64", async () => {
    const id = "11111111-1111-4111-8111-111111111111";
    const cases: [string, string, number][] = [
      ["after the row", encodeCursor({ date: "2026-05-01 10:00:05+00", id }), 1],
      ["before the row", encodeCursor({ date: "2026-05-01 09:59:59+00", id }), 0],
      ["microseconds", encodeCursor({ date: "2026-05-01 10:00:00.000001+00", id }), 1],
      ["one fraction digit", encodeCursor({ date: "2026-05-01 10:00:00.1+00", id }), 1],
      ["zone +09 after the row", encodeCursor({ date: "2026-05-01 19:00:01+09", id }), 1],
      ["zone +09 before the row", encodeCursor({ date: "2026-05-01 18:59:59+09", id }), 0],
      ["zone -08", encodeCursor({ date: "2026-05-01 02:00:01-08", id }), 1],
      ["zone +05:30", encodeCursor({ date: "2026-05-01 15:30:01+05:30", id }), 1],
      ["zone +05:30:15", encodeCursor({ date: "2026-05-01 15:30:16+05:30:15", id }), 1],
      ["upper-case id", encodeCursor({ date: "2026-05-01 10:00:05+00", id: id.toUpperCase() }), 1],
      ["extra keys", encodeCursor({ date: "2026-05-01 10:00:05+00", id, limit: 99, sent: true }), 1],
      ["keys in the other order", encodeCursor({ id, date: "2026-05-01 10:00:05+00" }), 1],
      ["standard base64 with padding", Buffer.from(JSON.stringify({ date: "2026-05-01 10:00:05+00", id })).toString("base64"), 1],
      ["far future", encodeCursor({ date: "9999-12-31 23:59:59+00", id }), 1],
      ["far past", encodeCursor({ date: "0001-01-01 00:00:00+00", id }), 0],
    ];
    for (const [label, cursor, rows] of cases) {
      const { status, body } = await get(`limit=5&cursor=${encodeURIComponent(cursor)}`);
      expect({ label, status, outcome: body.status, rows: body.body?.headers?.length }).toEqual({
        label,
        status: 200,
        outcome: "success",
        rows,
      });
    }
  });

  test("the cursor compares the id among rows of the cursor's exact date", async () => {
    const row = (await server.db.query("SELECT mail_id FROM mails WHERE user_id = $1", [box.id]))[0]!.mail_id as string;
    const at = (id: string) => encodeCursor({ date: "2026-05-01 10:00:00+00", id });
    expect((await get(`limit=5&cursor=${at("ffffffff-ffff-4fff-8fff-ffffffffffff")}`)).body.body.headers).toHaveLength(1);
    expect((await get(`limit=5&cursor=${at("00000000-0000-4000-8000-000000000000")}`)).body.body.headers).toHaveLength(0);
    // Its own position excludes the row itself: strictly after.
    expect((await get(`limit=5&cursor=${at(row)}`)).body.body.headers).toHaveLength(0);
  });

  // NOTE: the cursor's date is checked for shape only. A date that has the
  // shape but is no real timestamp (month 13, hour 25, zone +99) reaches the
  // query, the database rejects it, and the client gets a 500 for a value it
  // sent — where every other malformed cursor is a 200 refusal.
  test("a cursor whose date has the right shape but is no real time is a 500", async () => {
    const id = "11111111-1111-4111-8111-111111111111";
    for (const date of [
      "2026-13-01 00:00:00+00",
      "2026-02-31 00:00:00+00",
      "2026-01-01 25:00:00+00",
      "2026-01-01 00:61:00+00",
      "0000-01-01 00:00:00+00",
      "2026-01-01 00:00:00+99",
    ]) {
      const { status, body, headers } = await get(`limit=5&cursor=${encodeCursor({ date, id })}`);
      expect({ date, status, body, type: headers.get("content-type") }).toEqual({
        date,
        status: 500,
        body: INTERNAL_ERROR,
        type: JSON_TYPE,
      });
    }
    // The failure is alarmed under the route's name; the server stays up.
    const alarm = await waitFor(() => server.alarms.alarms.find((a) => a.title.includes("Route Error")), {
      what: "the route error alarm",
    });
    expect(alarm.title).toBe("🚨 Mail Server Error: Route Error: GET /headers/:account");
    expect((await get("limit=5")).body.body.headers).toHaveLength(1);
  });
});

describe("GET /api/mails/headers/:account?limit= — the largest page", () => {
  test("limit=10000 returns 10000 rows and a cursor to the rest", async () => {
    const box = await newMailbox(server);
    const me = box.address();
    await server.db.query(
      `INSERT INTO mails (user_id, message_id, subject, date, to_address, to_text, envelope_to, text)
       SELECT $1, '<bulk-' || g || '@x>', 'bulk ' || g, timestamptz '2026-01-01T00:00:00Z' + g * interval '1 second',
              $2::jsonb, $3, $2::jsonb, 'body ' || g
       FROM generate_series(1, 10001) g`,
      [box.id, [{ address: me }], me]
    );
    const first = await page(box, "limit=10000");
    expect(first.headers).toHaveLength(10000);
    expect(first.headers[0]!.subject).toBe("bulk 10001");
    expect(first.headers[9999]!.subject).toBe("bulk 2");
    expect(decodeCursor(first.next_cursor!)).toEqual({ date: "2026-01-01 00:00:02+00", id: first.headers[9999]!.id });
    const second = await page(box, `limit=10000&cursor=${first.next_cursor}`);
    expect(subjects(second.headers as any)).toEqual(["bulk 1"]);
    expect(second.next_cursor).toBeNull();
    // Every id exactly once across the two pages.
    expect(new Set([...ids(first.headers as any), ...ids(second.headers as any)]).size).toBe(10001);
    // A middle page of 500 picks up where a cursor says.
    const mid = await page(box, `limit=500&cursor=${encodeCursor({ date: "2026-01-01 01:00:01+00", id: "00000000-0000-4000-8000-000000000000" })}`);
    expect(mid.headers).toHaveLength(500);
    expect(mid.headers[0]!.subject).toBe("bulk 3600");
    expect(mid.headers[499]!.subject).toBe("bulk 3101");
  }, 120_000);
});
