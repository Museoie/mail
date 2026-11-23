import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { missingId, readMail, startServer, type TestServer } from "../harness";
import {
  BAD_REQUEST,
  JSON_TYPE,
  JSON_VALUES,
  NON_OBJECT_BODIES,
  adminMailbox,
  failed,
  multipart,
  fields,
  newMailbox,
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
const BAD_BODY = failed("Invalid request body.");
const BAD_ID = failed("mail_id must be a valid id");
const NOT_YOURS = failed("Invalid request. You may not manipulate other users' email");

const mark = (body: unknown, client = box.client) => client.post("/api/mails/mark", body);
const flags = async (mailId: string) => {
  const row = await readMail(server, mailId);
  return { read: row!.read, saved: row!.saved, modseq: String(row!.modseq), updated: +row!.updated };
};
const starred = (mailId: string) =>
  server.db.query("SELECT mailbox FROM mail_mailbox_uid WHERE mail_id = $1 AND mailbox = 'Starred'", [mailId]);

describe("POST /api/mails/mark — read", () => {
  test("read: true marks the mail read, with an exact answer", async () => {
    const mail = await box.mail();
    const { status, text, headers } = await mark({ mail_id: mail.mail_id, read: true });
    expect(status).toBe(200);
    expect(text).toBe('{"status":"success"}');
    expect(headers.get("content-type")).toBe(JSON_TYPE);
    expect(headers.get("etag")).toBe('W/"14-Y53wuE/mmbSikKcT/WualL1N65U"');
    expect((await flags(mail.mail_id)).read).toBe(true);
  });

  test("stamps the next mod-sequence and bumps updated, once", async () => {
    const fresh = await newMailbox(server);
    const mail = await fresh.mail({ modseq: 7, updated: "2026-01-01T00:00:00Z" });
    const before = Date.now();
    expect((await mark({ mail_id: mail.mail_id, read: true }, fresh.client)).body).toEqual(SUCCESS);
    const first = await flags(mail.mail_id);
    expect(first).toMatchObject({ read: true, saved: false, modseq: "8" });
    expect(first.updated).toBeGreaterThanOrEqual(before - 1000);
    // Marking it again succeeds and writes nothing.
    expect((await mark({ mail_id: mail.mail_id, read: true }, fresh.client)).body).toEqual(SUCCESS);
    expect(await flags(mail.mail_id)).toEqual(first);
  });

  test("the account's unread count follows", async () => {
    const fresh = await newMailbox(server);
    const mail = await fresh.mail();
    await fresh.mail();
    const unread = async () => (await fresh.client.get("/api/mails/accounts")).body.body.received[0].unread_doc_count;
    expect(await unread()).toBe(2);
    await mark({ mail_id: mail.mail_id, read: true }, fresh.client);
    expect(await unread()).toBe(1);
  });

  test("there is no way to mark a mail unread: read: false is accepted and does nothing", async () => {
    const mail = await box.mail({ read: true, modseq: 4 });
    expect((await mark({ mail_id: mail.mail_id, read: false })).body).toEqual(SUCCESS);
    expect(await flags(mail.mail_id)).toMatchObject({ read: true, modseq: "4" });
  });

  test("only the boolean true marks read; every other value is accepted and ignored", async () => {
    for (const [label, value] of JSON_VALUES) {
      const mail = await box.mail();
      const { status, body } = await mark({ mail_id: mail.mail_id, read: value });
      expect({ label, status, body }).toEqual({ label, status: 200, body: SUCCESS });
      expect({ label, ...(await flags(mail.mail_id)) }).toMatchObject({ label, read: value === true, saved: false });
    }
  });
});

describe("POST /api/mails/mark — save", () => {
  test("save: true and save: false set and clear the saved flag", async () => {
    const fresh = await newMailbox(server);
    const mail = await fresh.mail({ modseq: 7, updated: "2026-01-01T00:00:00Z" });
    expect((await mark({ mail_id: mail.mail_id, save: true }, fresh.client)).body).toEqual(SUCCESS);
    const saved = await flags(mail.mail_id);
    expect(saved).toMatchObject({ read: false, saved: true, modseq: "8" });
    expect(saved.updated).toBeGreaterThan(Date.parse("2026-01-01T00:00:00Z"));
    expect((await mark({ mail_id: mail.mail_id, save: false }, fresh.client)).body).toEqual(SUCCESS);
    expect(await flags(mail.mail_id)).toMatchObject({ read: false, saved: false, modseq: "9" });
  });

  test("saving keeps a Starred mailbox entry for the mail; un-saving removes it", async () => {
    const mail = await box.mail();
    expect(await starred(mail.mail_id)).toEqual([]);
    await mark({ mail_id: mail.mail_id, save: true });
    expect(await starred(mail.mail_id)).toEqual([{ mailbox: "Starred" }]);
    await mark({ mail_id: mail.mail_id, save: true });
    expect(await starred(mail.mail_id)).toEqual([{ mailbox: "Starred" }]);
    await mark({ mail_id: mail.mail_id, save: false });
    expect(await starred(mail.mail_id)).toEqual([]);
  });

  test("repeating the same value succeeds without a new mod-sequence", async () => {
    const fresh = await newMailbox(server);
    const mail = await fresh.mail({ modseq: 7 });
    await mark({ mail_id: mail.mail_id, save: true }, fresh.client);
    const first = await flags(mail.mail_id);
    expect((await mark({ mail_id: mail.mail_id, save: true }, fresh.client)).body).toEqual(SUCCESS);
    expect(await flags(mail.mail_id)).toEqual(first);
    const unsaved = await fresh.mail({ modseq: 2 });
    expect((await mark({ mail_id: unsaved.mail_id, save: false }, fresh.client)).body).toEqual(SUCCESS);
    expect(await flags(unsaved.mail_id)).toMatchObject({ saved: false, modseq: "2" });
  });

  test("only a boolean changes the saved flag; every other value is accepted and ignored", async () => {
    for (const [label, value] of JSON_VALUES) {
      for (const initial of [false, true]) {
        const mail = await box.mail({ saved: initial });
        const { status, body } = await mark({ mail_id: mail.mail_id, save: value });
        expect({ label, status, body }).toEqual({ label, status: 200, body: SUCCESS });
        const expected = typeof value === "boolean" ? value : initial;
        expect({ label, initial, ...(await flags(mail.mail_id)) }).toMatchObject({ label, initial, saved: expected, read: false });
      }
    }
  });

  test("the field is `save`; `saved` is not read", async () => {
    const mail = await box.mail();
    expect((await mark({ mail_id: mail.mail_id, saved: true })).body).toEqual(SUCCESS);
    expect((await flags(mail.mail_id)).saved).toBe(false);
  });
});

describe("POST /api/mails/mark — both, neither, extras", () => {
  test("read and save together apply both, with one mod-sequence each", async () => {
    const fresh = await newMailbox(server);
    const mail = await fresh.mail({ modseq: 7 });
    expect((await mark({ mail_id: mail.mail_id, read: true, save: true }, fresh.client)).body).toEqual(SUCCESS);
    expect(await flags(mail.mail_id)).toMatchObject({ read: true, saved: true, modseq: "9" });
  });

  test("a body with only a mail_id succeeds and changes nothing", async () => {
    const mail = await box.mail({ modseq: 5, updated: "2026-01-01T00:00:00Z" });
    expect((await mark({ mail_id: mail.mail_id })).body).toEqual(SUCCESS);
    expect(await flags(mail.mail_id)).toEqual({
      read: false,
      saved: false,
      modseq: "5",
      updated: Date.parse("2026-01-01T00:00:00Z"),
    });
  });

  test("unknown fields are ignored: no other column can be set", async () => {
    const mail = await box.mail();
    expect(
      (await mark({ mail_id: mail.mail_id, is_spam: true, deleted: true, expunged: true, sent: true, subject: "x", user_id: missingId() })).body
    ).toEqual(SUCCESS);
    const row = await readMail(server, mail.mail_id);
    expect({ is_spam: row!.is_spam, deleted: row!.deleted, expunged: row!.expunged, sent: row!.sent, subject: row!.subject }).toEqual({
      is_spam: false,
      deleted: false,
      expunged: false,
      sent: false,
      subject: mail.subject,
    });
  });

  test("accepts the id in upper case", async () => {
    const mail = await box.mail();
    expect((await mark({ mail_id: mail.mail_id.toUpperCase(), read: true })).body).toEqual(SUCCESS);
    expect((await flags(mail.mail_id)).read).toBe(true);
  });

  test("works on sent mail, spam, drafts and deleted mail", async () => {
    for (const row of [{ sent: true }, { is_spam: true }, { draft: true }, { deleted: true }]) {
      const mail = await box.mail(row);
      expect({ row, body: (await mark({ mail_id: mail.mail_id, read: true, save: true })).body }).toEqual({ row, body: SUCCESS });
      expect(await flags(mail.mail_id)).toMatchObject({ read: true, saved: true });
    }
  });

  test("an expunged mail is refused like a missing one, and nothing changes", async () => {
    const mail = await box.mail({ expunged: true, modseq: 5, updated: "2026-01-01T00:00:00Z" });
    for (const body of [{ read: true }, { save: true }, { read: true, save: false }]) {
      const answer = await mark({ mail_id: mail.mail_id, ...body });
      expect({ body, text: answer.text }).toEqual({
        body,
        text: '{"status":"failed","message":"Invalid request. You may not manipulate other users\' email"}',
      });
    }
    expect(await flags(mail.mail_id)).toEqual({ read: false, saved: false, modseq: "5", updated: Date.parse("2026-01-01T00:00:00Z") });
    expect(await starred(mail.mail_id)).toEqual([]);
  });
});

describe("POST /api/mails/mark — refusals", () => {
  test("mail_id must be a UUID string: every other JSON type is refused", async () => {
    for (const [label, value] of JSON_VALUES) {
      const { status, body } = await mark({ mail_id: value, read: true, save: true });
      expect({ label, status, body }).toEqual({ label, status: 200, body: BAD_ID });
    }
    for (const body of [{}, { read: true }, { save: false }, { id: missingId() }, { mailId: missingId() }]) {
      expect({ body, answer: (await mark(body)).body }).toEqual({ body, answer: BAD_ID });
    }
  });

  test("a string that is not exactly a UUID is refused", async () => {
    const mail = await box.mail();
    const id = mail.mail_id;
    for (const value of [` ${id}`, `${id} `, `{${id}}`, id.replace(/-/g, ""), `${id}0`, id.slice(1), `urn:uuid:${id}`, `${id}\n`, "x".repeat(5000)]) {
      const { body } = await mark({ mail_id: value, read: true });
      expect({ value: value.slice(0, 50), body }).toEqual({ value: value.slice(0, 50), body: BAD_ID });
    }
    expect((await flags(id)).read).toBe(false);
  });

  test("the refusals are exact", async () => {
    const bad = await mark({ mail_id: "abc" });
    expect(bad.status).toBe(200);
    expect(bad.text).toBe('{"status":"failed","message":"mail_id must be a valid id"}');
    const missing = await mark({ mail_id: missingId(), read: true });
    expect(missing.status).toBe(200);
    expect(missing.text).toBe('{"status":"failed","message":"Invalid request. You may not manipulate other users\' email"}');
  });

  test("a mail that does not exist and another user's mail get the same refusal, and nothing changes", async () => {
    const other = await newMailbox(server);
    const admin = await adminMailbox(server);
    const theirs = await other.mail();
    expect((await mark({ mail_id: missingId(), read: true })).body).toEqual(NOT_YOURS);
    for (const client of [box.client, admin.client]) {
      expect((await mark({ mail_id: theirs.mail_id, read: true, save: true }, client)).body).toEqual(NOT_YOURS);
    }
    expect(await flags(theirs.mail_id)).toMatchObject({ read: false, saved: false });
    expect(await starred(theirs.mail_id)).toEqual([]);
  });

  test("bodies that are not a JSON object", async () => {
    for (const { label, body, headers, parsed } of NON_OBJECT_BODIES) {
      const response = await box.client.request("POST", "/api/mails/mark", { body, headers });
      const expected =
        parsed === "bad-request"
          ? { status: 400, body: BAD_REQUEST }
          : { status: 200, body: parsed === "array" ? BAD_BODY : BAD_ID };
      expect({ label, status: response.status, body: response.body }).toEqual({ label, ...expected });
    }
    const arrayOfObjects = await box.client.request("POST", "/api/mails/mark", {
      body: JSON.stringify([{ mail_id: missingId(), read: true }]),
      headers: { "Content-Type": "application/json" },
    });
    expect(arrayOfObjects.body).toEqual(BAD_BODY);
  });

  test("a body that is not sent as application/json is not read at all", async () => {
    const mail = await box.mail();
    const json = JSON.stringify({ mail_id: mail.mail_id, read: true });
    const form = multipart(fields({ mail_id: mail.mail_id, read: "true" }));
    const attempts: [string, { body: BodyInit; headers?: Record<string, string> }][] = [
      ["text/plain", { body: json, headers: { "Content-Type": "text/plain" } }],
      ["no content type", { body: new TextEncoder().encode(json) }],
      ["application/ld+json", { body: json, headers: { "Content-Type": "application/ld+json" } }],
      ["form-encoded", { body: `mail_id=${mail.mail_id}&read=true`, headers: { "Content-Type": "application/x-www-form-urlencoded" } }],
      ["multipart", { body: form.body, headers: form.headers }],
    ];
    for (const [label, options] of attempts) {
      const { status, body } = await box.client.request("POST", "/api/mails/mark", options);
      expect({ label, status, body }).toEqual({ label, status: 200, body: BAD_ID });
    }
    expect((await flags(mail.mail_id)).read).toBe(false);
  });

  test("application/json is recognized in any case, with a utf-8 charset or a byte-order mark", async () => {
    const mail = await box.mail();
    const json = JSON.stringify({ mail_id: mail.mail_id });
    for (const [label, body, type] of [
      ["upper case", json, "APPLICATION/JSON"],
      ["charset utf-8", json, "application/json; charset=utf-8"],
      ["charset UTF-8 quoted", json, 'application/json; charset="UTF-8"'],
      ["byte-order mark", `﻿${json}`, "application/json"],
      ["leading whitespace", ` \n\t${json}`, "application/json"],
    ] as const) {
      const { status, body: answer } = await box.client.request("POST", "/api/mails/mark", { body, headers: { "Content-Type": type } });
      expect({ label, status, answer }).toEqual({ label, status: 200, answer: SUCCESS });
    }
  });

  test("a JSON body in another charset is a 415", async () => {
    const { status, body } = await box.client.request("POST", "/api/mails/mark", {
      body: "{}",
      headers: { "Content-Type": "application/json; charset=iso-8859-1" },
    });
    expect(status).toBe(415);
    expect(body).toEqual({ status: "error", message: "Unsupported Media Type" });
  });

  test("duplicate keys: the last one wins", async () => {
    const mail = await box.mail();
    const { body } = await box.client.request("POST", "/api/mails/mark", {
      body: `{"mail_id":"abc","mail_id":"${mail.mail_id}","read":false,"read":true}`,
      headers: { "Content-Type": "application/json" },
    });
    expect(body).toEqual(SUCCESS);
    expect((await flags(mail.mail_id)).read).toBe(true);
  });
});
