import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { addr, startServer, type TestServer } from "../harness";
import { BODY_ETAG, JSON_TYPE, adminMailbox, headerOf, newMailbox, subjects, type Mailbox } from "./mails-helpers";

let server: TestServer;

beforeAll(async () => {
  server = await startServer();
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

const search = async (box: Mailbox, value: string, query = "") => {
  const { status, body } = await box.client.get(`/api/mails/search/${encodeURIComponent(value)}${query}`);
  expect(status).toBe(200);
  expect(body.status).toBe("success");
  return body.body as Record<string, any>[];
};

const searchAccounts = async (box: Mailbox, value: string) => {
  const { status, body } = await box.client.get(`/api/mails/search-accounts/${encodeURIComponent(value)}`);
  expect(status).toBe(200);
  expect(body.status).toBe("success");
  return body.body as Record<string, any>[];
};

const EMPTY = '{"status":"success","body":[]}';

describe("GET /api/mails/search/:value — the response", () => {
  test("no match is an empty list with an exact body", async () => {
    const box = await newMailbox(server);
    const { status, text, headers } = await box.client.get("/api/mails/search/nothing");
    expect(status).toBe(200);
    expect(text).toBe(EMPTY);
    expect(headers.get("content-type")).toBe(JSON_TYPE);
    expect(headers.get("content-length")).toBe("30");
    expect(headers.get("etag")).toBe('W/"1e-ZirBHndQgB0ZQPCHGvcJtDrJf6U"');
  });

  test("one match, byte for byte: the row's key order and its highlight", async () => {
    const box = await newMailbox(server);
    const me = box.address();
    const mail = await box.mail({
      subject: "Quarterly invoice",
      text: "Please find the invoice attached.",
      date: "2026-01-02T03:04:05.678Z",
      message_id: "<s@x>",
      in_reply_to: "<p@x>",
      reference_ids: ["<p@x>"],
    });
    const { text, headers } = await box.client.get("/api/mails/search/invoice");
    expect(text).toBe(
      `{"status":"success","body":[{"id":"${mail.mail_id}","subject":"Quarterly invoice","date":"2026-01-02T03:04:05.678Z",` +
        `"from":{"value":[{"name":"Remote Sender","address":"sender@remote.example"}],"text":"Remote Sender <sender@remote.example>"},` +
        `"to":{"value":[{"address":"${me}"}],"text":"${me}"},` +
        `"read":false,"saved":false,"sent":false,"insight":null,` +
        `"envelopeTo":{"value":[{"address":"${me}"}],"text":""},` +
        `"highlight":{"subject":["Quarterly <em>invoice</em>"],"text":["Please find the <em>invoice</em> attached."]},` +
        `"snippet":"Please find the invoice attached."}]}`
    );
    expect(headers.get("etag")).toMatch(BODY_ETAG);
    expect(headers.get("content-length")).toBe(String(Buffer.byteLength(text)));
  });

  test("every optional field present: cc and bcc sit after insight, spam comes last, thread ids are absent", async () => {
    const box = await newMailbox(server);
    const mail = await box.mail({
      subject: "Kiwi",
      text: "kiwi",
      cc: [addr("c@remote.example", "C")],
      bcc: [addr("b@remote.example")],
      is_spam: true,
      read: true,
      saved: true,
      insight: { summary: ["s"] },
      in_reply_to: "<p@x>",
      reference_ids: ["<p@x>"],
    });
    const [row] = await search(box, "kiwi");
    expect(Object.keys(row!)).toEqual([
      "id", "subject", "date", "from", "to", "read", "saved", "sent", "insight", "cc", "bcc", "envelopeTo",
      "highlight", "snippet", "spam",
    ]);
    expect(row).toEqual({
      ...headerOf(mail, { messageId: undefined, inReplyTo: undefined, references: undefined }),
      highlight: { subject: ["<em>Kiwi</em>"], text: ["<em>kiwi</em>"] },
      spam: true,
    });
  });
});

describe("GET /api/mails/search/:value — what matches", () => {
  let box: Mailbox;
  beforeAll(async () => {
    box = await newMailbox(server);
    const me = box.address();
    await box.mail({ subject: "Quarterly invoice for Zebra", text: "Please find the invoice attached. The zebra crossing is closed.", date: "2026-01-01T00:00:00Z" });
    await box.mail({ subject: "Lunch plans", text: "Shall we meet for lunch on Tuesday? Running late again.", from: [addr("alice.wonder@partner.example", "Alice Wonder")], to: [addr(me, "Boxy McBox")], date: "2026-01-02T00:00:00Z" });
    await box.mail({ subject: "Addresses", text: "nothing special", to: [addr(me)], cc: [addr("carbon@copy.example", "Carbon Copy")], bcc: [addr("blind@copy.example", "Blind Copy")], envelope_to: [{ address: box.address("envelope") }], date: "2026-01-03T00:00:00Z" });
    await box.mail({ subject: "HTML only", text: "", html: "<p>Unique <b>platypus</b> content</p>", date: "2026-01-04T00:00:00Z" });
    await box.mail({ subject: "Ünïcödé 日本語 😀 café", text: "naïve café 日本語のテキスト 😀 emoji", date: "2026-01-05T00:00:00Z" });
    await box.mail({ subject: "Sent report", text: "Report body about C++ & <tags> and 100% coverage", from: [addr(me)], to: [addr("customer@client.example")], envelope_to: [], sent: true, read: true, date: "2026-01-06T00:00:00Z" });
    await box.mail({ subject: "<script>alert(1)</script> markup", text: "body with <em>markup</em> & ampersand", date: "2026-01-07T00:00:00Z" });
  });
  const hits = async (value: string) => subjects(await search(box, value) as { subject: string }[]);

  test("words of the subject and of the text, in any letter case", async () => {
    expect(await hits("invoice")).toEqual(["Quarterly invoice for Zebra"]);
    expect(await hits("ZEBRA")).toEqual(["Quarterly invoice for Zebra"]);
    expect(await hits("quarterly")).toEqual(["Quarterly invoice for Zebra"]);
    expect(await hits("crossing")).toEqual(["Quarterly invoice for Zebra"]);
    expect(await hits("tuesday")).toEqual(["Lunch plans"]);
  });

  test("English word forms: plurals and -ing reduce to one stem", async () => {
    expect(await hits("invoices")).toEqual(["Quarterly invoice for Zebra"]);
    expect(await hits("zebras")).toEqual(["Quarterly invoice for Zebra"]);
    expect(await hits("run")).toEqual(["Lunch plans"]);
    expect(await hits("planning")).toEqual(["Lunch plans"]);
    expect(await hits("zeb")).toEqual([]);
  });

  test("every word of the value must match", async () => {
    expect(await hits("invoice zebra")).toEqual(["Quarterly invoice for Zebra"]);
    expect(await hits("zebra   crossing  closed")).toEqual(["Quarterly invoice for Zebra"]);
    expect(await hits("zebra lunch")).toEqual([]);
  });

  test("sender and recipient names and whole addresses, but not parts of an address", async () => {
    expect(await hits("alice")).toEqual(["Lunch plans"]);
    expect(await hits("wonder")).toEqual(["Lunch plans"]);
    expect(await hits("boxy")).toEqual(["Lunch plans"]);
    expect(await hits("alice.wonder@partner.example")).toEqual(["Lunch plans"]);
    expect(await hits("customer@client.example")).toEqual(["Sent report"]);
    expect(await hits("partner")).toEqual([]);
    expect(await hits("partner.example")).toEqual([]);
    expect(await hits("customer")).toEqual([]);
  });

  test("not Cc, Bcc, the envelope recipient or the HTML body", async () => {
    for (const value of ["carbon", "blind", "copy", "carbon@copy.example", "envelope", "platypus", "unique"]) {
      expect({ value, hits: await hits(value) }).toEqual({ value, hits: [] });
    }
    expect(await hits("special")).toEqual(["Addresses"]);
  });

  test("the user's own address matches every mail that names it in From or To", async () => {
    expect((await hits(box.address())).sort()).toEqual([
      "<script>alert(1)</script> markup",
      "Addresses",
      "HTML only",
      "Lunch plans",
      "Quarterly invoice for Zebra",
      "Sent report",
      "Ünïcödé 日本語 😀 café",
    ].sort());
  });

  test("accented words match exactly as written; CJK runs match whole; emoji never match", async () => {
    expect(await hits("café")).toEqual(["Ünïcödé 日本語 😀 café"]);
    expect(await hits("CAFÉ")).toEqual(["Ünïcödé 日本語 😀 café"]);
    expect(await hits("naïve")).toEqual(["Ünïcödé 日本語 😀 café"]);
    expect(await hits("日本語")).toEqual(["Ünïcödé 日本語 😀 café"]);
    expect(await hits("cafe")).toEqual([]);
    expect(await hits("naive")).toEqual([]);
    expect(await hits("日本")).toEqual([]);
    expect(await hits("😀")).toEqual([]);
  });

  test("punctuation in the value is dropped: operators and quotes are just text", async () => {
    for (const value of ["!zebra", "zebra:*", "'zebra'", '"zebra"', "zebra;", "-zebra", "(zebra)", "zebra & invoice", "zebra, invoice."]) {
      expect({ value, hits: await hits(value) }).toEqual({ value, hits: ["Quarterly invoice for Zebra"] });
    }
    for (const value of ["zebra | lunch", "zebra OR lunch", "zebra -crossing lunch", "zebra) OR (1=1"]) {
      expect({ value, hits: await hits(value) }).toEqual({ value, hits: [] });
    }
    // A word in angle brackets reads as a markup tag, and tags are not searched.
    expect(await hits("<zebra>")).toEqual([]);
    expect(await hits("C++")).toEqual(["Sent report"]);
    expect(await hits("100%")).toEqual(["Sent report"]);
  });

  test("a value with no searchable word matches nothing", async () => {
    for (const value of ["the", "a", "and", "%", "_", "'", "\\", "*", "!!!", "; DROP TABLE mails; --", "x".repeat(3000)]) {
      const { status, text } = await box.client.get(`/api/mails/search/${encodeURIComponent(value)}`);
      expect({ value: value.slice(0, 30), status, text }).toEqual({ value: value.slice(0, 30), status: 200, text: EMPTY });
    }
  });

  test("the value is trimmed, and a blank value is an empty list", async () => {
    expect(await hits("  zebra  ")).toEqual(["Quarterly invoice for Zebra"]);
    expect(await hits("\tzebra\n")).toEqual(["Quarterly invoice for Zebra"]);
    for (const path of ["/api/mails/search/%20", "/api/mails/search/%20%09%0A"]) {
      expect((await box.client.get(path)).text).toBe(EMPTY);
    }
  });

  test("an encoded slash is part of the value, where it glues two words into one path-like token", async () => {
    expect(await hits("zebra/invoice")).toEqual([]);
    expect(await hits("zebra / invoice")).toEqual(["Quarterly invoice for Zebra"]);
  });

  // NOTE: a NUL byte in the value makes the database reject the query. The
  // error is swallowed and answered as "no results" rather than as a refusal.
  test("a value with a NUL byte is answered as no results", async () => {
    for (const path of ["/api/mails/search/%00", "/api/mails/search/zebra%00"]) {
      const { status, text } = await box.client.get(path);
      expect({ path, status, text }).toEqual({ path, status: 200, text: EMPTY });
    }
    expect(await hits("zebra")).toEqual(["Quarterly invoice for Zebra"]);
  });

  test("?field= changes nothing, whatever its value", async () => {
    const plain = await box.client.get("/api/mails/search/zebra");
    for (const query of ["?field=subject", "?field=text", "?field=from", "?field=to", "?field=nope", "?field=", "?field", "?field=a&field=b", "?limit=1&cursor=x&since=y"]) {
      const { text } = await box.client.get(`/api/mails/search/zebra${query}`);
      expect({ query, same: text === plain.text }).toEqual({ query, same: true });
    }
    // A word found only in the text is still found with field=subject.
    expect(subjects((await search(box, "crossing", "?field=subject")) as { subject: string }[])).toEqual([
      "Quarterly invoice for Zebra",
    ]);
  });
});

describe("GET /api/mails/search/:value — highlights and snippets", () => {
  let box: Mailbox;
  beforeAll(async () => {
    box = await newMailbox(server);
  });
  const only = async (value: string) => {
    const rows = await search(box, value);
    expect(rows).toHaveLength(1);
    return rows[0]!;
  };

  test("highlight holds the fields where a word was found, each as a one-item list", async () => {
    await box.mail({ subject: "Apricot jam", text: "A recipe." });
    await box.mail({ subject: "Recipe", text: "Use a ripe blackberry." });
    await box.mail({ subject: "Cherry pie", text: "The cherry must be fresh." });
    expect((await only("apricot")).highlight).toEqual({ subject: ["<em>Apricot</em> jam"] });
    expect((await only("blackberry")).highlight).toEqual({ text: ["Use a ripe <em>blackberry</em>."] });
    expect((await only("cherry")).highlight).toEqual({
      subject: ["<em>Cherry</em> pie"],
      text: ["The <em>cherry</em> must be fresh."],
    });
  });

  test("a match in a name or address only gives an empty highlight object", async () => {
    await box.mail({ subject: "Greetings", text: "Hello there", from: [addr("dana@partner.example", "Dana Scully")] });
    const row = await only("scully");
    expect(row.highlight).toEqual({});
    expect(row.snippet).toBe("Hello there");
  });

  test("each matching word is wrapped, in its original spelling", async () => {
    await box.mail({ subject: "Mango MANGO mangoes", text: "mango Mangos" });
    expect((await only("mango")).highlight).toEqual({
      subject: ["<em>Mango</em> <em>MANGO</em> <em>mangoes</em>"],
      text: ["<em>mango</em> <em>Mangos</em>"],
    });
  });

  test("a long text is cut to a fragment around the match; the snippet stays the start of the text", async () => {
    await box.mail({ subject: "Long", text: `${"filler ".repeat(100)}needle ${"filler ".repeat(100)}` });
    const row = await only("needle");
    expect(row.highlight).toEqual({
      text: ["<em>needle</em> filler filler filler filler filler filler filler filler filler"],
    });
    expect(row.snippet).toBe(`${"filler ".repeat(22).trimEnd()}…`);
  });

  test("a text that is one word repeated yields a fragment of ten words", async () => {
    await box.mail({ subject: "Many", text: "papaya ".repeat(200) });
    const [fragment] = (await only("papaya")).highlight.text;
    expect(fragment).toBe(Array(10).fill("<em>papaya</em>").join(" "));
  });

  test("a fragment grows to cover several distinct words of the value, up to fifty words", async () => {
    const words = Array.from({ length: 80 }, (_, i) => `w${i}`);
    words[5] = "papaw";
    words[30] = "pawpaw";
    words[70] = "paw";
    await box.mail({ subject: "Spread", text: words.join(" ") });
    const [fragment] = (await only("papaw pawpaw paw")).highlight.text;
    const shown = fragment.split(" ");
    expect(shown.length).toBeLessThanOrEqual(50);
    expect(fragment).toContain("<em>papaw</em>");
    expect(fragment).toContain("<em>pawpaw</em>");
  });

  test("markup in the stored subject or text is blanked out of the highlight, not escaped", async () => {
    await box.mail({ subject: "<script>alert(1)</script> lychee", text: "body with <em>lychee</em> & <b>more</b>" });
    const row = await only("lychee");
    expect(row.highlight).toEqual({
      subject: [" alert(1)  <em>lychee</em>"],
      text: ["body with  <em>lychee</em>  &  more "],
    });
    expect(row.subject).toBe("<script>alert(1)</script> lychee");
    expect(row.snippet).toBe("body with <em>lychee</em> & <b>more</b>");
  });

  test("the snippet is left out when the mail has no text", async () => {
    await box.mail({ subject: "Guava", text: "", html: "<p>html</p>" });
    const row = await only("guava");
    expect("snippet" in row).toBe(false);
    expect(row.highlight).toEqual({ subject: ["<em>Guava</em>"] });
  });
});

describe("GET /api/mails/search/:value — which mails, in which order", () => {
  test("searches received, sent and spam mail; spam is labelled; drafts and expunged mail are left out", async () => {
    const box = await newMailbox(server);
    const me = box.address();
    await box.mail({ subject: "Fig received", text: "x", date: "2026-01-05T00:00:00Z" });
    await box.mail({ subject: "Fig sent", text: "x", from: [addr(me)], to: [addr("x@remote.example")], envelope_to: [], sent: true, date: "2026-01-04T00:00:00Z" });
    await box.mail({ subject: "Fig spam", text: "x", is_spam: true, date: "2026-01-03T00:00:00Z" });
    await box.mail({ subject: "Fig draft", text: "x", draft: true });
    await box.mail({ subject: "Fig expunged", text: "x", expunged: true });
    await box.mail({ subject: "Fig deleted flag", text: "x", deleted: true, date: "2026-01-02T00:00:00Z" });
    const rows = await search(box, "fig");
    expect(rows.map((r) => [r.subject, r.spam, r.sent])).toEqual([
      ["Fig received", undefined, false],
      ["Fig sent", undefined, true],
      ["Fig spam", true, false],
      ["Fig deleted flag", undefined, false],
    ]);
  });

  test("never returns another user's mail", async () => {
    const first = await newMailbox(server);
    const second = await newMailbox(server);
    const admin = await adminMailbox(server);
    await second.mail({ subject: "Secret pomelo", to: [addr(first.address())] });
    expect(await search(first, "pomelo")).toEqual([]);
    expect(await search(admin, "pomelo")).toEqual([]);
    expect(await search(second, "pomelo")).toHaveLength(1);
  });

  test("orders by relevance, then newest first", async () => {
    const box = await newMailbox(server);
    await box.mail({ subject: "once", text: "quince", date: "2026-01-09T00:00:00Z" });
    await box.mail({ subject: "many", text: "quince ".repeat(30), date: "2026-01-01T00:00:00Z" });
    await box.mail({ subject: "twice", text: "quince and quince", date: "2026-01-05T00:00:00Z" });
    await box.mail({ subject: "once, older", text: "quince", date: "2026-01-02T00:00:00Z" });
    await box.mail({ subject: "once, newest", text: "quince", date: "2026-01-10T00:00:00Z" });
    expect(subjects((await search(box, "quince")) as { subject: string }[])).toEqual([
      "many",
      "twice",
      "once, newest",
      "once",
      "once, older",
    ]);
  });

  test("returns at most 1000 rows", async () => {
    const box = await newMailbox(server);
    const me = box.address();
    await server.db.query(
      `INSERT INTO mails (user_id, message_id, subject, date, to_address, to_text, envelope_to, text)
       SELECT $1, '<bulk-' || g || '@x>', 'bulk ' || g, timestamptz '2026-01-01T00:00:00Z' + g * interval '1 second',
              $2::jsonb, $3, $2::jsonb, 'persimmon'
       FROM generate_series(1, 1001) g`,
      [box.id, [{ address: me }], me]
    );
    const rows = await search(box, "persimmon");
    expect(rows).toHaveLength(1000);
    // Equal relevance, so newest first: the oldest row is the one cut.
    expect(rows[0]!.subject).toBe("bulk 1001");
    expect(rows[999]!.subject).toBe("bulk 2");
  }, 60_000);
});

describe("GET /api/mails/search-accounts/:value", () => {
  let box: Mailbox;
  beforeAll(async () => {
    box = await newMailbox(server);
    const me = box.address();
    await box.mail({ subject: "Tangerine one", text: "x", to: [addr(box.address("a"))], date: "2026-01-01T00:00:00Z" });
    await box.mail({ subject: "Tangerine two", text: "x", to: [addr(box.address("a"))], read: true, saved: true, date: "2026-01-03T00:00:00Z" });
    await box.mail({ subject: "Tangerine three", text: "x", to: [addr(box.address("b")), addr("x@remote.example")], cc: [addr(box.address("c"))], envelope_to: [{ address: box.address("d") }], date: "2026-01-02T00:00:00Z" });
    await box.mail({ subject: "Other fruit", text: "x", to: [addr(box.address("a"))], date: "2026-01-09T00:00:00Z" });
    await box.mail({ subject: "Tangerine spam", text: "x", to: [addr(box.address("s"))], is_spam: true, date: "2026-01-04T00:00:00Z" });
    await box.mail({ subject: "Tangerine sent", text: "x", from: [addr(me)], to: [addr("y@remote.example")], envelope_to: [], sent: true, date: "2026-01-05T00:00:00Z" });
    await box.mail({ subject: "Tangerine draft", text: "x", to: [addr(box.address("draft"))], draft: true });
    await box.mail({ subject: "Tangerine expunged", text: "x", to: [addr(box.address("gone"))], expunged: true });
  });
  const account = (local: string, doc: number, unread: number, saved: number, updated: string) => ({
    key: box.address(local),
    doc_count: doc,
    unread_doc_count: unread,
    saved_doc_count: saved,
    updated,
  });

  test("no match is an empty list with an exact body", async () => {
    const { status, text, headers } = await box.client.get("/api/mails/search-accounts/nothing");
    expect(status).toBe(200);
    expect(text).toBe(EMPTY);
    expect(headers.get("content-type")).toBe(JSON_TYPE);
    expect(headers.get("content-length")).toBe("30");
  });

  test("one account, byte for byte", async () => {
    const fresh = await newMailbox(server);
    await fresh.mail({ subject: "Nectarine", date: "2026-03-04T05:06:07.891Z" });
    const { text } = await fresh.client.get("/api/mails/search-accounts/nectarine");
    expect(text).toBe(
      `{"status":"success","body":[{"key":"${fresh.address()}","doc_count":1,"unread_doc_count":1,"saved_doc_count":0,` +
        `"updated":"2026-03-04T05:06:07.891Z"}]}`
    );
  });

  test("lists the user's accounts holding a matching mail, counting matching mails only, newest first", async () => {
    expect(await searchAccounts(box, "tangerine")).toEqual([
      account("s", 1, 1, 0, "2026-01-04T00:00:00.000Z"),
      account("a", 2, 1, 1, "2026-01-03T00:00:00.000Z"),
      expect.objectContaining({ updated: "2026-01-02T00:00:00.000Z" }),
      expect.objectContaining({ updated: "2026-01-02T00:00:00.000Z" }),
      expect.objectContaining({ updated: "2026-01-02T00:00:00.000Z" }),
    ]);
    const tied = (await searchAccounts(box, "tangerine")).slice(2);
    expect(tied.sort((x, y) => x.key.localeCompare(y.key))).toEqual([
      account("b", 1, 1, 0, "2026-01-02T00:00:00.000Z"),
      account("c", 1, 1, 0, "2026-01-02T00:00:00.000Z"),
      account("d", 1, 1, 0, "2026-01-02T00:00:00.000Z"),
    ]);
  });

  test("counts spam like any other match, and leaves out drafts, expunged mail and mail sent elsewhere", async () => {
    const keys = (await searchAccounts(box, "tangerine")).map((a) => a.key);
    expect(keys).toContain(box.address("s"));
    expect(keys).not.toContain(box.address("draft"));
    expect(keys).not.toContain(box.address("gone"));
    expect(keys).not.toContain(box.address());
    expect(await searchAccounts(box, "sent")).toEqual([]);
  });

  test("matches like the search itself: stems, all words, trimmed, nothing for a blank or wordless value", async () => {
    expect(await searchAccounts(box, "  TANGERINES  ")).toHaveLength(5);
    expect(await searchAccounts(box, "tangerine two")).toEqual([account("a", 1, 0, 1, "2026-01-03T00:00:00.000Z")]);
    expect(await searchAccounts(box, "tangerine fruit")).toEqual([]);
    for (const path of ["%20", "the", "%25", "%00", encodeURIComponent("'; DROP TABLE mails; --")]) {
      const { status, text } = await box.client.get(`/api/mails/search-accounts/${path}`);
      expect({ path, status, text }).toEqual({ path, status: 200, text: EMPTY });
    }
  });

  test("ignores the query string", async () => {
    const plain = await box.client.get("/api/mails/search-accounts/tangerine");
    const queried = await box.client.get("/api/mails/search-accounts/tangerine?field=subject&limit=1");
    expect(queried.body.body).toHaveLength(plain.body.body.length);
  });

  test("never counts another user's mail; admin sees only @mail.test accounts", async () => {
    const other = await newMailbox(server);
    const admin = await adminMailbox(server);
    await other.mail({ subject: "Clementine", to: [addr(box.address()), addr(other.address())] });
    await admin.mail({ subject: "Clementine", to: [addr("info@mail.test"), addr("x@admin.mail.test"), addr(box.address())], date: "2026-02-01T00:00:00Z" });
    expect(await searchAccounts(box, "clementine")).toEqual([]);
    expect((await searchAccounts(other, "clementine")).map((a) => a.key)).toEqual([other.address()]);
    expect(await searchAccounts(admin, "clementine")).toEqual([
      { key: "info@mail.test", doc_count: 1, unread_doc_count: 1, saved_doc_count: 0, updated: "2026-02-01T00:00:00.000Z" },
    ]);
  });
});
