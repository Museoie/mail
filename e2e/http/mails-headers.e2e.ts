import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { addr, startServer, type InsertedMail, type TestServer } from "../harness";
import {
  BODY_ETAG,
  JSON_TYPE,
  adminMailbox,
  failed,
  headerOf,
  newMailbox,
  subjects,
  type Mailbox,
  type SeedRow,
} from "./mails-helpers";

let server: TestServer;

beforeAll(async () => {
  server = await startServer();
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

const NOT_YOURS = failed("Invalid request. You may not look at other users' emails.");

const list = async (box: Mailbox, query = "", account = box.address()) => {
  const { status, body } = await box.client.get(`/api/mails/headers/${encodeURIComponent(account)}${query}`);
  expect(status).toBe(200);
  expect(body.status).toBe("success");
  return body.body as Record<string, any>[];
};

/** Inserts one mail and returns the single row the default list then holds for it. */
const listed = async (box: Mailbox, row: SeedRow): Promise<[Record<string, any>, InsertedMail]> => {
  const mail = await box.mail(row);
  const rows = (await list(box)).filter((header) => header.id === mail.mail_id);
  expect(rows).toHaveLength(1);
  return [rows[0]!, mail];
};

describe("GET /api/mails/headers/:account — the response", () => {
  test("is an empty list for an account with no mail, with an exact body", async () => {
    const box = await newMailbox(server);
    const { status, text, headers } = await box.client.get(`/api/mails/headers/${box.address()}`);
    expect(status).toBe(200);
    expect(text).toBe('{"status":"success","body":[]}');
    expect(headers.get("content-type")).toBe(JSON_TYPE);
    expect(headers.get("etag")).toBe('W/"1e-ZirBHndQgB0ZQPCHGvcJtDrJf6U"');
  });

  test("one mail, byte for byte: key order and which fields are present", async () => {
    const box = await newMailbox(server);
    const me = box.address();
    const mail = await box.mail({
      to: [addr(me, "Box")],
      subject: "Hi",
      text: "Body text",
      date: "2026-01-02T03:04:05.678Z",
      message_id: "<fixed@remote.example>",
    });
    const { text, headers } = await box.client.get(`/api/mails/headers/${me}`);
    expect(text).toBe(
      `{"status":"success","body":[{"id":"${mail.mail_id}","subject":"Hi","date":"2026-01-02T03:04:05.678Z",` +
        `"from":{"value":[{"name":"Remote Sender","address":"sender@remote.example"}],"text":"Remote Sender <sender@remote.example>"},` +
        `"to":{"value":[{"name":"Box","address":"${me}"}],"text":"Box <${me}>"},` +
        `"envelopeTo":{"value":[{"address":"${me}"}],"text":""},` +
        `"read":false,"saved":false,"sent":false,"insight":null,"snippet":"Body text","messageId":"<fixed@remote.example>"}]}`
    );
    expect(headers.get("etag")).toMatch(BODY_ETAG);
  });

  test("every optional field present, byte for byte", async () => {
    const box = await newMailbox(server);
    const me = box.address();
    const mail = await box.mail({
      from: [addr(me, "Me")],
      to: [addr(me)],
      cc: [addr("cc@remote.example", "Cc")],
      bcc: [addr("bcc@remote.example")],
      subject: "Full",
      text: "t",
      date: "2026-01-02T03:04:05Z",
      message_id: "<m@x>",
      in_reply_to: "<p@x>",
      reference_ids: ["<r@x>", "<p@x>"],
      read: true,
      saved: true,
      sent: true,
      insight: { summary: ["s"], action_items: ["a"], suggested_reply: "r" },
    });
    const { text } = await box.client.get(`/api/mails/headers/${me}`);
    expect(text).toBe(
      `{"status":"success","body":[{"id":"${mail.mail_id}","subject":"Full","date":"2026-01-02T03:04:05.000Z",` +
        `"from":{"value":[{"name":"Me","address":"${me}"}],"text":"Me <${me}>"},` +
        `"to":{"value":[{"address":"${me}"}],"text":"${me}"},` +
        `"cc":{"value":[{"name":"Cc","address":"cc@remote.example"}],"text":"Cc <cc@remote.example>"},` +
        `"bcc":{"value":[{"address":"bcc@remote.example"}],"text":"bcc@remote.example"},` +
        `"envelopeTo":{"value":[{"address":"${me}"}],"text":""},` +
        `"read":true,"saved":true,"sent":true,` +
        `"insight":{"summary":["s"],"action_items":["a"],"suggested_reply":"r"},"snippet":"t",` +
        `"messageId":"<m@x>","inReplyTo":"<p@x>","references":["<r@x>","<p@x>"]}]}`
    );
  });
});

describe("GET /api/mails/headers/:account — stored shapes", () => {
  let box: Mailbox;
  beforeAll(async () => {
    box = await newMailbox(server);
  });

  test("a plain received mail", async () => {
    const [header, mail] = await listed(box, {});
    expect(header).toEqual(headerOf(mail));
    expect(Object.keys(header)).toEqual([
      "id", "subject", "date", "from", "to", "envelopeTo", "read", "saved", "sent", "insight", "snippet", "messageId",
    ]);
  });

  test("names, Cc and Bcc", async () => {
    const [header, mail] = await listed(box, {
      to: [addr(box.address(), "Me Myself")],
      cc: [addr("cc@remote.example", "CC"), addr("cc2@remote.example")],
      bcc: [addr("bcc@remote.example")],
    });
    expect(header).toEqual(headerOf(mail));
    expect(header.cc).toEqual({
      value: [{ name: "CC", address: "cc@remote.example" }, { address: "cc2@remote.example" }],
      text: "CC <cc@remote.example>, cc2@remote.example",
    });
    expect(header.bcc).toEqual({ value: [{ address: "bcc@remote.example" }], text: "bcc@remote.example" });
  });

  test("an address entry keeps a null name, an empty name and unknown keys as stored", async () => {
    const me = box.address();
    const [nullName] = await listed(box, { to_address: [{ address: me, name: null }], to_text: me });
    expect(nullName.to).toEqual({ value: [{ name: null, address: me }], text: me });
    const [emptyName] = await listed(box, { to_address: [{ address: me, name: "" }], to_text: me });
    expect(emptyName.to).toEqual({ value: [{ name: "", address: me }], text: me });
    const [extra] = await listed(box, {
      to_address: [{ address: me, name: "X", group: [1], extra: { a: 1 } }],
      to_text: "X",
    });
    expect(extra.to).toEqual({ value: [{ name: "X", extra: { a: 1 }, group: [1], address: me }], text: "X" });
  });

  test("a NULL address text becomes an empty string", async () => {
    const me = box.address();
    const [header] = await listed(box, {
      to_address: [{ address: me }],
      to_text: null,
      from_address: [{ address: "s@remote.example" }],
      from_text: null,
      cc_address: [{ address: "c@remote.example" }],
      cc_text: null,
      bcc_address: [{ address: "b@remote.example" }],
      bcc_text: null,
    });
    expect(header.from).toEqual({ value: [{ address: "s@remote.example" }], text: "" });
    expect(header.to).toEqual({ value: [{ address: me }], text: "" });
    expect(header.cc).toEqual({ value: [{ address: "c@remote.example" }], text: "" });
    expect(header.bcc).toEqual({ value: [{ address: "b@remote.example" }], text: "" });
  });

  test("a NULL address column is left out; an empty list is kept", async () => {
    const [noFrom, noFromMail] = await listed(box, { from_address: null, from_text: null });
    expect("from" in noFrom).toBe(false);
    expect(noFrom).toEqual(headerOf(noFromMail));
    const [emptyFrom] = await listed(box, { from_address: [], from_text: "" });
    expect(emptyFrom.from).toEqual({ value: [], text: "" });
    const [emptyCc] = await listed(box, { cc_address: [], cc_text: "" });
    expect(emptyCc.cc).toEqual({ value: [], text: "" });
    expect("bcc" in emptyCc).toBe(false);
  });

  test("an envelope-only (Bcc) delivery is listed under the envelope address", async () => {
    const me = box.address();
    const [header, mail] = await listed(box, { to: [addr("list@remote.example")], envelope_to: [{ address: me }] });
    expect(header).toEqual(headerOf(mail));
    expect(header.to).toEqual({ value: [{ address: "list@remote.example" }], text: "list@remote.example" });
    expect(header.envelopeTo).toEqual({ value: [{ address: me }], text: "" });
    const [noTo] = await listed(box, { to_address: null, to_text: null, envelope_to: [{ address: me }] });
    expect("to" in noTo).toBe(false);
    const [emptyTo] = await listed(box, { to_address: [], to_text: "", envelope_to: [{ address: me }] });
    expect(emptyTo.to).toEqual({ value: [], text: "" });
  });

  test("envelopeTo names only the user's own envelope recipients, whatever the row stores", async () => {
    const me = box.address();
    const [header] = await listed(box, {
      to: [addr("list@remote.example")],
      envelope_to: [
        { address: "x@remote.example" },
        { address: me },
        { address: "y@someone-else.mail.test" },
        { address: "z@mail.test" },
        { address: box.address("second").toUpperCase() },
      ],
    });
    expect(header.envelopeTo).toEqual({ value: [{ address: me }, { address: box.address("second").toUpperCase() }], text: "" });

    const admin = await adminMailbox(server);
    const adminRow = await admin.mail({
      to: [addr("list@remote.example")],
      envelope_to: [{ address: "hello@mail.test" }, { address: me }, { address: "x@admin.mail.test" }],
    });
    const adminList = await list(admin, "", "hello@mail.test");
    expect(adminList.find((row) => row.id === adminRow.mail_id)!.envelopeTo).toEqual({
      value: [{ address: "hello@mail.test" }, { address: "x@admin.mail.test" }],
      text: "",
    });
  });

  test("no envelope recipient leaves envelopeTo out; an empty one is kept", async () => {
    const me = box.address();
    const [none, noneMail] = await listed(box, { to_address: [{ address: me }], to_text: me });
    expect("envelopeTo" in none).toBe(false);
    expect(none).toEqual(headerOf(noneMail));
    const [empty] = await listed(box, { to_address: [{ address: me }], to_text: me, envelope_to: [] });
    expect(empty.envelopeTo).toEqual({ value: [], text: "" });
  });

  test("list entries that are not address objects are passed through", async () => {
    const [header] = await listed(box, {
      to_address: ["a", 1, null],
      to_text: "x",
      envelope_to: [{ address: box.address() }],
    });
    expect(header.to).toEqual({ value: ["a", 1, null], text: "x" });
  });

  test("insight is null when absent and otherwise whatever JSON is stored", async () => {
    const [none] = await listed(box, {});
    expect(none.insight).toBeNull();
    const full = { summary: ["a", "b"], action_items: ["x"], suggested_reply: "ok" };
    expect((await listed(box, { insight: full }))[0].insight).toEqual(full);
    expect((await listed(box, { insight: { summary: ["a"] } }))[0].insight).toEqual({ summary: ["a"] });
    expect((await listed(box, { insight: {} }))[0].insight).toEqual({});
    expect((await listed(box, { insight: [1, 2] }))[0].insight).toEqual([1, 2]);
    expect((await listed(box, { insight: JSON.stringify("text") }))[0].insight).toBe("text");
  });

  test("flags are booleans as stored", async () => {
    const [header] = await listed(box, { read: true, saved: true, answered: true, deleted: true });
    expect({ read: header.read, saved: header.saved, sent: header.sent }).toEqual({
      read: true,
      saved: true,
      sent: false,
    });
    expect("answered" in header || "deleted" in header || "draft" in header || "is_spam" in header).toBe(false);
  });

  test("an empty subject is an empty string", async () => {
    const [header] = await listed(box, { subject: "" });
    expect(header.subject).toBe("");
  });

  test("unicode, emoji, markup and a very long subject come back unchanged", async () => {
    const subject = 'Ünïcödé 😀 日本語 "quote" <b>&amp; \\ \u0001 tab\t';
    const [header] = await listed(box, { subject, to: [addr(box.address(), "Ünï 😀")] });
    expect(header.subject).toBe(subject);
    expect(header.to.text).toBe(`Ünï 😀 <${box.address()}>`);
    const long = "long subject 😀 ".repeat(4000);
    const [longHeader] = await listed(box, { subject: long });
    expect(longHeader.subject).toBe(long);
  });

  test("an empty Message-ID or In-Reply-To is left out", async () => {
    const [noId] = await listed(box, { message_id: "" });
    expect("messageId" in noId).toBe(false);
    const [noReply] = await listed(box, { in_reply_to: "" });
    expect("inReplyTo" in noReply).toBe(false);
  });

  test("references: up to three as stored, more are cut to the first and the last two", async () => {
    const refs = (n: number) => Array.from({ length: n }, (_, i) => `<r${i + 1}@x>`);
    expect("references" in (await listed(box, { reference_ids: [] }))[0]).toBe(false);
    expect("references" in (await listed(box, { reference_ids: null }))[0]).toBe(false);
    expect((await listed(box, { reference_ids: refs(1) }))[0].references).toEqual(["<r1@x>"]);
    expect((await listed(box, { reference_ids: refs(3) }))[0].references).toEqual(["<r1@x>", "<r2@x>", "<r3@x>"]);
    expect((await listed(box, { reference_ids: refs(4) }))[0].references).toEqual(["<r1@x>", "<r3@x>", "<r4@x>"]);
    expect((await listed(box, { reference_ids: refs(40) }))[0].references).toEqual(["<r1@x>", "<r39@x>", "<r40@x>"]);
    const [both, mail] = await listed(box, { in_reply_to: "<a@x>", reference_ids: ["<r1@x>", "<a@x>"] });
    expect(both).toEqual(headerOf(mail));
    expect(Object.keys(both).slice(-3)).toEqual(["messageId", "inReplyTo", "references"]);
  });
});

describe("GET /api/mails/headers/:account — dates", () => {
  test("dates are UTC with milliseconds, microseconds cut off", async () => {
    const box = await newMailbox(server);
    const cases: [string, string][] = [
      ["2026-07-01 10:00:00+09", "2026-07-01T01:00:00.000Z"],
      ["2026-07-01 10:00:00-08:30", "2026-07-01T18:30:00.000Z"],
      ["2026-07-01 10:00:00.123999+00", "2026-07-01T10:00:00.123Z"],
      ["2026-07-01 10:00:00.000001+00", "2026-07-01T10:00:00.000Z"],
      ["1969-12-31 23:59:59.5+00", "1969-12-31T23:59:59.500Z"],
      ["9999-12-31 23:59:59+00", "9999-12-31T23:59:59.000Z"],
      ["2024-02-29 23:59:60+00", "2024-03-01T00:00:00.000Z"],
    ];
    for (const [stored, expected] of cases) {
      const [header] = await listed(box, { date: stored, subject: stored });
      expect({ stored, date: header.date }).toEqual({ stored, date: expected });
    }
  });
});

describe("GET /api/mails/headers/:account — snippets", () => {
  let box: Mailbox;
  beforeAll(async () => {
    box = await newMailbox(server);
  });
  const snippet = async (text: string) => (await listed(box, { text, html: "<p>x</p>" }))[0].snippet;

  test("is the text on one line with whitespace collapsed", async () => {
    expect(await snippet("  Hello\r\n\r\n   world\t and more  ")).toBe("Hello world and more");
  });

  test("is left out when the mail has no text", async () => {
    const [header] = await listed(box, { text: "", html: "<p>Only html</p>" });
    expect("snippet" in header).toBe(false);
    const [blank] = await listed(box, { text: " \n\t \n" });
    expect("snippet" in blank).toBe(false);
  });

  test("drops quoted lines and the line that introduces them", async () => {
    expect(await snippet("Thanks!\n\nOn Mon, Jan 1, 2026 at 1:00 PM Bob <b@x.com> wrote:\n> old stuff\n> more\n")).toBe("Thanks!");
    expect(await snippet("Merci\nLe 5 janv. 2021 à 15:00, Jean <jean@x.com> a écrit :\n> ancien")).toBe("Merci");
    expect(await snippet("  > indented quote\nkept\n>> nested\nalso kept")).toBe("kept also kept");
    expect(await snippet("On the other hand, he wrote: nothing\nOn second thought")).toBe(
      "On the other hand, he wrote: nothing On second thought"
    );
  });

  test("is left out when everything is quoted", async () => {
    const [header] = await listed(box, { text: "> a\n> b\nOn Monday someone wrote:\n" });
    expect("snippet" in header).toBe(false);
  });

  test("stops at a forwarded-message or original-message separator", async () => {
    expect(await snippet("See below\n---------- Forwarded message ---------\nFrom: a")).toBe("See below");
    expect(await snippet("Top\n-----Original Message-----\nFrom: a")).toBe("Top");
    expect(await snippet("Top\n-- original message --\nrest")).toBe("Top");
    expect(await snippet("Top\n________________________________\nFrom: a")).toBe("Top");
    expect(await snippet("Top\nBegin forwarded message:\nFrom: a")).toBe("Top");
    expect(await snippet("Top\n -- \nSignature")).toBe("Top -- Signature");
    expect(await snippet("Top\n_single underscore\nrest")).toBe("Top _single underscore rest");
  });

  test("is cut at the last space within 160 characters and ends with an ellipsis", async () => {
    expect(await snippet("word ".repeat(200))).toBe(`${"word ".repeat(32).trimEnd()}…`);
    expect(await snippet("x".repeat(500))).toBe(`${"x".repeat(160)}…`);
    expect(await snippet("y".repeat(160))).toBe("y".repeat(160));
    expect(await snippet(`${"z".repeat(159)} tail`)).toBe(`${"z".repeat(159)}…`);
    expect(await snippet(`${"z".repeat(160)} tail`)).toBe(`${"z".repeat(160)}…`);
  });

  test("is built from the first 400 characters of the text only", async () => {
    const quoted = "> quoted line\n".repeat(40);
    const [header] = await listed(box, { text: `${quoted}Late unquoted content` });
    expect("snippet" in header).toBe(false);
    expect(await snippet(`${"a".repeat(398)} b cut here`)).toBe(`${"a".repeat(160)}…`);
    expect(await snippet(`intro ${"\n".repeat(390)}late words that do not fit`)).toBe("intro late");
  });

  test("keeps markup and entities as plain characters", async () => {
    expect(await snippet("<b>bold</b> &amp; \"quotes\" 😀")).toBe('<b>bold</b> &amp; "quotes" 😀');
  });
});

describe("GET /api/mails/headers/:account — which mails, in which order", () => {
  test("newest first, and by id descending among equal dates", async () => {
    const box = await newMailbox(server);
    await box.mail({ subject: "old", date: "2026-01-01T00:00:00Z" });
    await box.mail({ subject: "new", date: "2026-01-03T00:00:00Z" });
    await box.mail({ subject: "tie-low", date: "2026-01-02T00:00:00Z", mail_id: "00000000-0000-4000-8000-000000000001" });
    await box.mail({ subject: "tie-high", date: "2026-01-02T00:00:00Z", mail_id: "ffffffff-ffff-4fff-8fff-fffffffffff1" });
    await box.mail({ subject: "tie-mid", date: "2026-01-02T00:00:00Z", mail_id: "80000000-0000-4000-8000-000000000001" });
    await box.mail({ subject: "micro-later", date: "2026-01-02 00:00:00.000001+00" });
    expect(subjects(await list(box))).toEqual(["new", "micro-later", "tie-high", "tie-mid", "tie-low", "old"]);
  });

  test("read state does not change the order", async () => {
    const box = await newMailbox(server);
    await box.mail({ subject: "unread-old", date: "2026-01-01T00:00:00Z" });
    await box.mail({ subject: "read-new", date: "2026-01-02T00:00:00Z", read: true });
    expect(subjects(await list(box))).toEqual(["read-new", "unread-old"]);
  });

  test("the default view matches To, Cc, Bcc and the envelope recipient, exactly", async () => {
    const box = await newMailbox(server);
    const me = box.address();
    await box.mail({ subject: "to", to: [addr("x@remote.example"), addr(me)], envelope_to: [] });
    await box.mail({ subject: "cc", to: [addr("x@remote.example")], cc: [addr(me)], envelope_to: [] });
    await box.mail({ subject: "bcc", to: [addr("x@remote.example")], bcc: [addr(me)], envelope_to: [] });
    await box.mail({ subject: "envelope", to: [addr("x@remote.example")], envelope_to: [{ address: me }] });
    await box.mail({ subject: "from only", from: [addr(me)], to: [addr("x@remote.example")], envelope_to: [] });
    await box.mail({ subject: "other account", to: [addr(box.address("other"))] });
    await box.mail({ subject: "upper case", to: [addr(me.toUpperCase())] });
    await box.mail({ subject: "prefix", to: [addr(`x${me}`)] });
    expect(subjects(await list(box)).sort()).toEqual(["bcc", "cc", "envelope", "to"]);
    expect(subjects(await list(box, "", box.address("other")))).toEqual(["other account"]);
    expect(subjects(await list(box, "", me.toUpperCase()))).toEqual(["upper case"]);
  });

  test("expunged mail, drafts and spam are not listed; deleted and answered mail is", async () => {
    const box = await newMailbox(server);
    await box.mail({ subject: "expunged", expunged: true });
    await box.mail({ subject: "draft", draft: true });
    await box.mail({ subject: "spam", is_spam: true });
    await box.mail({ subject: "deleted flag", deleted: true });
    await box.mail({ subject: "answered flag", answered: true });
    expect(subjects(await list(box)).sort()).toEqual(["answered flag", "deleted flag"]);
  });

  test("?sent lists mail by its From address, whatever the sent flag says", async () => {
    const box = await newMailbox(server);
    const me = box.address();
    await box.mail({ subject: "sent", from: [addr(me)], to: [addr("x@remote.example")], envelope_to: [], sent: true, date: "2026-01-03T00:00:00Z" });
    await box.mail({ subject: "from me, not flagged", from: [addr(me)], to: [addr("x@remote.example")], envelope_to: [], date: "2026-01-02T00:00:00Z" });
    await box.mail({ subject: "to self", from: [addr(me)], to: [addr(me)], sent: true, date: "2026-01-01T00:00:00Z" });
    await box.mail({ subject: "received" });
    await box.mail({ subject: "flagged sent, from another", from: [addr("x@remote.example")], sent: true });
    await box.mail({ subject: "sent spam", from: [addr(me)], to: [addr("x@remote.example")], envelope_to: [], sent: true, is_spam: true });
    await box.mail({ subject: "sent draft", from: [addr(me)], to: [addr("x@remote.example")], envelope_to: [], sent: true, draft: true });
    await box.mail({ subject: "sent expunged", from: [addr(me)], to: [addr("x@remote.example")], envelope_to: [], sent: true, expunged: true });
    expect(subjects(await list(box, "?sent=1"))).toEqual(["sent", "from me, not flagged", "to self"]);
    expect(subjects(await list(box)).sort()).toEqual(["flagged sent, from another", "received", "to self"]);
  });

  test("?saved lists saved mail the account sent or received", async () => {
    const box = await newMailbox(server);
    const me = box.address();
    await box.mail({ subject: "saved received", saved: true, date: "2026-01-03T00:00:00Z" });
    await box.mail({ subject: "saved sent", from: [addr(me)], to: [addr("x@remote.example")], envelope_to: [], sent: true, saved: true, date: "2026-01-02T00:00:00Z" });
    await box.mail({ subject: "not saved" });
    await box.mail({ subject: "saved, other account", to: [addr(box.address("other"))], saved: true });
    await box.mail({ subject: "saved spam", saved: true, is_spam: true });
    await box.mail({ subject: "saved draft", saved: true, draft: true });
    await box.mail({ subject: "saved expunged", saved: true, expunged: true });
    expect(subjects(await list(box, "?saved=1"))).toEqual(["saved received", "saved sent"]);
    // With ?sent as well, only the From side counts.
    expect(subjects(await list(box, "?saved=1&sent=1"))).toEqual(["saved sent"]);
  });

  test("?spam lists received spam only", async () => {
    const box = await newMailbox(server);
    const me = box.address();
    await box.mail({ subject: "spam", is_spam: true, date: "2026-01-03T00:00:00Z" });
    await box.mail({ subject: "spam by envelope", to: [addr("x@remote.example")], envelope_to: [{ address: me }], is_spam: true, date: "2026-01-02T00:00:00Z" });
    await box.mail({ subject: "ham" });
    await box.mail({ subject: "spam flagged sent", is_spam: true, sent: true });
    await box.mail({ subject: "spam from me", from: [addr(me)], to: [addr("x@remote.example")], envelope_to: [], is_spam: true });
    await box.mail({ subject: "spam draft", is_spam: true, draft: true });
    await box.mail({ subject: "spam expunged", is_spam: true, expunged: true });
    await box.mail({ subject: "saved spam", is_spam: true, saved: true, date: "2026-01-01T00:00:00Z" });
    expect(subjects(await list(box, "?spam=1"))).toEqual(["spam", "spam by envelope", "saved spam"]);
    expect(subjects(await list(box, "?spam=1&saved=1"))).toEqual(["saved spam"]);
    // ?sent&spam asks for From = account, is_spam and not flagged sent.
    expect(subjects(await list(box, "?spam=1&sent=1"))).toEqual(["spam from me"]);
    // A spam row carries no marker in a list row.
    expect("spam" in (await list(box, "?spam=1"))[0]!).toBe(false);
  });

  test("a view flag is on for any non-empty value and off when empty", async () => {
    const box = await newMailbox(server);
    const me = box.address();
    await box.mail({ subject: "received" });
    await box.mail({ subject: "sent", from: [addr(me)], to: [addr("x@remote.example")], envelope_to: [], sent: true });
    for (const query of ["?sent=1", "?sent=true", "?sent=false", "?sent=0", "?sent=no", "?sent=&sent=", "?sent=a&sent=b"]) {
      expect({ query, subjects: subjects(await list(box, query)) }).toEqual({ query, subjects: ["sent"] });
    }
    for (const query of ["", "?sent=", "?sent", "?SENT=1", "?new=1", "?unknown=1&x=2"]) {
      expect({ query, subjects: subjects(await list(box, query)) }).toEqual({ query, subjects: ["received"] });
    }
  });

  test("rows of another user are never listed, even under the same address", async () => {
    const first = await newMailbox(server);
    const second = await newMailbox(server);
    await second.mail({ subject: "stored for second", to: [addr(first.address())] });
    await first.mail({ subject: "mine" });
    expect(subjects(await list(first))).toEqual(["mine"]);
  });

  test("the full list holds at most 10000 rows, the newest", async () => {
    const box = await newMailbox(server);
    const me = box.address();
    await server.db.query(
      `INSERT INTO mails (user_id, message_id, subject, date, to_address, to_text, envelope_to, text)
       SELECT $1, '<bulk-' || g || '@x>', 'bulk ' || g, timestamptz '2026-01-01T00:00:00Z' + g * interval '1 second',
              $2::jsonb, $3, $2::jsonb, 'body ' || g
       FROM generate_series(1, 10001) g`,
      [box.id, [{ address: me }], me]
    );
    const rows = await list(box);
    expect(rows).toHaveLength(10000);
    expect(rows[0]!.subject).toBe("bulk 10001");
    expect(rows[9999]!.subject).toBe("bulk 2");
    expect(rows[5000]).toEqual({
      id: rows[5000]!.id,
      subject: "bulk 5001",
      date: "2026-01-01T01:23:21.000Z",
      to: { value: [{ address: me }], text: me },
      envelopeTo: { value: [{ address: me }], text: "" },
      read: false,
      saved: false,
      sent: false,
      insight: null,
      snippet: "body 5001",
      messageId: "<bulk-5001@x>",
    });
  }, 120_000);
});

describe("GET /api/mails/headers/:account — whose account", () => {
  let box: Mailbox;
  beforeAll(async () => {
    box = await newMailbox(server);
    await box.mail({ subject: "mine" });
  });

  test("a user may name any address on their own domain", async () => {
    const d = box.domain;
    for (const account of [
      box.address(),
      box.address("anything"),
      `@${d}`,
      d,
      box.username,
      `X@${d.toUpperCase()}`,
      `a@b@${d}`,
      `Name <${box.address()}>`,
      `${box.address()} `,
    ]) {
      const { status, body } = await box.client.get(`/api/mails/headers/${encodeURIComponent(account)}`);
      expect({ account, status, outcome: body.status }).toEqual({ account, status: 200, outcome: "success" });
    }
  });

  test("a user is refused any other account", async () => {
    const other = await newMailbox(server);
    for (const account of [
      other.address(),
      "hello@mail.test",
      "x@admin.mail.test",
      "admin",
      "mail.test",
      "sender@remote.example",
      `x@sub.${box.domain}`,
      `x@${box.username}mail.test`,
      " ",
      "@",
    ]) {
      const { status, body } = await box.client.get(`/api/mails/headers/${encodeURIComponent(account)}`);
      expect({ account, status, body }).toEqual({ account, status: 200, body: NOT_YOURS });
    }
  });

  test("the refusal is exact and comes before any query parameter is read", async () => {
    const { status, text } = await box.client.get("/api/mails/headers/hello@mail.test?since=garbage&limit=0&cursor=x");
    expect(status).toBe(200);
    expect(text).toBe('{"status":"failed","message":"Invalid request. You may not look at other users\' emails."}');
  });

  // NOTE: the owner of an account is read from the text before the first
  // ".mail.test" of the address's domain, so a look-alike domain that merely
  // starts with the user's own passes the check. No data leaks — the query is
  // still bound to the session's user — but the check is looser than it reads.
  test("a domain that only starts with the user's own passes the check", async () => {
    for (const account of [`x@${box.domain}.evil.example`, `x@${box.domain}x`]) {
      const { body } = await box.client.get(`/api/mails/headers/${encodeURIComponent(account)}`);
      expect({ account, body }).toEqual({ account, body: { status: "success", body: [] } });
    }
  });

  test("admin may name any account but only ever sees admin's own rows", async () => {
    const admin = await adminMailbox(server);
    const adminCopy = await admin.mail({ subject: "admin's copy", to: [addr(box.address())] });
    for (const account of [box.address(), "sender@remote.example", "x@nouser.mail.test", "admin", box.username, "x"]) {
      const { status, body } = await admin.client.get(`/api/mails/headers/${encodeURIComponent(account)}`);
      expect({ account, status, outcome: body.status }).toEqual({ account, status: 200, outcome: "success" });
    }
    const rows = (await admin.client.get(`/api/mails/headers/${box.address()}`)).body.body;
    // The row's envelope names only the other user's address, none of admin's.
    expect(rows).toEqual([headerOf(adminCopy, { envelopeTo: { value: [], text: "" } })]);
  });

  test("an account of 5000 characters is answered", async () => {
    const account = `${"a".repeat(5000)}@${box.domain}`;
    const { status, body } = await box.client.get(`/api/mails/headers/${account}`);
    expect(status).toBe(200);
    expect(body).toEqual({ status: "success", body: [] });
  });
});
