import { afterAll, beforeAll, beforeEach, describe, expect, test } from "bun:test";
import { addr, missingId, readMail, startServer, type ApiClient, type InsertedMail, type TestServer } from "../harness";
import {
  BAD_REQUEST,
  JSON_TYPE,
  NOT_LOGGED_IN,
  adminMailbox,
  countMails,
  failed,
  fields,
  headerOf,
  multipart,
  newMailbox,
  writeAttachment,
  type Mailbox,
} from "./mails-helpers";

const RO_PASSWORD = "read-only-password-e2e";
const ATTACHMENT_ID = "abababab-abab-4bab-8bab-abababababab";

let server: TestServer;
let admin: Mailbox;
let readOnly: ApiClient;
let mail: InsertedMail;

beforeAll(async () => {
  server = await startServer({ env: { ADMIN_RO_PASSWORD: RO_PASSWORD } });
  admin = await adminMailbox(server);
  readOnly = server.client();
  await readOnly.login("admin-ro", RO_PASSWORD);
  writeAttachment(server, ATTACHMENT_ID, "admin's file");
  mail = await admin.mail({
    to: [addr("hello@mail.test", "Hello")],
    subject: "Admin papaya mail",
    text: "Read-only sessions may read this.",
    html: "<p>Read-only sessions may read this.</p>",
    message_id: "<ro-root@remote.example>",
    attachments: [{ content: { data: ATTACHMENT_ID }, filename: "a.txt", contentType: "text/plain", size: 12 }],
    modseq: 7,
    updated: "2026-01-01T00:00:00Z",
    date: "2026-01-02T00:00:00Z",
  });
  await admin.client.post("/api/mails/spam-allowlist", { pattern: "trusted@example.com" });
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

beforeEach(() => {
  server.mailgun.reset();
});

const refusal = (action: string) => failed(`${action} not permitted for read-only user (admin-ro).`);
const MARKING = refusal("Marking mail");
const SENDING = refusal("Sending mail");
const DELETING = refusal("Deleting mail");
const MARKING_SPAM = refusal("Marking spam");
const ADDING = refusal("Adding to the spam allowlist");
const REMOVING = refusal("Removing from the spam allowlist");

const untouched = async () => {
  const row = await readMail(server, mail.mail_id);
  expect(row).toBeDefined();
  expect({
    read: row!.read,
    saved: row!.saved,
    is_spam: row!.is_spam,
    expunged: row!.expunged,
    modseq: String(row!.modseq),
    updated: new Date(row!.updated).toISOString(),
  }).toEqual({ read: false, saved: false, is_spam: false, expunged: false, modseq: "7", updated: "2026-01-01T00:00:00.000Z" });
};

describe("the read-only session", () => {
  test("signs in as admin-ro and is told it acts as admin, read-only", async () => {
    const client = server.client();
    const { body } = await client.login("admin-ro", RO_PASSWORD);
    expect(body.body).toMatchObject({ username: "admin", isReadOnly: true, authenticatedAs: "admin-ro" });
    expect(body.body.id).toBe(admin.id);
  });

  test("the role exists only when ADMIN_RO_PASSWORD is set", async () => {
    const plain = await startServer();
    try {
      const client = plain.client();
      const login = await client.post("/api/users/login", { username: "admin-ro", password: RO_PASSWORD });
      expect(login.body.status).toBe("failed");
      expect((await client.get("/api/mails/accounts")).body).toEqual(NOT_LOGGED_IN);
    } finally {
      await plain.stop();
    }
  }, 90_000);
});

describe("read-only: every read route answers with admin's data", () => {
  test("GET /domain and /accounts", async () => {
    expect((await readOnly.get("/api/mails/domain")).body).toEqual({ status: "success", body: "mail.test" });
    const mine = await admin.client.get("/api/mails/accounts");
    const theirs = await readOnly.get("/api/mails/accounts");
    expect(theirs.status).toBe(200);
    expect(theirs.text).toBe(mine.text);
    expect(theirs.body.body.received.map((a: { key: string }) => a.key)).toContain("hello@mail.test");
  });

  test("GET /headers/:account in every form, for any account like admin", async () => {
    for (const query of ["", "?limit=5", "?since=2020-01-01T00:00:00Z", "?sent=1", "?saved=1", "?spam=1"]) {
      const mine = await admin.client.get(`/api/mails/headers/hello@mail.test${query}`);
      const theirs = await readOnly.get(`/api/mails/headers/hello@mail.test${query}`);
      expect({ query, status: theirs.status, outcome: theirs.body.status }).toEqual({ query, status: 200, outcome: "success" });
      if (!query.includes("since")) expect(theirs.text).toBe(mine.text);
    }
    expect((await readOnly.get("/api/mails/headers/hello@mail.test")).body.body).toEqual([headerOf(mail, { snippet: "Read-only sessions may read this." })]);
    // Like admin, it may name any account — and like admin it only ever sees admin's rows.
    const user = await newMailbox(server);
    await user.mail({ subject: "A user's own mail" });
    expect((await readOnly.get(`/api/mails/headers/${user.address()}`)).body).toEqual({ status: "success", body: [] });
  });

  test("GET /unified in every form", async () => {
    for (const query of ["", "?limit=5", "?since=2020-01-01T00:00:00Z"]) {
      const { status, body } = await readOnly.get(`/api/mails/unified${query}`);
      expect({ query, status, outcome: body.status }).toEqual({ query, status: 200, outcome: "success" });
    }
    expect((await readOnly.get("/api/mails/unified")).body.body.map((h: { id: string }) => h.id)).toContain(mail.mail_id);
  });

  test("GET /thread, /search, /search-accounts and /spam-allowlist", async () => {
    expect((await readOnly.get(`/api/mails/thread/${mail.mail_id}`)).body.body.map((h: { id: string }) => h.id)).toEqual([mail.mail_id]);
    expect((await readOnly.get("/api/mails/search/papaya")).body.body.map((h: { id: string }) => h.id)).toEqual([mail.mail_id]);
    expect((await readOnly.get("/api/mails/search-accounts/papaya")).body.body.map((a: { key: string }) => a.key)).toEqual(["hello@mail.test"]);
    expect((await readOnly.get("/api/mails/spam-allowlist")).body.body.map((e: { pattern: string }) => e.pattern)).toEqual(["trusted@example.com"]);
  });

  test("GET /attachment serves admin's file", async () => {
    const { status, text, headers } = await readOnly.get(`/api/mails/attachment/${ATTACHMENT_ID}`);
    expect(status).toBe(200);
    expect(text).toBe("admin's file");
    expect(headers.get("content-type")).toBe("application/octet-stream");
  });

  test("it cannot read a user's mail any more than admin can", async () => {
    const user = await newMailbox(server);
    const theirs = await user.mail();
    expect((await readOnly.get(`/api/mails/body/${theirs.mail_id}`)).body).toEqual(failed("No email is found."));
    expect((await readOnly.get(`/api/mails/thread/${theirs.mail_id}`)).body).toEqual(failed("No email is found."));
  });
});

describe("read-only: reading a body does not mark it read", () => {
  test("GET /body answers the body and leaves the row alone", async () => {
    const { status, body } = await readOnly.get(`/api/mails/body/${mail.mail_id}`);
    expect(status).toBe(200);
    expect(body).toEqual({
      status: "success",
      body: {
        id: mail.mail_id,
        html: "<p>Read-only sessions may read this.</p>",
        attachments: [{ size: 12, content: { data: ATTACHMENT_ID }, filename: "a.txt", contentType: "text/plain" }],
        messageId: "<ro-root@remote.example>",
        insight: null,
      },
    });
    await untouched();
  });

  test("HEAD /body and repeated reads leave it unread as well", async () => {
    expect((await readOnly.head(`/api/mails/body/${mail.mail_id}`)).status).toBe(200);
    await readOnly.get(`/api/mails/body/${mail.mail_id}`);
    await readOnly.get(`/api/mails/body/${mail.mail_id.toUpperCase()}`);
    await untouched();
    expect((await readOnly.get("/api/mails/accounts")).body.body.received.find((a: { key: string }) => a.key === "hello@mail.test").unread_doc_count).toBeGreaterThanOrEqual(1);
  });

  test("the same read by admin proper does mark it", async () => {
    const other = await admin.mail({ to: [addr("second@mail.test")] });
    await readOnly.get(`/api/mails/body/${other.mail_id}`);
    expect((await readMail(server, other.mail_id))!.read).toBe(false);
    await admin.client.get(`/api/mails/body/${other.mail_id}`);
    expect((await readMail(server, other.mail_id))!.read).toBe(true);
  });
});

describe("read-only: every write is refused with its own message", () => {
  test("POST /mark", async () => {
    const { status, text, headers } = await readOnly.post("/api/mails/mark", { mail_id: mail.mail_id, read: true, save: true });
    expect(status).toBe(200);
    expect(text).toBe('{"status":"failed","message":"Marking mail not permitted for read-only user (admin-ro)."}');
    expect(headers.get("content-type")).toBe(JSON_TYPE);
    await untouched();
    expect(await server.db.query("SELECT 1 FROM mail_mailbox_uid WHERE mail_id = $1 AND mailbox = 'Starred'", [mail.mail_id])).toEqual([]);
  });

  test("POST /send", async () => {
    const before = await countMails(server, admin.id);
    const { status, text } = await readOnly.post("/api/mails/send", { sender: "hello", to: "x@remote.example", subject: "s", html: "<p>h</p>" });
    expect(status).toBe(200);
    expect(text).toBe('{"status":"failed","message":"Sending mail not permitted for read-only user (admin-ro)."}');
    expect((await readOnly.post("/api/mails/send", { sender: "hello", to: "hello@mail.test" })).body).toEqual(SENDING);
    expect(server.mailgun.requests).toEqual([]);
    expect(await countMails(server, admin.id)).toBe(before);
  });

  test("DELETE /:id", async () => {
    const { status, text } = await readOnly.delete(`/api/mails/${mail.mail_id}`);
    expect(status).toBe(200);
    expect(text).toBe('{"status":"failed","message":"Deleting mail not permitted for read-only user (admin-ro)."}');
    await untouched();
  });

  test("POST /spam/mark", async () => {
    const { status, text } = await readOnly.post("/api/mails/spam/mark", { mail_id: mail.mail_id, is_spam: true });
    expect(status).toBe(200);
    expect(text).toBe('{"status":"failed","message":"Marking spam not permitted for read-only user (admin-ro)."}');
    await untouched();
    expect(await server.db.query("SELECT 1 FROM spam_training WHERE user_id = $1", [admin.id])).toEqual([]);
  });

  test("POST /spam-allowlist", async () => {
    const { status, text } = await readOnly.post("/api/mails/spam-allowlist", { pattern: "planted@example.com" });
    expect(status).toBe(200);
    expect(text).toBe('{"status":"failed","message":"Adding to the spam allowlist not permitted for read-only user (admin-ro)."}');
    expect((await admin.client.get("/api/mails/spam-allowlist")).body.body.map((e: { pattern: string }) => e.pattern)).toEqual(["trusted@example.com"]);
  });

  test("DELETE /spam-allowlist/:pattern", async () => {
    const { status, text } = await readOnly.delete("/api/mails/spam-allowlist/trusted@example.com");
    expect(status).toBe(200);
    expect(text).toBe('{"status":"failed","message":"Removing from the spam allowlist not permitted for read-only user (admin-ro)."}');
    expect((await admin.client.get("/api/mails/spam-allowlist")).body.body).toHaveLength(1);
  });

  test("the refusal comes before any validation: a bad request gets the same answer", async () => {
    expect((await readOnly.post("/api/mails/mark", {})).body).toEqual(MARKING);
    expect((await readOnly.post("/api/mails/mark", { mail_id: "abc" })).body).toEqual(MARKING);
    expect((await readOnly.post("/api/mails/mark", { mail_id: missingId(), read: true })).body).toEqual(MARKING);
    expect((await readOnly.request("POST", "/api/mails/mark", { body: "[]", headers: { "Content-Type": "application/json" } })).body).toEqual(MARKING);
    expect((await readOnly.post("/api/mails/send", {})).body).toEqual(SENDING);
    expect((await readOnly.post("/api/mails/send", { sender: "a b", to: "nope" })).body).toEqual(SENDING);
    expect((await readOnly.post("/api/mails/send", { sender: "hello", to: "x@remote.example", inReplyTo: 5 })).body).toEqual(SENDING);
    expect((await readOnly.delete("/api/mails/abc")).body).toEqual(DELETING);
    expect((await readOnly.delete(`/api/mails/${missingId()}`)).body).toEqual(DELETING);
    expect((await readOnly.delete("/api/mails/accounts")).body).toEqual(DELETING);
    expect((await readOnly.post("/api/mails/spam/mark", {})).body).toEqual(MARKING_SPAM);
    expect((await readOnly.post("/api/mails/spam/mark", { mail_id: "abc", is_spam: "yes" })).body).toEqual(MARKING_SPAM);
    expect((await readOnly.post("/api/mails/spam-allowlist", {})).body).toEqual(ADDING);
    expect((await readOnly.post("/api/mails/spam-allowlist", { pattern: "not a pattern" })).body).toEqual(ADDING);
    expect((await readOnly.delete("/api/mails/spam-allowlist/no-such-entry@example.com")).body).toEqual(REMOVING);
    await untouched();
  });

  test("a body the parser rejects is still a 400, before the route is reached", async () => {
    const { status, body } = await readOnly.request("POST", "/api/mails/mark", { body: "{", headers: { "Content-Type": "application/json" } });
    expect(status).toBe(400);
    expect(body).toEqual(BAD_REQUEST);
  });

  test("a multipart send is refused, with attachments, with too many of them, in any spelling of the path", async () => {
    const text = fields({ sender: "hello", to: "x@remote.example", subject: "s", html: "<p>h</p>" });
    const file = (i: number) => ({ name: "attachments", filename: `f${i}.txt`, contentType: "text/plain", value: "x" });
    for (const parts of [text, [...text, file(0)], [...text, ...Array.from({ length: 25 }, (_, i) => file(i))]]) {
      const form = multipart(parts);
      for (const path of ["/api/mails/send", "/api/mails/SEND/"]) {
        const { status, body } = await readOnly.request("POST", path, { body: form.body, headers: form.headers });
        expect({ parts: parts.length, path, status, body }).toEqual({ parts: parts.length, path, status: 200, body: SENDING });
      }
    }
    expect(server.mailgun.requests).toEqual([]);
  });

  // NOTE: the upload is parsed before the read-only check, so a read-only
  // session's files are written to temporary storage and judged for size
  // before the session is told it may not send at all.
  test("an oversized or malformed upload is judged before the read-only check", async () => {
    const big = multipart([{ name: "sender", value: "hello" }, { name: "attachments", filename: "big.bin", contentType: "application/octet-stream", value: new Uint8Array(25 * 1024 * 1024) }]);
    const oversize = await readOnly.request("POST", "/api/mails/send", { body: big.body, headers: big.headers });
    expect(oversize.status).toBe(413);
    expect(oversize.body).toEqual(failed("Each attachment must be smaller than 25MB"));
    const malformed = await readOnly.request("POST", "/api/mails/send", { body: "--abc\r\nbroken", headers: { "Content-Type": "multipart/form-data; boundary=abc" } });
    expect(malformed.status).toBe(400);
    expect(malformed.body).toEqual(BAD_REQUEST);
  }, 120_000);
});

describe("read-only: admin proper is unaffected", () => {
  test("admin can still write on the same server, and the read-only session sees the result", async () => {
    const target = await admin.mail({ to: [addr("writes@mail.test")], subject: "Writable" });
    expect((await admin.client.post("/api/mails/mark", { mail_id: target.mail_id, read: true, save: true })).body).toEqual({ status: "success" });
    const seen = (await readOnly.get("/api/mails/headers/writes@mail.test")).body.body;
    expect(seen).toEqual([headerOf(target, { read: true, saved: true })]);
    expect((await admin.client.post("/api/mails/spam/mark", { mail_id: target.mail_id, is_spam: true })).body).toEqual({ status: "success" });
    expect((await readOnly.get("/api/mails/headers/writes@mail.test?spam=1")).body.body).toHaveLength(1);
    expect((await admin.client.delete(`/api/mails/${target.mail_id}`)).body).toEqual({ status: "success" });
    expect((await readOnly.get("/api/mails/headers/writes@mail.test?spam=1")).body.body).toEqual([]);
  });

  test("a regular user's session is not read-only", async () => {
    const user = await newMailbox(server);
    const theirs = await user.mail();
    expect((await user.client.post("/api/mails/mark", { mail_id: theirs.mail_id, read: true })).body).toEqual({ status: "success" });
    expect((await user.client.post("/api/mails/spam-allowlist", { pattern: "a@example.com" })).body.status).toBe("success");
  });
});
