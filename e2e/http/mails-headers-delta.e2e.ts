import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { addr, startServer, type TestServer } from "../harness";
import { BODY_ETAG, ISO_MS, JSON_TYPE, failed, headerOf, newMailbox, subjects, type Mailbox } from "./mails-helpers";

let server: TestServer;

beforeAll(async () => {
  server = await startServer();
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

const BAD_SINCE = failed("Invalid `since` parameter: expected an ISO timestamp.");
const OLD = "2026-01-01T00:00:00Z";
const SINCE = "2026-06-01T00:00:00Z";
const RECENT = "2026-09-01T00:00:00Z";

interface Delta {
  as_of: string;
  headers: Record<string, any>[];
  expunged_ids: string[];
}

const delta = async (box: Mailbox, since: string, extra = "", account = box.address()) => {
  const { status, body } = await box.client.get(
    `/api/mails/headers/${account}?since=${encodeURIComponent(since)}${extra}`
  );
  expect(status).toBe(200);
  expect(body.status).toBe("success");
  return body.body as Delta;
};

describe("GET /api/mails/headers/:account?since= — the response", () => {
  test("an empty delta: key order, types and headers", async () => {
    const box = await newMailbox(server);
    const { status, text, headers, body } = await box.client.get(`/api/mails/headers/${box.address()}?since=${SINCE}`);
    expect(status).toBe(200);
    expect(text).toBe(`{"status":"success","body":{"as_of":"${body.body.as_of}","headers":[],"expunged_ids":[]}}`);
    expect(body.body.as_of).toMatch(ISO_MS);
    expect(headers.get("content-type")).toBe(JSON_TYPE);
    expect(headers.get("content-length")).toBe("95");
    expect(headers.get("etag")).toMatch(BODY_ETAG);
  });

  test("as_of is the database clock two seconds ago", async () => {
    const box = await newMailbox(server);
    const before = Date.now();
    const { as_of } = await delta(box, SINCE);
    const after = Date.now();
    const at = Date.parse(as_of);
    expect(at).toBeGreaterThanOrEqual(before - 2000 - 250);
    expect(at).toBeLessThanOrEqual(after - 2000 + 250);
  });

  test("as_of is accepted back as the next since", async () => {
    const box = await newMailbox(server);
    const first = await delta(box, SINCE);
    const second = await delta(box, first.as_of);
    expect(Date.parse(second.as_of)).toBeGreaterThanOrEqual(Date.parse(first.as_of));
  });

  test("rows have the shape of the full list's rows", async () => {
    const box = await newMailbox(server);
    const mail = await box.mail({
      cc: [addr("c@remote.example", "C")],
      in_reply_to: "<p@x>",
      reference_ids: ["<a@x>", "<b@x>", "<c@x>", "<p@x>"],
      updated: RECENT,
    });
    const full = (await box.client.get(`/api/mails/headers/${box.address()}`)).body.body;
    const result = await delta(box, SINCE);
    expect(result.headers).toEqual(full);
    expect(result.headers).toEqual([headerOf(mail)]);
    expect(result.expunged_ids).toEqual([]);
  });
});

describe("GET /api/mails/headers/:account?since= — what changed", () => {
  test("returns the rows updated after since, strictly", async () => {
    const box = await newMailbox(server);
    await box.mail({ subject: "old", updated: OLD });
    await box.mail({ subject: "at since", updated: "2026-06-01 00:00:00+00" });
    await box.mail({ subject: "a microsecond after", updated: "2026-06-01 00:00:00.000001+00" });
    await box.mail({ subject: "recent", updated: RECENT });
    expect(subjects((await delta(box, SINCE)).headers).sort()).toEqual(["a microsecond after", "recent"]);
    expect(subjects((await delta(box, "2026-05-31T23:59:59.999Z")).headers).sort()).toEqual([
      "a microsecond after",
      "at since",
      "recent",
    ]);
    expect((await delta(box, "2999-01-01T00:00:00Z")).headers).toEqual([]);
    expect((await delta(box, "1970-01-01T00:00:00Z")).headers).toHaveLength(4);
  });

  test("orders rows like the full list: by date then id, not by when they changed", async () => {
    const box = await newMailbox(server);
    await box.mail({ subject: "date 1, changed last", date: "2026-01-01T00:00:00Z", updated: "2026-09-03T00:00:00Z" });
    await box.mail({ subject: "date 3, changed first", date: "2026-01-03T00:00:00Z", updated: "2026-09-01T00:00:00Z" });
    await box.mail({ subject: "date 2", date: "2026-01-02T00:00:00Z", updated: "2026-09-02T00:00:00Z" });
    expect(subjects((await delta(box, SINCE)).headers)).toEqual([
      "date 3, changed first",
      "date 2",
      "date 1, changed last",
    ]);
  });

  test("reports expunged rows and rows that became spam as expunged_ids, not as headers", async () => {
    const box = await newMailbox(server);
    const kept = await box.mail({ subject: "kept", updated: RECENT });
    const expunged = await box.mail({ subject: "expunged", expunged: true, updated: RECENT });
    const spam = await box.mail({ subject: "now spam", is_spam: true, updated: RECENT });
    const oldExpunged = await box.mail({ subject: "expunged long ago", expunged: true, updated: OLD });
    const oldSpam = await box.mail({ subject: "spam long ago", is_spam: true, updated: OLD });
    const result = await delta(box, SINCE);
    expect(result.headers).toEqual([headerOf(kept)]);
    expect([...result.expunged_ids].sort()).toEqual([expunged.mail_id, spam.mail_id].sort());
    expect(result.expunged_ids).not.toContain(oldExpunged.mail_id);
    expect(result.expunged_ids).not.toContain(oldSpam.mail_id);
  });

  test("only looks at the account's rows", async () => {
    const box = await newMailbox(server);
    const other = box.address("other");
    await box.mail({ subject: "other account", to: [addr(other)], updated: RECENT });
    const gone = await box.mail({ subject: "other account, expunged", to: [addr(other)], expunged: true, updated: RECENT });
    expect(await delta(box, SINCE)).toMatchObject({ headers: [], expunged_ids: [] });
    const result = await delta(box, SINCE, "", other);
    expect(subjects(result.headers)).toEqual(["other account"]);
    expect(result.expunged_ids).toEqual([gone.mail_id]);
  });

  test("never returns another user's rows or tombstones", async () => {
    const first = await newMailbox(server);
    const second = await newMailbox(server);
    await second.mail({ to: [addr(first.address())], updated: RECENT });
    await second.mail({ to: [addr(first.address())], expunged: true, updated: RECENT });
    expect(await delta(first, SINCE)).toMatchObject({ headers: [], expunged_ids: [] });
  });

  test("a draft is not a header; an expunged draft is still reported as expunged", async () => {
    const box = await newMailbox(server);
    await box.mail({ subject: "draft", draft: true, updated: RECENT });
    const gone = await box.mail({ subject: "expunged draft", draft: true, expunged: true, updated: RECENT });
    const result = await delta(box, SINCE);
    expect(result.headers).toEqual([]);
    expect(result.expunged_ids).toEqual([gone.mail_id]);
  });

  test("a row that is both expunged and spam is reported once", async () => {
    const box = await newMailbox(server);
    const both = await box.mail({ expunged: true, is_spam: true, updated: RECENT });
    expect((await delta(box, SINCE)).expunged_ids).toEqual([both.mail_id]);
  });
});

describe("GET /api/mails/headers/:account?since= — views", () => {
  test("?sent: rows and tombstones by the From address", async () => {
    const box = await newMailbox(server);
    const me = box.address();
    const sent = { from: [addr(me)], to: [addr("x@remote.example")], envelope_to: [], sent: true };
    await box.mail({ ...sent, subject: "sent recent", updated: RECENT });
    await box.mail({ ...sent, subject: "sent old", updated: OLD });
    const gone = await box.mail({ ...sent, subject: "sent expunged", expunged: true, updated: RECENT });
    await box.mail({ subject: "received recent", updated: RECENT });
    const receivedGone = await box.mail({ subject: "received expunged", expunged: true, updated: RECENT });
    const result = await delta(box, SINCE, "&sent=1");
    expect(subjects(result.headers)).toEqual(["sent recent"]);
    expect(result.expunged_ids).toEqual([gone.mail_id]);
    const received = await delta(box, SINCE);
    expect(subjects(received.headers)).toEqual(["received recent"]);
    expect(received.expunged_ids).toEqual([receivedGone.mail_id]);
  });

  test("?saved: saved rows only; a row that was un-saved gets no tombstone", async () => {
    const box = await newMailbox(server);
    await box.mail({ subject: "saved recent", saved: true, updated: RECENT });
    await box.mail({ subject: "saved old", saved: true, updated: OLD });
    await box.mail({ subject: "un-saved recently", saved: false, updated: RECENT });
    const gone = await box.mail({ subject: "saved expunged", saved: true, expunged: true, updated: RECENT });
    const unsavedGone = await box.mail({ subject: "unsaved expunged", expunged: true, updated: RECENT });
    const result = await delta(box, SINCE, "&saved=1");
    expect(subjects(result.headers)).toEqual(["saved recent"]);
    // Tombstones are not limited to saved rows.
    expect([...result.expunged_ids].sort()).toEqual([gone.mail_id, unsavedGone.mail_id].sort());
  });

  // NOTE: in the spam view a tombstone is "expunged, or not spam, and changed
  // since". That covers a row un-marked as spam, as intended — but also every
  // ordinary mail of the account that arrived or changed in the window, none
  // of which was ever in the spam folder.
  test("?spam: tombstones are expunged spam plus every non-spam row changed since", async () => {
    const box = await newMailbox(server);
    await box.mail({ subject: "spam recent", is_spam: true, updated: RECENT });
    await box.mail({ subject: "spam old", is_spam: true, updated: OLD });
    const spamGone = await box.mail({ subject: "spam expunged", is_spam: true, expunged: true, updated: RECENT });
    const unmarked = await box.mail({ subject: "no longer spam", is_spam: false, updated: RECENT });
    const ordinary = await box.mail({ subject: "ordinary new mail", updated: RECENT });
    const ordinaryOld = await box.mail({ subject: "ordinary old mail", updated: OLD });
    const result = await delta(box, SINCE, "&spam=1");
    expect(subjects(result.headers)).toEqual(["spam recent"]);
    expect([...result.expunged_ids].sort()).toEqual([spamGone.mail_id, unmarked.mail_id, ordinary.mail_id].sort());
    expect(result.expunged_ids).not.toContain(ordinaryOld.mail_id);
  });

  test("?spam: spam flagged sent is neither a header nor, unless expunged, a tombstone", async () => {
    const box = await newMailbox(server);
    await box.mail({ subject: "sent spam", is_spam: true, sent: true, updated: RECENT });
    expect(await delta(box, SINCE, "&spam=1")).toMatchObject({ headers: [], expunged_ids: [] });
  });
});

describe("GET /api/mails/headers/:account?since= — after real changes", () => {
  let box: Mailbox;
  let since: string;
  beforeAll(async () => {
    box = await newMailbox(server);
    since = new Date(Date.now() - 60_000).toISOString();
  });
  const seed = (subject: string) => box.mail({ subject, to: [addr(box.address(subject))], updated: OLD });

  test("marking a mail read or saved puts it in the next delta", async () => {
    const mail = await seed("flags");
    expect((await delta(box, since, "", box.address("flags"))).headers).toEqual([]);
    await box.client.post("/api/mails/mark", { mail_id: mail.mail_id, read: true });
    const afterRead = await delta(box, since, "", box.address("flags"));
    expect(afterRead.headers).toEqual([headerOf(mail, { read: true })]);
    await box.client.post("/api/mails/mark", { mail_id: mail.mail_id, save: true });
    expect((await delta(box, since, "", box.address("flags"))).headers).toEqual([
      headerOf(mail, { read: true, saved: true }),
    ]);
  });

  test("reading a body puts the mail in the next delta once, not on a second read", async () => {
    const mail = await seed("body");
    await box.client.get(`/api/mails/body/${mail.mail_id}`);
    const first = await delta(box, since, "", box.address("body"));
    expect(first.headers).toEqual([headerOf(mail, { read: true })]);
    const updated = (await server.db.query("SELECT updated FROM mails WHERE mail_id = $1", [mail.mail_id]))[0]!.updated;
    await box.client.get(`/api/mails/body/${mail.mail_id}`);
    const again = (await server.db.query("SELECT updated FROM mails WHERE mail_id = $1", [mail.mail_id]))[0]!.updated;
    expect(+again).toBe(+updated);
  });

  test("marking spam moves the mail from headers to expunged_ids, and into the spam view's headers", async () => {
    const mail = await seed("spam");
    await box.client.post("/api/mails/spam/mark", { mail_id: mail.mail_id, is_spam: true });
    const inbox = await delta(box, since, "", box.address("spam"));
    expect(inbox.headers).toEqual([]);
    expect(inbox.expunged_ids).toEqual([mail.mail_id]);
    const spamView = await delta(box, since, "&spam=1", box.address("spam"));
    expect(subjects(spamView.headers)).toEqual(["spam"]);
    expect(spamView.expunged_ids).toEqual([]);
    await box.client.post("/api/mails/spam/mark", { mail_id: mail.mail_id, is_spam: false });
    const back = await delta(box, since, "", box.address("spam"));
    expect(subjects(back.headers)).toEqual(["spam"]);
    expect(back.expunged_ids).toEqual([]);
    expect((await delta(box, since, "&spam=1", box.address("spam"))).expunged_ids).toEqual([mail.mail_id]);
  });

  // NOTE: DELETE /api/mails/:id removes the row outright, so nothing is left
  // to report: a client syncing by delta never learns the mail is gone.
  test("a mail deleted over HTTP leaves no tombstone", async () => {
    const mail = await seed("deleted");
    await box.client.post("/api/mails/mark", { mail_id: mail.mail_id, read: true });
    expect((await delta(box, since, "", box.address("deleted"))).headers).toHaveLength(1);
    expect((await box.client.delete(`/api/mails/${mail.mail_id}`)).body).toEqual({ status: "success" });
    expect(await delta(box, since, "", box.address("deleted"))).toMatchObject({ headers: [], expunged_ids: [] });
  });
});

describe("GET /api/mails/headers/:account?since= — validation", () => {
  let box: Mailbox;
  beforeAll(async () => {
    box = await newMailbox(server);
    await box.mail({ subject: "recent", updated: RECENT });
  });
  const get = (query: string) => box.client.get(`/api/mails/headers/${box.address()}?${query}`);

  test("accepts ISO 8601 and the other spellings both the server and the database read", async () => {
    for (const since of [
      "2026-06-01T00:00:00Z",
      "2026-06-01T00:00:00.000Z",
      "2026-06-01T00:00:00.123456Z",
      "2026-06-01T09:00:00+09:00",
      "2026-05-31T16:00:00-08:00",
      "2026-06-01T00:00:00",
      "2026-06-01T00:00Z",
      "2026-06-01",
      "2026-06-01 00:00:00",
      "2026-06-01 00:00:00.5+00",
      "2026-06-01t00:00:00z",
      "Mon, 01 Jun 2026 00:00:00 GMT",
    ]) {
      const { status, body } = await get(`since=${encodeURIComponent(since)}`);
      expect({ since, status, outcome: body.status, rows: body.body?.headers?.length }).toEqual({
        since,
        status: 200,
        outcome: "success",
        rows: 1,
      });
      expect(body.body.as_of).toMatch(ISO_MS);
    }
  });

  test("reads an offset in since", async () => {
    // 2026-09-01T00:00:00Z is the row's change time.
    expect((await get(`since=${encodeURIComponent("2026-09-01T08:59:59+09:00")}`)).body.body.headers).toHaveLength(1);
    expect((await get(`since=${encodeURIComponent("2026-09-01T09:00:00+09:00")}`)).body.body.headers).toHaveLength(0);
    expect((await get(`since=${encodeURIComponent("2026-08-31T16:00:01-08:00")}`)).body.body.headers).toHaveLength(0);
  });

  test("refuses a since that is no date", async () => {
    for (const since of [
      "", "abc", "now", "today", "infinity", "-infinity", "1777593600000", "2026-13-01", "2026-06-01T25:00:00Z",
      "20260601", "20260601T000000Z", " 2026-06-01T00:00:00Z ", "2026-06-01T00:00:00 PST", "null", "undefined", "true",
    ]) {
      const { status, body } = await get(`since=${encodeURIComponent(since)}`);
      expect({ since, status, body }).toEqual({ since, status: 200, body: BAD_SINCE });
    }
  });

  test("refuses a bare or repeated since", async () => {
    for (const query of [
      "since",
      "since=2026-06-01T00:00:00Z&since=2026-06-02T00:00:00Z",
      "since=2026-06-01T00:00:00Z&since=2026-06-01T00:00:00Z",
      "since=&since=2026-06-01T00:00:00Z",
    ]) {
      const { status, body } = await get(query);
      expect({ query, status, body }).toEqual({ query, status: 200, body: BAD_SINCE });
    }
  });

  test("the refusal is exact", async () => {
    const { status, text, headers } = await get("since=abc");
    expect(status).toBe(200);
    expect(text).toBe('{"status":"failed","message":"Invalid `since` parameter: expected an ISO timestamp."}');
    expect(headers.get("content-type")).toBe(JSON_TYPE);
  });

  test("since wins over limit and cursor: a delta is never paged, and their values are not even checked", async () => {
    for (const query of [
      "since=2026-06-01T00:00:00Z&limit=1",
      "since=2026-06-01T00:00:00Z&limit=0",
      "since=2026-06-01T00:00:00Z&cursor=garbage",
      "limit=abc&cursor=x&since=2026-06-01T00:00:00Z",
    ]) {
      const { body } = await get(query);
      expect({ query, keys: Object.keys(body.body ?? {}) }).toEqual({ query, keys: ["as_of", "headers", "expunged_ids"] });
    }
    expect((await get("since=abc&limit=0")).body).toEqual(BAD_SINCE);
  });

  test("the account check comes before since is read", async () => {
    const { body } = await box.client.get("/api/mails/headers/hello?since=abc");
    expect(body).toEqual(failed("Invalid request. You may not look at other users' emails."));
  });

  // NOTE: `since` is validated with the server's lenient date parser but
  // compared in the database, which is stricter. A value only the former
  // accepts makes the query fail; the failure is swallowed and answered as a
  // successful, empty delta whose as_of is the caller's own text — not a
  // timestamp, and not a refusal.
  test("a since the database rejects is answered as an empty delta echoing the value as as_of", async () => {
    for (const since of ["2020", "2026-02-30T00:00:00Z", "2026-02-30"]) {
      const { status, body } = await get(`since=${encodeURIComponent(since)}`);
      expect({ since, status, body }).toEqual({
        since,
        status: 200,
        body: { status: "success", body: { as_of: since, headers: [], expunged_ids: [] } },
      });
    }
  });
});
