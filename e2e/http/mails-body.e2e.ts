import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { missingId, readMail, startServer, type TestServer } from "../harness";
import { BODY_ETAG, JSON_TYPE, adminMailbox, failed, newMailbox, type Mailbox, type SeedRow } from "./mails-helpers";

let server: TestServer;
let box: Mailbox;

beforeAll(async () => {
  server = await startServer();
  box = await newMailbox(server);
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

const NOT_FOUND = failed("No email is found.");

const bodyOf = async (row: SeedRow, owner = box) => {
  const mail = await owner.mail(row);
  const { status, body } = await owner.client.get(`/api/mails/body/${mail.mail_id}`);
  expect(status).toBe(200);
  expect(body.status).toBe("success");
  return { mail, body: body.body as Record<string, any> };
};

describe("GET /api/mails/body/:id — the response", () => {
  test("an HTML mail, byte for byte", async () => {
    const mail = await box.mail({ html: "<p>Hello</p>", text: "Hello", message_id: "<b1@remote.example>" });
    const { status, text, headers } = await box.client.get(`/api/mails/body/${mail.mail_id}`);
    expect(status).toBe(200);
    expect(text).toBe(
      `{"status":"success","body":{"id":"${mail.mail_id}","html":"<p>Hello</p>","messageId":"<b1@remote.example>","insight":null}}`
    );
    expect(headers.get("content-type")).toBe(JSON_TYPE);
    expect(headers.get("etag")).toMatch(BODY_ETAG);
    expect(headers.get("content-length")).toBe(String(Buffer.byteLength(text)));
  });

  test("with attachments and insight, byte for byte", async () => {
    const mail = await box.mail({
      html: "<p>x</p>",
      message_id: "<b2@remote.example>",
      attachments: [{ content: { data: "11111111-1111-4111-8111-111111111111" }, contentType: "text/plain", filename: "a.txt", size: 3 }],
      insight: { summary: ["s"], action_items: [], suggested_reply: "" },
    });
    const { text } = await box.client.get(`/api/mails/body/${mail.mail_id}`);
    expect(text).toBe(
      `{"status":"success","body":{"id":"${mail.mail_id}","html":"<p>x</p>",` +
        `"attachments":[{"size":3,"content":{"data":"11111111-1111-4111-8111-111111111111"},"filename":"a.txt","contentType":"text/plain"}],` +
        `"messageId":"<b2@remote.example>","insight":{"summary":["s"],"action_items":[],"suggested_reply":""}}}`
    );
  });

  test("carries no header fields: no subject, addresses, date or flags", async () => {
    const { body } = await bodyOf({ subject: "Subject", read: false });
    expect(Object.keys(body)).toEqual(["id", "html", "messageId", "insight"]);
  });
});

describe("GET /api/mails/body/:id — html", () => {
  test("stored HTML is returned untouched, scripts and all", async () => {
    const html = '<div onclick="x()">Ünï 😀 日本語 <script>alert(1)</script> &amp; <img src="cid:1"></div>\n';
    expect((await bodyOf({ html, text: "something else" })).body.html).toBe(html);
  });

  test("HTML with no text at all is returned untouched", async () => {
    expect((await bodyOf({ html: "<p>only html</p>", text: "" })).body.html).toBe("<p>only html</p>");
  });

  test("an empty mail has an empty html string", async () => {
    expect((await bodyOf({ html: "", text: "" })).body.html).toBe("");
  });

  test("a plain-text mail (html equal to the text) is wrapped in a pre-wrap block", async () => {
    expect((await bodyOf({ html: "Hello\nWorld", text: "Hello\nWorld" })).body.html).toBe(
      '<div style="white-space: pre-wrap">Hello\nWorld</div>'
    );
  });

  test("a mail with text but no html is converted the same way", async () => {
    expect((await bodyOf({ html: "", text: "Just text" })).body.html).toBe(
      '<div style="white-space: pre-wrap">Just text</div>'
    );
  });

  test("the conversion escapes markup and quotes", async () => {
    const text = `a < b > c & d "e" 'f' &amp; <script>x</script>`;
    expect((await bodyOf({ html: text, text })).body.html).toBe(
      `<div style="white-space: pre-wrap">a &lt; b &gt; c &amp; d &quot;e&quot; 'f' &amp;amp; &lt;script&gt;x&lt;/script&gt;</div>`
    );
  });

  test("the conversion links http and https URLs and leaves trailing punctuation out of the link", async () => {
    const text = "See https://example.com/a?b=1&c=2. Or (http://x.example/y), ftp://no.example www.no.example";
    expect((await bodyOf({ html: "", text })).body.html).toBe(
      '<div style="white-space: pre-wrap">See <a href="https://example.com/a?b=1&amp;c=2">https://example.com/a?b=1&amp;c=2</a>. ' +
        'Or (<a href="http://x.example/y">http://x.example/y</a>), ftp://no.example www.no.example</div>'
    );
  });

  test("the conversion nests quoted lines in blockquotes by depth", async () => {
    const text = "Line 1\r\nLine 2\n> quoted\n>> deeper\n> > also deeper\n> back\nplain\n>tight\n";
    expect((await bodyOf({ html: "", text })).body.html).toBe(
      '<div style="white-space: pre-wrap">Line 1\nLine 2<blockquote>quoted<blockquote>deeper\nalso deeper</blockquote>back</blockquote>' +
        "plain<blockquote>tight</blockquote></div>"
    );
  });

  test("the conversion keeps blank lines and runs of spaces", async () => {
    const text = "a\n\n\n  b   c\n";
    expect((await bodyOf({ html: text, text })).body.html).toBe('<div style="white-space: pre-wrap">a\n\n\n  b   c\n</div>');
  });

  test("a body of 300000 characters comes back whole", async () => {
    const html = `<p>${"x".repeat(300_000)}</p>`;
    expect((await bodyOf({ html, text: "x" })).body.html).toBe(html);
  });
});

describe("GET /api/mails/body/:id — attachments, insight, messageId", () => {
  test("attachments are left out when NULL, kept when an empty list, and passed through as stored", async () => {
    expect("attachments" in (await bodyOf({ attachments: null })).body).toBe(false);
    expect((await bodyOf({ attachments: [] })).body.attachments).toEqual([]);
    const stored = [
      { content: { data: "22222222-2222-4222-8222-222222222222" }, contentType: "image/png", filename: "ünï 😀.png", size: 12345 },
      { filename: "no-content.bin" },
      { content: { data: "x" }, contentType: "text/plain", filename: "a.txt", size: 0, extra: true },
    ];
    expect((await bodyOf({ attachments: stored })).body.attachments).toEqual(stored);
  });

  test("attachments stored as an object rather than a list are left out", async () => {
    expect("attachments" in (await bodyOf({ attachments: { a: 1 } })).body).toBe(false);
  });

  test("insight is null when absent and otherwise as stored", async () => {
    expect((await bodyOf({})).body.insight).toBeNull();
    const insight = { summary: ["a", "b"], action_items: ["do"], suggested_reply: "ok" };
    expect((await bodyOf({ insight })).body.insight).toEqual(insight);
    expect((await bodyOf({ insight: {} })).body.insight).toEqual({});
  });

  test("an empty stored Message-ID is replaced by eight random hex characters", async () => {
    const mail = await box.mail({ message_id: "" });
    const first = (await box.client.get(`/api/mails/body/${mail.mail_id}`)).body.body.messageId;
    const second = (await box.client.get(`/api/mails/body/${mail.mail_id}`)).body.body.messageId;
    expect(first).toMatch(/^[0-9a-f]{8}$/);
    expect(second).toMatch(/^[0-9a-f]{8}$/);
    expect(second).not.toBe(first);
  });
});

describe("GET /api/mails/body/:id — marks the mail read", () => {
  test("sets read, stamps the next mod-sequence and bumps updated", async () => {
    const fresh = await newMailbox(server);
    const mail = await fresh.mail({ modseq: 7, updated: "2026-01-01T00:00:00Z" });
    const before = Date.now();
    await fresh.client.get(`/api/mails/body/${mail.mail_id}`);
    const row = await readMail(server, mail.mail_id);
    expect(row!.read).toBe(true);
    expect(String(row!.modseq)).toBe("8");
    expect(+row!.updated).toBeGreaterThanOrEqual(before - 1000);
    // Nothing else moves.
    expect({ saved: row!.saved, sent: row!.sent, is_spam: row!.is_spam, deleted: row!.deleted, answered: row!.answered }).toEqual({
      saved: false,
      sent: false,
      is_spam: false,
      deleted: false,
      answered: false,
    });
    expect((await fresh.client.get("/api/mails/accounts")).body.body.received[0].unread_doc_count).toBe(0);
  });

  test("a second read, or a read of an already-read mail, writes nothing", async () => {
    const fresh = await newMailbox(server);
    const mail = await fresh.mail({ modseq: 7 });
    await fresh.client.get(`/api/mails/body/${mail.mail_id}`);
    const first = await readMail(server, mail.mail_id);
    await fresh.client.get(`/api/mails/body/${mail.mail_id}`);
    const second = await readMail(server, mail.mail_id);
    expect(String(second!.modseq)).toBe(String(first!.modseq));
    expect(+second!.updated).toBe(+first!.updated);

    const already = await fresh.mail({ read: true, modseq: 3, updated: "2026-01-01T00:00:00Z" });
    await fresh.client.get(`/api/mails/body/${already.mail_id}`);
    const row = await readMail(server, already.mail_id);
    expect(String(row!.modseq)).toBe("3");
    expect(new Date(row!.updated).toISOString()).toBe("2026-01-01T00:00:00.000Z");
  });

  test("HEAD marks the mail read too", async () => {
    const mail = await box.mail();
    const head = await box.client.head(`/api/mails/body/${mail.mail_id}`);
    expect(head.status).toBe(200);
    expect(head.text).toBe("");
    expect((await readMail(server, mail.mail_id))!.read).toBe(true);
  });

  test("a revalidation answered 304 still marks the mail read", async () => {
    const mail = await box.mail({ read: true });
    const etag = (await box.client.get(`/api/mails/body/${mail.mail_id}`)).headers.get("etag")!;
    await server.db.query("UPDATE mails SET read = FALSE WHERE mail_id = $1", [mail.mail_id]);
    const again = await box.client.get(`/api/mails/body/${mail.mail_id}`, { headers: { "If-None-Match": etag } });
    expect(again.status).toBe(304);
    expect((await readMail(server, mail.mail_id))!.read).toBe(true);
  });

  test("a refused read marks nothing", async () => {
    const other = await newMailbox(server);
    const mail = await other.mail();
    expect((await box.client.get(`/api/mails/body/${mail.mail_id}`)).body).toEqual(NOT_FOUND);
    expect((await readMail(server, mail.mail_id))!.read).toBe(false);
  });
});

describe("GET /api/mails/body/:id — which mails", () => {
  test("refuses an id that is not a UUID, a missing id and another user's mail with one message", async () => {
    const other = await newMailbox(server);
    const theirs = await other.mail();
    for (const id of [
      "abc",
      "123",
      "undefined",
      "null",
      missingId(),
      theirs.mail_id,
      `${theirs.mail_id}x`,
      `{${theirs.mail_id}}`,
      theirs.mail_id.replace(/-/g, ""),
      "%20",
      "x".repeat(4000),
    ]) {
      const { status, body } = await box.client.get(`/api/mails/body/${id}`);
      expect({ id: id.slice(0, 40), status, body }).toEqual({ id: id.slice(0, 40), status: 200, body: NOT_FOUND });
    }
  });

  test("the refusal is exact", async () => {
    const { status, text, headers } = await box.client.get("/api/mails/body/abc");
    expect(status).toBe(200);
    expect(text).toBe('{"status":"failed","message":"No email is found."}');
    expect(headers.get("etag")).toBe('W/"32-U3eJ28oJwjfICmuyrSj2Qdt+wcs"');
  });

  test("admin cannot read a user's mail by id", async () => {
    const admin = await adminMailbox(server);
    const mail = await box.mail();
    expect((await admin.client.get(`/api/mails/body/${mail.mail_id}`)).body).toEqual(NOT_FOUND);
    expect((await readMail(server, mail.mail_id))!.read).toBe(false);
  });

  test("accepts the id in upper case", async () => {
    const mail = await box.mail();
    const { body } = await box.client.get(`/api/mails/body/${mail.mail_id.toUpperCase()}`);
    expect(body.body.id).toBe(mail.mail_id);
  });

  test("serves sent mail, spam and drafts by id", async () => {
    for (const row of [{ sent: true }, { is_spam: true }, { draft: true }, { deleted: true }] as SeedRow[]) {
      const { mail, body } = await bodyOf(row);
      expect(body.id).toBe(mail.mail_id);
      expect((await readMail(server, mail.mail_id))!.read).toBe(true);
    }
  });

  test("refuses an expunged mail like a missing one, and marks nothing", async () => {
    const mail = await box.mail({ expunged: true, html: "<p>gone</p>", modseq: 4, updated: "2026-01-01T00:00:00Z" });
    const { status, text } = await box.client.get(`/api/mails/body/${mail.mail_id}`);
    expect(status).toBe(200);
    expect(text).toBe('{"status":"failed","message":"No email is found."}');
    expect((await box.client.head(`/api/mails/body/${mail.mail_id}`)).status).toBe(200);
    const row = await readMail(server, mail.mail_id);
    expect({ read: row!.read, modseq: String(row!.modseq), updated: new Date(row!.updated).toISOString() }).toEqual({
      read: false,
      modseq: "4",
      updated: "2026-01-01T00:00:00.000Z",
    });
  });

  test("an expunged mail stays refused whatever its other flags", async () => {
    for (const row of [{ sent: true }, { is_spam: true }, { draft: true }, { deleted: true }, { saved: true, read: true }] as SeedRow[]) {
      const mail = await box.mail({ ...row, expunged: true });
      expect({ row, body: (await box.client.get(`/api/mails/body/${mail.mail_id}`)).body }).toEqual({ row, body: NOT_FOUND });
    }
  });

  test("ignores the query string", async () => {
    const mail = await box.mail();
    const { body } = await box.client.get(`/api/mails/body/${mail.mail_id}?since=x&limit=0`);
    expect(body.body.id).toBe(mail.mail_id);
  });
});
