import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { addr, missingId, readMail, startServer, waitFor, type TestServer } from "../harness";
import {
  BAD_REQUEST,
  JSON_TYPE,
  JSON_VALUES,
  NON_OBJECT_BODIES,
  adminMailbox,
  failed,
  newMailbox,
  subjects,
  type Mailbox,
} from "./mails-helpers";

let server: TestServer;
let box: Mailbox;

beforeAll(async () => {
  server = await startServer();
  box = await newMailbox(server);
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

const SUCCESS = { status: "success" };
const NOT_BOOLEAN = failed("is_spam must be a boolean");
const NOT_FOUND = failed("Mail not found or you don't have permission");

const spamMark = (body: unknown, client = box.client) => client.post("/api/mails/spam/mark", body);

const training = (userId: string) =>
  server.db.query<{ word: string; spam_count: number; ham_count: number }>(
    "SELECT word, spam_count, ham_count FROM spam_training WHERE user_id = $1 ORDER BY word",
    [userId]
  );

/** Waits until the user's training holds `spam` spam documents and `ham` ham documents. */
const trainedTo = (userId: string, spam: number, ham: number) =>
  waitFor(
    async () => {
      const rows = await training(userId);
      const total = rows.find((row) => row.word === "__total__");
      return total && total.spam_count === spam && total.ham_count === ham ? rows : undefined;
    },
    { what: `training totals ${spam}/${ham}` }
  );

describe("POST /api/mails/spam/mark — the flag", () => {
  test("is_spam: true flags the mail, with an exact answer", async () => {
    const mail = await box.mail();
    const { status, text, headers } = await spamMark({ mail_id: mail.mail_id, is_spam: true });
    expect(status).toBe(200);
    expect(text).toBe('{"status":"success"}');
    expect(headers.get("content-type")).toBe(JSON_TYPE);
    expect((await readMail(server, mail.mail_id))!.is_spam).toBe(true);
  });

  test("stamps the next mod-sequence and bumps updated; nothing else moves", async () => {
    const fresh = await newMailbox(server);
    const mail = await fresh.mail({ modseq: 7, updated: "2026-01-01T00:00:00Z", read: false, saved: true, spam_score: 3 });
    const before = Date.now();
    expect((await spamMark({ mail_id: mail.mail_id, is_spam: true }, fresh.client)).body).toEqual(SUCCESS);
    const row = await readMail(server, mail.mail_id);
    expect({ is_spam: row!.is_spam, modseq: String(row!.modseq), read: row!.read, saved: row!.saved, spam_score: row!.spam_score, spam_reasons: row!.spam_reasons }).toEqual({
      is_spam: true,
      modseq: "8",
      read: false,
      saved: true,
      spam_score: 3,
      spam_reasons: null,
    });
    expect(+row!.updated).toBeGreaterThanOrEqual(before - 1000);
  });

  test("re-marking with the same value succeeds, leaves the row alone and uses up one mod-sequence number", async () => {
    const fresh = await newMailbox(server);
    const mail = await fresh.mail({ modseq: 7 });
    await spamMark({ mail_id: mail.mail_id, is_spam: true }, fresh.client);
    const first = await readMail(server, mail.mail_id);
    expect(String(first!.modseq)).toBe("8");
    expect((await spamMark({ mail_id: mail.mail_id, is_spam: true }, fresh.client)).body).toEqual(SUCCESS);
    const second = await readMail(server, mail.mail_id);
    expect(String(second!.modseq)).toBe("8");
    expect(+second!.updated).toBe(+first!.updated);
    // The no-op reserved number 9 without stamping it: the next change gets 10.
    expect((await spamMark({ mail_id: mail.mail_id, is_spam: false }, fresh.client)).body).toEqual(SUCCESS);
    const third = await readMail(server, mail.mail_id);
    expect({ is_spam: third!.is_spam, modseq: String(third!.modseq) }).toEqual({ is_spam: false, modseq: "10" });
  });

  test("is_spam: false on a mail that is not spam succeeds and changes nothing", async () => {
    const mail = await box.mail({ modseq: 4, updated: "2026-01-01T00:00:00Z" });
    expect((await spamMark({ mail_id: mail.mail_id, is_spam: false })).body).toEqual(SUCCESS);
    const row = await readMail(server, mail.mail_id);
    expect({ is_spam: row!.is_spam, modseq: String(row!.modseq), updated: new Date(row!.updated).toISOString() }).toEqual({
      is_spam: false,
      modseq: "4",
      updated: "2026-01-01T00:00:00.000Z",
    });
  });

  test("the mail moves between the inbox views and the spam view, and back", async () => {
    const fresh = await newMailbox(server);
    const me = fresh.address();
    const mail = await fresh.mail({ subject: "Movable" });
    const views = async () => ({
      inbox: subjects((await fresh.client.get(`/api/mails/headers/${me}`)).body.body),
      spam: subjects((await fresh.client.get(`/api/mails/headers/${me}?spam=1`)).body.body),
      unified: subjects((await fresh.client.get("/api/mails/unified")).body.body),
      accounts: Object.fromEntries(
        Object.entries((await fresh.client.get("/api/mails/accounts")).body.body).map(([list, rows]) => [list, (rows as unknown[]).length])
      ),
    });
    expect(await views()).toEqual({ inbox: ["Movable"], spam: [], unified: ["Movable"], accounts: { received: 1, sent: 0, spam: 0, shared: 0 } });
    await spamMark({ mail_id: mail.mail_id, is_spam: true }, fresh.client);
    expect(await views()).toEqual({ inbox: [], spam: ["Movable"], unified: [], accounts: { received: 0, sent: 0, spam: 1, shared: 0 } });
    await spamMark({ mail_id: mail.mail_id, is_spam: false }, fresh.client);
    expect(await views()).toEqual({ inbox: ["Movable"], spam: [], unified: ["Movable"], accounts: { received: 1, sent: 0, spam: 0, shared: 0 } });
  });

  test("accepts the id in upper case and ignores unknown fields", async () => {
    const mail = await box.mail();
    expect((await spamMark({ mail_id: mail.mail_id.toUpperCase(), is_spam: true, read: true, expunged: true })).body).toEqual(SUCCESS);
    const row = await readMail(server, mail.mail_id);
    expect({ is_spam: row!.is_spam, read: row!.read, expunged: row!.expunged }).toEqual({ is_spam: true, read: false, expunged: false });
  });

  test("works on sent mail, drafts and deleted mail", async () => {
    for (const row of [{ sent: true }, { draft: true }, { deleted: true }]) {
      const mail = await box.mail(row);
      expect({ row, body: (await spamMark({ mail_id: mail.mail_id, is_spam: true })).body }).toEqual({ row, body: SUCCESS });
      expect((await readMail(server, mail.mail_id))!.is_spam).toBe(true);
    }
  });

  test("an expunged mail is refused like a missing one, either way, and nothing changes", async () => {
    for (const is_spam of [true, false]) {
      const mail = await box.mail({ expunged: true, is_spam: !is_spam, modseq: 5, updated: "2026-01-01T00:00:00Z" });
      const { text } = await spamMark({ mail_id: mail.mail_id, is_spam });
      expect({ is_spam, text }).toEqual({ is_spam, text: '{"status":"failed","message":"Mail not found or you don\'t have permission"}' });
      const row = await readMail(server, mail.mail_id);
      expect({ is_spam: row!.is_spam, modseq: String(row!.modseq), updated: new Date(row!.updated).toISOString() }).toEqual({
        is_spam: !is_spam,
        modseq: "5",
        updated: "2026-01-01T00:00:00.000Z",
      });
    }
  });
});

describe("POST /api/mails/spam/mark — training", () => {
  test("marking spam trains the user's classifier with the mail's words, once", async () => {
    const fresh = await newMailbox(server);
    const mail = await fresh.mail({
      subject: "Cheap Pills NOW",
      text: "Buy cheap pills online, limited offer 123 ab",
      html: "<p class=\"promo\">Buy <b>cheap</b> pills at Ünïcode-shop</p>",
    });
    expect(await training(fresh.id)).toEqual([]);
    await spamMark({ mail_id: mail.mail_id, is_spam: true }, fresh.client);
    const rows = await trainedTo(fresh.id, 1, 0);
    // Letters a-z only, three or more, lower-cased, each once; markup, digits and short words dropped.
    expect(rows).toEqual(
      ["__total__", "buy", "cheap", "code", "limited", "now", "offer", "online", "pills", "shop"].map((word) => ({
        word,
        spam_count: 1,
        ham_count: 0,
      }))
    );
  });

  test("re-marking with the same value does not train again; flipping trains the other class", async () => {
    const fresh = await newMailbox(server);
    const mail = await fresh.mail({ subject: "Lottery winner", text: "claim prize", html: "" });
    await spamMark({ mail_id: mail.mail_id, is_spam: true }, fresh.client);
    await trainedTo(fresh.id, 1, 0);
    await spamMark({ mail_id: mail.mail_id, is_spam: true }, fresh.client);
    await spamMark({ mail_id: mail.mail_id, is_spam: true }, fresh.client);
    // Flip to not-spam: one ham document. Had the repeats trained, spam would be 3 by now.
    await spamMark({ mail_id: mail.mail_id, is_spam: false }, fresh.client);
    const rows = await trainedTo(fresh.id, 1, 1);
    expect(rows).toEqual(
      ["__total__", "claim", "lottery", "prize", "winner"].map((word) => ({ word, spam_count: 1, ham_count: 1 }))
    );
    await spamMark({ mail_id: mail.mail_id, is_spam: false }, fresh.client);
    await spamMark({ mail_id: mail.mail_id, is_spam: true }, fresh.client);
    expect((await trainedTo(fresh.id, 2, 1)).find((row) => row.word === "lottery")).toEqual({ word: "lottery", spam_count: 2, ham_count: 1 });
  });

  test("is_spam: false on a mail that was never spam trains nothing", async () => {
    const fresh = await newMailbox(server);
    const untouched = await fresh.mail({ subject: "Ordinary words here", text: "plain", html: "" });
    const marker = await fresh.mail({ subject: "Marker", text: "", html: "" });
    await spamMark({ mail_id: untouched.mail_id, is_spam: false }, fresh.client);
    await spamMark({ mail_id: marker.mail_id, is_spam: true }, fresh.client);
    expect(await trainedTo(fresh.id, 1, 0)).toEqual([
      { word: "__total__", spam_count: 1, ham_count: 0 },
      { word: "marker", spam_count: 1, ham_count: 0 },
    ]);
  });

  test("words accumulate across mails, and each user has their own training", async () => {
    const first = await newMailbox(server);
    const second = await newMailbox(server);
    const a = await first.mail({ subject: "alpha beta", text: "", html: "" });
    const b = await first.mail({ subject: "beta gamma", text: "", html: "" });
    const c = await second.mail({ subject: "beta", text: "", html: "" });
    await spamMark({ mail_id: a.mail_id, is_spam: true }, first.client);
    await trainedTo(first.id, 1, 0);
    await spamMark({ mail_id: b.mail_id, is_spam: true }, first.client);
    await spamMark({ mail_id: c.mail_id, is_spam: true }, second.client);
    expect(await trainedTo(first.id, 2, 0)).toEqual([
      { word: "__total__", spam_count: 2, ham_count: 0 },
      { word: "alpha", spam_count: 1, ham_count: 0 },
      { word: "beta", spam_count: 2, ham_count: 0 },
      { word: "gamma", spam_count: 1, ham_count: 0 },
    ]);
    expect(await trainedTo(second.id, 1, 0)).toEqual([
      { word: "__total__", spam_count: 1, ham_count: 0 },
      { word: "beta", spam_count: 1, ham_count: 0 },
    ]);
  });

  test("a mail with no usable word is flagged but trains nothing", async () => {
    const fresh = await newMailbox(server);
    const empty = await fresh.mail({ subject: "12 34 ab", text: "", html: "<p></p>" });
    const marker = await fresh.mail({ subject: "Sentinel", text: "", html: "" });
    expect((await spamMark({ mail_id: empty.mail_id, is_spam: true }, fresh.client)).body).toEqual(SUCCESS);
    expect((await readMail(server, empty.mail_id))!.is_spam).toBe(true);
    await spamMark({ mail_id: marker.mail_id, is_spam: true }, fresh.client);
    expect((await trainedTo(fresh.id, 1, 0)).map((row) => row.word)).toEqual(["__total__", "sentinel"]);
  });

  test("the sender's address is not a training word", async () => {
    const fresh = await newMailbox(server);
    const mail = await fresh.mail({ subject: "Hello", text: "", html: "", from: [addr("spammer@badsender.example", "Evil Person")] });
    await spamMark({ mail_id: mail.mail_id, is_spam: true }, fresh.client);
    expect((await trainedTo(fresh.id, 1, 0)).map((row) => row.word)).toEqual(["__total__", "hello"]);
  });

  test("a refused mark trains nothing", async () => {
    const fresh = await newMailbox(server);
    const other = await newMailbox(server);
    const theirs = await other.mail({ subject: "Not yours", text: "", html: "" });
    const marker = await fresh.mail({ subject: "Checkpoint", text: "", html: "" });
    expect((await spamMark({ mail_id: theirs.mail_id, is_spam: true }, fresh.client)).body).toEqual(NOT_FOUND);
    await spamMark({ mail_id: marker.mail_id, is_spam: true }, fresh.client);
    expect((await trainedTo(fresh.id, 1, 0)).map((row) => row.word)).toEqual(["__total__", "checkpoint"]);
    expect(await training(other.id)).toEqual([]);
  });

  test("a mark refused for an expunged mail trains nothing", async () => {
    const fresh = await newMailbox(server);
    const gone = await fresh.mail({ subject: "Vanished words", text: "", html: "", expunged: true });
    const marker = await fresh.mail({ subject: "Checkpoint", text: "", html: "" });
    expect((await spamMark({ mail_id: gone.mail_id, is_spam: true }, fresh.client)).body).toEqual(NOT_FOUND);
    await spamMark({ mail_id: marker.mail_id, is_spam: true }, fresh.client);
    expect((await trainedTo(fresh.id, 1, 0)).map((row) => row.word)).toEqual(["__total__", "checkpoint"]);
  });
});

describe("POST /api/mails/spam/mark — refusals", () => {
  test("is_spam must be a boolean: every other JSON type is refused, before the id is looked at", async () => {
    const mail = await box.mail();
    for (const [label, value] of JSON_VALUES) {
      if (typeof value === "boolean") continue;
      for (const mail_id of [mail.mail_id, "abc"]) {
        const { status, body } = await spamMark({ mail_id, is_spam: value });
        expect({ label, mail_id, status, body }).toEqual({ label, mail_id, status: 200, body: NOT_BOOLEAN });
      }
    }
    for (const body of [{}, { mail_id: mail.mail_id }, { mail_id: mail.mail_id, isSpam: true }, { mail_id: mail.mail_id, spam: true }]) {
      expect({ body, answer: (await spamMark(body)).body }).toEqual({ body, answer: NOT_BOOLEAN });
    }
    expect((await readMail(server, mail.mail_id))!.is_spam).toBe(false);
  });

  test("mail_id must be a UUID string: every other JSON type is refused as not found", async () => {
    for (const [label, value] of JSON_VALUES) {
      const { status, body } = await spamMark({ mail_id: value, is_spam: true });
      expect({ label, status, body }).toEqual({ label, status: 200, body: NOT_FOUND });
    }
    expect((await spamMark({ is_spam: true })).body).toEqual(NOT_FOUND);
    expect((await spamMark({ is_spam: false })).body).toEqual(NOT_FOUND);
  });

  test("the refusals are exact", async () => {
    const notBoolean = await spamMark({ mail_id: missingId() });
    expect(notBoolean.status).toBe(200);
    expect(notBoolean.text).toBe('{"status":"failed","message":"is_spam must be a boolean"}');
    const notFound = await spamMark({ mail_id: missingId(), is_spam: true });
    expect(notFound.status).toBe(200);
    expect(notFound.text).toBe('{"status":"failed","message":"Mail not found or you don\'t have permission"}');
  });

  test("a mail that does not exist and another user's mail get the same refusal", async () => {
    const other = await newMailbox(server);
    const admin = await adminMailbox(server);
    const theirs = await other.mail({ modseq: 5 });
    for (const is_spam of [true, false]) {
      expect((await spamMark({ mail_id: missingId(), is_spam })).body).toEqual(NOT_FOUND);
      expect((await spamMark({ mail_id: theirs.mail_id, is_spam })).body).toEqual(NOT_FOUND);
      expect((await spamMark({ mail_id: theirs.mail_id, is_spam }, admin.client)).body).toEqual(NOT_FOUND);
    }
    const row = await readMail(server, theirs.mail_id);
    expect({ is_spam: row!.is_spam, modseq: String(row!.modseq) }).toEqual({ is_spam: false, modseq: "5" });
  });

  test("bodies that are not a JSON object", async () => {
    for (const { label, body, headers, parsed } of NON_OBJECT_BODIES) {
      const response = await box.client.request("POST", "/api/mails/spam/mark", { body, headers });
      const expected = parsed === "bad-request" ? { status: 400, body: BAD_REQUEST } : { status: 200, body: NOT_BOOLEAN };
      expect({ label, status: response.status, body: response.body }).toEqual({ label, ...expected });
    }
  });

  test("a body that is not sent as application/json is not read", async () => {
    const mail = await box.mail();
    const json = JSON.stringify({ mail_id: mail.mail_id, is_spam: true });
    for (const [label, options] of [
      ["text/plain", { body: json, headers: { "Content-Type": "text/plain" } }],
      ["form-encoded", { body: `mail_id=${mail.mail_id}&is_spam=true`, headers: { "Content-Type": "application/x-www-form-urlencoded" } }],
      ["no content type", { body: new TextEncoder().encode(json) }],
    ] as const) {
      const { status, body } = await box.client.request("POST", "/api/mails/spam/mark", options);
      expect({ label, status, body }).toEqual({ label, status: 200, body: NOT_BOOLEAN });
    }
    expect((await readMail(server, mail.mail_id))!.is_spam).toBe(false);
  });

  test("the route is /spam/mark only", async () => {
    for (const path of ["/api/mails/spam", "/api/mails/spam/", "/api/mails/spam/unmark", "/api/mails/spam/mark/now", "/api/mails/spam-mark"]) {
      const { status } = await box.client.post(path, { mail_id: missingId(), is_spam: true });
      expect({ path, status }).toEqual({ path, status: 404 });
    }
  });
});
