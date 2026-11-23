import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { addr, startServer, waitFor, type TestServer } from "../harness";
import {
  BODY_ETAG,
  INTERNAL_ERROR,
  ISO_MS,
  JSON_TYPE,
  adminMailbox,
  decodeCursor,
  encodeCursor,
  failed,
  headerOf,
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

const BAD_SINCE = failed("Invalid `since` parameter: expected an ISO timestamp.");
const BAD_LIMIT = failed("Invalid `limit` parameter: expected 1-10000.");
const BAD_CURSOR = failed("Invalid `cursor` parameter.");
const OLD = "2026-01-01T00:00:00Z";
const SINCE = "2026-06-01T00:00:00Z";
const RECENT = "2026-09-01T00:00:00Z";

const unified = async (box: Mailbox, query = "") => {
  const { status, body } = await box.client.get(`/api/mails/unified${query}`);
  expect(status).toBe(200);
  expect(body.status).toBe("success");
  return body.body;
};

describe("GET /api/mails/unified — the full list", () => {
  test("is an empty list for a user with no mail, with an exact body", async () => {
    const box = await newMailbox(server);
    const { status, text, headers } = await box.client.get("/api/mails/unified");
    expect(status).toBe(200);
    expect(text).toBe('{"status":"success","body":[]}');
    expect(headers.get("content-type")).toBe(JSON_TYPE);
    expect(headers.get("etag")).toBe('W/"1e-ZirBHndQgB0ZQPCHGvcJtDrJf6U"');
  });

  test("one mail, byte for byte, identical to the per-account list's row", async () => {
    const box = await newMailbox(server);
    const me = box.address();
    const mail = await box.mail({ subject: "Hi", text: "Body", date: "2026-01-02T03:04:05.678Z", message_id: "<u@x>" });
    const { text, headers } = await box.client.get("/api/mails/unified");
    expect(text).toBe(
      `{"status":"success","body":[{"id":"${mail.mail_id}","subject":"Hi","date":"2026-01-02T03:04:05.678Z",` +
        `"from":{"value":[{"name":"Remote Sender","address":"sender@remote.example"}],"text":"Remote Sender <sender@remote.example>"},` +
        `"to":{"value":[{"address":"${me}"}],"text":"${me}"},"envelopeTo":{"value":[{"address":"${me}"}],"text":""},` +
        `"read":false,"saved":false,"sent":false,"insight":null,"snippet":"Body","messageId":"<u@x>"}]}`
    );
    expect(headers.get("etag")).toMatch(BODY_ETAG);
    const perAccount = await box.client.get(`/api/mails/headers/${me}`);
    expect(perAccount.text).toBe(text);
    expect(perAccount.headers.get("etag")).toBe(headers.get("etag"));
  });

  test("lists received mail of every account of the user", async () => {
    const box = await newMailbox(server);
    const a = await box.mail({ subject: "to a", to: [addr(box.address("a"))], date: "2026-01-04T00:00:00Z" });
    const b = await box.mail({ subject: "cc b", to: [addr("x@remote.example")], cc: [addr(box.address("b"))], envelope_to: [], date: "2026-01-03T00:00:00Z" });
    const c = await box.mail({ subject: "bcc c", to: [addr("x@remote.example")], bcc: [addr(box.address("c"))], envelope_to: [], date: "2026-01-02T00:00:00Z" });
    const d = await box.mail({ subject: "envelope d", to: [addr("list@remote.example")], envelope_to: [{ address: box.address("d") }], date: "2026-01-01T00:00:00Z" });
    expect(await unified(box)).toEqual([headerOf(a), headerOf(b), headerOf(c), headerOf(d)]);
  });

  test("orders unread mail first, then newest first within each read state", async () => {
    const box = await newMailbox(server);
    await box.mail({ subject: "read newest", read: true, date: "2026-01-06T00:00:00Z" });
    await box.mail({ subject: "unread old", date: "2026-01-01T00:00:00Z" });
    await box.mail({ subject: "read old", read: true, date: "2026-01-02T00:00:00Z" });
    await box.mail({ subject: "unread new", date: "2026-01-05T00:00:00Z" });
    await box.mail({ subject: "unread mid", date: "2026-01-03T00:00:00.000001Z" });
    await box.mail({ subject: "read mid", read: true, date: "2026-01-03T00:00:00Z" });
    expect(subjects(await unified(box))).toEqual([
      "unread new",
      "unread mid",
      "unread old",
      "read newest",
      "read mid",
      "read old",
    ]);
  });

  test("leaves out spam, drafts, expunged mail and mail naming none of the user's addresses", async () => {
    const box = await newMailbox(server);
    const me = box.address();
    await box.mail({ subject: "kept" });
    await box.mail({ subject: "spam", is_spam: true });
    await box.mail({ subject: "draft", draft: true });
    await box.mail({ subject: "expunged", expunged: true });
    await box.mail({ subject: "sent to someone else", from: [addr(me)], to: [addr("x@remote.example")], envelope_to: [], sent: true });
    await box.mail({ subject: "no own recipient", to: [addr("x@remote.example")], envelope_to: [{ address: "y@remote.example" }] });
    await box.mail({ subject: "sub-subdomain", to: [addr(`x@sub.${box.domain}`)] });
    await box.mail({ subject: "no recipients", to_address: null, to_text: null, envelope_to: null });
    expect(subjects(await unified(box))).toEqual(["kept"]);
  });

  test("includes mail the user sent to one of their own addresses, and deleted or answered mail", async () => {
    const box = await newMailbox(server);
    const me = box.address();
    await box.mail({ subject: "to self", from: [addr(me)], to: [addr(me)], sent: true, read: true, date: "2026-01-01T00:00:00Z" });
    await box.mail({ subject: "flagged", deleted: true, answered: true, read: true, date: "2026-01-02T00:00:00Z" });
    expect(subjects(await unified(box))).toEqual(["flagged", "to self"]);
  });

  test("an address is in the view only in the spelling some listed mail carries", async () => {
    const box = await newMailbox(server);
    const upper = `Box@${box.domain.toUpperCase()}`;
    await box.mail({ subject: "lower", date: "2026-01-02T00:00:00Z" });
    await box.mail({ subject: "upper", to: [addr(upper)], date: "2026-01-01T00:00:00Z" });
    expect(subjects(await unified(box))).toEqual(["lower", "upper"]);
  });

  test("ignores the per-account view flags", async () => {
    const box = await newMailbox(server);
    const me = box.address();
    await box.mail({ subject: "received", date: "2026-01-02T00:00:00Z" });
    await box.mail({ subject: "saved", saved: true, date: "2026-01-01T00:00:00Z" });
    await box.mail({ subject: "spam", is_spam: true });
    await box.mail({ subject: "sent", from: [addr(me)], to: [addr("x@remote.example")], envelope_to: [], sent: true });
    for (const query of ["?sent=1", "?saved=1", "?spam=1", "?sent=1&saved=1&spam=1", "?new=1"]) {
      expect({ query, subjects: subjects(await unified(box, query)) }).toEqual({ query, subjects: ["received", "saved"] });
    }
  });

  test("never lists another user's mail", async () => {
    const first = await newMailbox(server);
    const second = await newMailbox(server);
    await first.mail({ subject: "mine" });
    await second.mail({ subject: "theirs, naming me", to: [addr(first.address()), addr(second.address())] });
    expect(subjects(await unified(first))).toEqual(["mine"]);
    expect(subjects(await unified(second))).toEqual(["theirs, naming me"]);
  });

  test("holds at most 10000 rows: unread first, so read mail is what falls off", async () => {
    const box = await newMailbox(server);
    const me = box.address();
    await server.db.query(
      `INSERT INTO mails (user_id, message_id, subject, date, to_address, to_text, envelope_to, text, read)
       SELECT $1, '<bulk-' || g || '@x>', 'bulk ' || g, timestamptz '2026-01-01T00:00:00Z' + g * interval '1 second',
              $2::jsonb, $3, $2::jsonb, 'body ' || g, g > 9000
       FROM generate_series(1, 10002) g`,
      [box.id, [{ address: me }], me]
    );
    const rows = await unified(box);
    expect(rows).toHaveLength(10000);
    expect(rows[0].subject).toBe("bulk 9000");
    expect(rows[8999].subject).toBe("bulk 1");
    expect(rows[9000].subject).toBe("bulk 10002");
    expect(rows[9999].subject).toBe("bulk 9003");
    expect(rows[9000].read).toBe(true);
  }, 120_000);
});

describe("GET /api/mails/unified for admin", () => {
  test("covers @mail.test addresses, not @admin.mail.test ones", async () => {
    const admin = await adminMailbox(server);
    const direct = await admin.mail({ subject: "bare domain", to: [addr("info@mail.test")], date: "2026-01-02T00:00:00Z" });
    await admin.mail({ subject: "admin subdomain", to: [addr("info@admin.mail.test")], date: "2026-01-01T00:00:00Z" });
    await admin.mail({ subject: "a user's address", to: [addr("x@someone.mail.test")] });
    expect(await unified(admin)).toEqual([headerOf(direct)]);
  });
});

describe("GET /api/mails/unified?limit= — pages", () => {
  let box: Mailbox;
  const order = ["m7", "m6", "micro", "tie-high", "tie-low", "m1", "m0"];
  beforeAll(async () => {
    box = await newMailbox(server);
    await box.mail({ subject: "m0", to: [addr(box.address("a"))], date: "2026-05-01 10:00:00+00", read: true });
    await box.mail({ subject: "m1", to: [addr(box.address("b"))], date: "2026-05-01 10:00:01+00" });
    await box.mail({ subject: "tie-low", date: "2026-05-01 10:00:02.123456+00", mail_id: "00000000-0000-4000-8000-0000000000bb" });
    await box.mail({ subject: "tie-high", date: "2026-05-01 10:00:02.123456+00", mail_id: "ffffffff-ffff-4fff-8fff-ffffffffffbb", read: true });
    await box.mail({ subject: "micro", date: "2026-05-01 10:00:02.123457+00" });
    await box.mail({ subject: "m6", to: [addr(box.address("c"))], date: "2026-05-01 10:00:06+00", read: true });
    await box.mail({ subject: "m7", date: "2026-05-01 10:00:07+00" });
    await box.mail({ subject: "spam", is_spam: true, date: "2026-05-01 10:00:08+00" });
    await box.mail({ subject: "draft", draft: true, date: "2026-05-01 10:00:08+00" });
    await box.mail({ subject: "expunged", expunged: true, date: "2026-05-01 10:00:08+00" });
  });

  test("an empty page has a null cursor and an exact body", async () => {
    const empty = await newMailbox(server);
    const { text, headers } = await empty.client.get("/api/mails/unified?limit=5");
    expect(text).toBe('{"status":"success","body":{"next_cursor":null,"headers":[]}}');
    expect(headers.get("etag")).toBe('W/"3d-xaLYkRxFwldd65XSt+xcFxXkCKE"');
  });

  test("a page is newest first whatever the read state — not the full list's unread-first order", async () => {
    const paged = await unified(box, "?limit=100");
    expect(Object.keys(paged)).toEqual(["next_cursor", "headers"]);
    expect(paged.next_cursor).toBeNull();
    expect(subjects(paged.headers)).toEqual(order);
    expect(subjects(await unified(box))).toEqual(["m7", "micro", "tie-low", "m1", "m6", "tie-high", "m0"]);
  });

  test("walking the pages yields every row once, with ids breaking date ties", async () => {
    for (const limit of [1, 2, 3, 4, 7]) {
      const seen: string[] = [];
      let cursor: string | null = null;
      let pages = 0;
      do {
        const result: { next_cursor: string | null; headers: { subject: string }[] } = await unified(
          box,
          `?limit=${limit}${cursor ? `&cursor=${cursor}` : ""}`
        );
        expect(result.headers.length).toBeLessThanOrEqual(limit);
        seen.push(...subjects(result.headers));
        cursor = result.next_cursor;
        pages++;
      } while (cursor && pages < 50);
      expect({ limit, seen, pages }).toEqual({ limit, seen: order, pages: Math.ceil(order.length / limit) });
    }
  });

  test("the cursor is the last row's stored date and id", async () => {
    const first = await unified(box, "?limit=3");
    expect(subjects(first.headers)).toEqual(["m7", "m6", "micro"]);
    expect(decodeCursor(first.next_cursor)).toEqual({ date: "2026-05-01 10:00:02.123457+00", id: first.headers[2].id });
    expect(first.next_cursor).toBe(encodeCursor({ date: "2026-05-01 10:00:02.123457+00", id: first.headers[2].id }));
  });

  test("rows have the full list's shape", async () => {
    const full = await unified(box);
    const paged = await unified(box, "?limit=100");
    const byId = new Map(full.map((row: { id: string }) => [row.id, row]));
    for (const row of paged.headers) expect(row).toEqual(byId.get(row.id));
  });

  test("a cursor from the per-account list works here, and the reverse", async () => {
    const perAccount = (await box.client.get(`/api/mails/headers/${box.address()}?limit=2`)).body.body;
    expect(subjects(perAccount.headers)).toEqual(["m7", "micro"]);
    const next = await unified(box, `?limit=2&cursor=${perAccount.next_cursor}`);
    expect(subjects(next.headers)).toEqual(["tie-high", "tie-low"]);
    const back = (await box.client.get(`/api/mails/headers/${box.address()}?limit=5&cursor=${next.next_cursor}`)).body.body;
    expect(back).toEqual({ next_cursor: null, headers: [] });
  });

  test("limit=10000 is accepted and limit and cursor are validated as on the per-account list", async () => {
    expect((await unified(box, "?limit=10000")).headers).toHaveLength(7);
    for (const query of ["?limit=0", "?limit=10001", "?limit=abc", "?limit=", "?limit=1&limit=2", "?limit=-1", "?limit=1.5"]) {
      const { status, body } = await box.client.get(`/api/mails/unified${query}`);
      expect({ query, status, body }).toEqual({ query, status: 200, body: BAD_LIMIT });
    }
    for (const query of ["?cursor=abc", "?cursor="]) {
      const { body } = await box.client.get(`/api/mails/unified${query}`);
      expect({ query, body }).toEqual({ query, body: failed("`cursor` needs `limit`.") });
    }
    for (const cursor of ["abc", "", encodeCursor({ date: "2026-05-01T10:00:00Z", id: "x" }), encodeCursor(null)]) {
      const { body } = await box.client.get(`/api/mails/unified?limit=5&cursor=${cursor}`);
      expect({ cursor, body }).toEqual({ cursor, body: BAD_CURSOR });
    }
  });

  // NOTE: as on the per-account list, a cursor date of the right shape that
  // is no real timestamp reaches the database and comes back as a 500.
  test("a cursor whose date is no real time is a 500", async () => {
    const cursor = encodeCursor({ date: "2026-13-01 00:00:00+00", id: "11111111-1111-4111-8111-111111111111" });
    const { status, body } = await box.client.get(`/api/mails/unified?limit=5&cursor=${cursor}`);
    expect(status).toBe(500);
    expect(body).toEqual(INTERNAL_ERROR);
    const alarm = await waitFor(() => server.alarms.alarms.find((a) => a.title.includes("GET /unified")), {
      what: "the route error alarm",
    });
    expect(alarm.title).toBe("🚨 Mail Server Error: Route Error: GET /unified");
  });

  test("the largest page: 10000 rows and a cursor to the rest", async () => {
    const big = await newMailbox(server);
    const me = big.address();
    await server.db.query(
      `INSERT INTO mails (user_id, message_id, subject, date, to_address, to_text, envelope_to, text, read)
       SELECT $1, '<bulk-' || g || '@x>', 'bulk ' || g, timestamptz '2026-01-01T00:00:00Z' + g * interval '1 second',
              $2::jsonb, $3, $2::jsonb, 'body ' || g, g % 2 = 0
       FROM generate_series(1, 10001) g`,
      [big.id, [{ address: me }], me]
    );
    const first = await unified(big, "?limit=10000");
    expect(first.headers).toHaveLength(10000);
    expect(first.headers[0].subject).toBe("bulk 10001");
    expect(first.headers[9999].subject).toBe("bulk 2");
    const second = await unified(big, `?limit=10000&cursor=${first.next_cursor}`);
    expect(subjects(second.headers)).toEqual(["bulk 1"]);
    expect(second.next_cursor).toBeNull();
  }, 120_000);
});

describe("GET /api/mails/unified?since= — the delta", () => {
  test("an empty delta: key order and headers", async () => {
    const box = await newMailbox(server);
    const { status, text, headers, body } = await box.client.get(`/api/mails/unified?since=${SINCE}`);
    expect(status).toBe(200);
    expect(text).toBe(`{"status":"success","body":{"as_of":"${body.body.as_of}","headers":[],"expunged_ids":[]}}`);
    expect(body.body.as_of).toMatch(ISO_MS);
    expect(headers.get("content-length")).toBe("95");
  });

  test("as_of is the database clock two seconds ago", async () => {
    const box = await newMailbox(server);
    const before = Date.now();
    const { as_of } = await unified(box, `?since=${SINCE}`);
    const after = Date.now();
    expect(Date.parse(as_of)).toBeGreaterThanOrEqual(before - 2250);
    expect(Date.parse(as_of)).toBeLessThanOrEqual(after - 1750);
  });

  test("returns rows changed after since across accounts, unread first, and tombstones for expunged or spam rows", async () => {
    const box = await newMailbox(server);
    const a = box.address("a");
    const b = box.address("b");
    await box.mail({ subject: "old", to: [addr(a)], updated: OLD });
    await box.mail({ subject: "read a", to: [addr(a)], read: true, date: "2026-01-05T00:00:00Z", updated: RECENT });
    await box.mail({ subject: "unread b old", to: [addr(b)], date: "2026-01-01T00:00:00Z", updated: RECENT });
    await box.mail({ subject: "unread a new", to: [addr(a)], date: "2026-01-03T00:00:00Z", updated: RECENT });
    await box.mail({ subject: "at since", to: [addr(a)], updated: "2026-06-01 00:00:00+00" });
    await box.mail({ subject: "draft", draft: true, to: [addr(a)], updated: RECENT });
    const expunged = await box.mail({ subject: "expunged", to: [addr(b)], expunged: true, updated: RECENT });
    const spam = await box.mail({ subject: "spam", to: [addr(a)], is_spam: true, updated: RECENT });
    await box.mail({ subject: "expunged long ago", to: [addr(a)], expunged: true, updated: OLD });
    await box.mail({ subject: "sent elsewhere", from: [addr(a)], to: [addr("x@remote.example")], envelope_to: [], sent: true, updated: RECENT });
    const result = await unified(box, `?since=${SINCE}`);
    expect(subjects(result.headers)).toEqual(["unread a new", "unread b old", "read a"]);
    expect([...result.expunged_ids].sort()).toEqual([expunged.mail_id, spam.mail_id].sort());
  });

  test("rows have the full list's shape", async () => {
    const box = await newMailbox(server);
    const mail = await box.mail({ cc: [addr("c@remote.example")], in_reply_to: "<p@x>", reference_ids: ["<p@x>"], updated: RECENT });
    expect((await unified(box, `?since=${SINCE}`)).headers).toEqual([headerOf(mail)]);
  });

  test("a tombstone is only reported for an address the view still covers", async () => {
    const box = await newMailbox(server);
    // The only mail of this account is expunged, so the account is no longer
    // one of the user's received accounts and the view no longer covers it.
    await box.mail({ subject: "lone expunged", to: [addr(box.address("gone"))], expunged: true, updated: RECENT });
    expect(await unified(box, `?since=${SINCE}`)).toMatchObject({ headers: [], expunged_ids: [] });
    // Once another mail names the account, the tombstone is reported.
    await box.mail({ subject: "alive", to: [addr(box.address("gone"))], updated: OLD });
    expect((await unified(box, `?since=${SINCE}`)).expunged_ids).toHaveLength(1);
  });

  test("never returns another user's rows or tombstones", async () => {
    const first = await newMailbox(server);
    const second = await newMailbox(server);
    await first.mail({ subject: "mine", updated: OLD });
    await second.mail({ to: [addr(first.address())], updated: RECENT });
    await second.mail({ to: [addr(first.address())], expunged: true, updated: RECENT });
    expect(await unified(first, `?since=${SINCE}`)).toMatchObject({ headers: [], expunged_ids: [] });
  });

  test("validates since as the per-account list does, before limit and cursor", async () => {
    const box = await newMailbox(server);
    for (const query of ["?since=", "?since", "?since=abc", "?since=now", `?since=${SINCE}&since=${SINCE}`, "?since=abc&limit=0"]) {
      const { status, body } = await box.client.get(`/api/mails/unified${query}`);
      expect({ query, status, body }).toEqual({ query, status: 200, body: BAD_SINCE });
    }
    for (const query of [`?since=${SINCE}&limit=0`, `?since=${SINCE}&cursor=x`, `?since=2026-06-01`, `?since=${encodeURIComponent("2026-06-01T09:00:00+09:00")}`]) {
      const { body } = await box.client.get(`/api/mails/unified${query}`);
      expect({ query, keys: Object.keys(body.body ?? {}) }).toEqual({ query, keys: ["as_of", "headers", "expunged_ids"] });
    }
  });

  // NOTE: same as the per-account delta — a since the database rejects is
  // answered as an empty success whose as_of is the caller's text.
  test("a since the database rejects is echoed back as as_of", async () => {
    const box = await newMailbox(server);
    await box.mail({ updated: RECENT });
    const { status, body } = await box.client.get("/api/mails/unified?since=2020");
    expect(status).toBe(200);
    expect(body).toEqual({ status: "success", body: { as_of: "2020", headers: [], expunged_ids: [] } });
  });
});
