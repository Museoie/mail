import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { rawHttp, startServer, type TestServer } from "../harness";
import {
  BAD_REQUEST,
  ISO_MS,
  JSON_TYPE,
  JSON_VALUES,
  NON_OBJECT_BODIES,
  UUID,
  adminMailbox,
  failed,
  newMailbox,
  type Mailbox,
} from "./mails-helpers";

let server: TestServer;

beforeAll(async () => {
  server = await startServer();
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

const SUCCESS = { status: "success" };
const REQUIRED = failed("pattern is required");
const BAD_PATTERN = failed("Pattern must be an email address (user@example.com) or domain wildcard (*@example.com)");
const EXISTS = failed("Entry already exists");
const TOO_LONG = failed("Pattern must be 320 bytes or fewer");
const AT_LIMIT = failed("Allowlist limit of 1000 entries reached");
const NO_ENTRY = failed("Entry not found");

const add = (box: Mailbox, pattern: unknown) => box.client.post("/api/mails/spam-allowlist", { pattern });
const remove = (box: Mailbox, pattern: string) => box.client.delete(`/api/mails/spam-allowlist/${encodeURIComponent(pattern)}`);
const patterns = async (box: Mailbox) => {
  const { status, body } = await box.client.get("/api/mails/spam-allowlist");
  expect(status).toBe(200);
  expect(body.status).toBe("success");
  return (body.body as { pattern: string }[]).map((entry) => entry.pattern);
};
const stored = (box: Mailbox) =>
  server.db.query<{ allowlist_id: string; pattern: string; created_at: Date }>(
    "SELECT allowlist_id, pattern, created_at FROM spam_allowlist WHERE user_id = $1 ORDER BY created_at, pattern",
    [box.id]
  );
/** An address of exactly `bytes` ASCII bytes. */
const ofLength = (bytes: number) => `${"a".repeat(bytes - "@example.com".length)}@example.com`;

describe("GET /api/mails/spam-allowlist", () => {
  test("is an empty list for a user with no entries, with an exact body", async () => {
    const box = await newMailbox(server);
    const { status, text, headers } = await box.client.get("/api/mails/spam-allowlist");
    expect(status).toBe(200);
    expect(text).toBe('{"status":"success","body":[]}');
    expect(headers.get("content-type")).toBe(JSON_TYPE);
    expect(headers.get("content-length")).toBe("30");
  });

  test("lists entries as id, pattern, createdAt — newest first", async () => {
    const box = await newMailbox(server);
    await server.db.query(
      "INSERT INTO spam_allowlist (user_id, pattern, created_at) VALUES ($1, 'old@example.com', '2026-01-01T00:00:00Z'), ($1, '*@new.example', '2026-03-01 12:34:56.789123+09'), ($1, 'mid@example.com', '2026-02-01T00:00:00Z')",
      [box.id]
    );
    const rows = await stored(box);
    const id = (pattern: string) => rows.find((row) => row.pattern === pattern)!.allowlist_id;
    const { text } = await box.client.get("/api/mails/spam-allowlist");
    expect(text).toBe(
      `{"status":"success","body":[` +
        `{"id":"${id("*@new.example")}","pattern":"*@new.example","createdAt":"2026-03-01T03:34:56.789Z"},` +
        `{"id":"${id("mid@example.com")}","pattern":"mid@example.com","createdAt":"2026-02-01T00:00:00.000Z"},` +
        `{"id":"${id("old@example.com")}","pattern":"old@example.com","createdAt":"2026-01-01T00:00:00.000Z"}]}`
    );
  });

  test("each user has their own list, admin included", async () => {
    const first = await newMailbox(server);
    const second = await newMailbox(server);
    const admin = await adminMailbox(server);
    expect((await add(first, "shared@example.com")).body.status).toBe("success");
    expect((await add(second, "shared@example.com")).body.status).toBe("success");
    expect((await add(second, "only-second@example.com")).body.status).toBe("success");
    expect(await patterns(first)).toEqual(["shared@example.com"]);
    expect((await patterns(second)).sort()).toEqual(["only-second@example.com", "shared@example.com"]);
    expect(await patterns(admin)).toEqual([]);
  });

  test("ignores the query string", async () => {
    const box = await newMailbox(server);
    await add(box, "a@example.com");
    expect((await box.client.get("/api/mails/spam-allowlist?limit=0&pattern=x")).body.body).toHaveLength(1);
  });
});

describe("POST /api/mails/spam-allowlist — adding", () => {
  test("answers the new entry, byte for byte, and stores it", async () => {
    const box = await newMailbox(server);
    const before = Date.now();
    const { status, text, body, headers } = await add(box, "friend@example.com");
    expect(status).toBe(200);
    expect(headers.get("content-type")).toBe(JSON_TYPE);
    expect(text).toBe(
      `{"status":"success","body":{"id":"${body.body.id}","pattern":"friend@example.com","createdAt":"${body.body.createdAt}"}}`
    );
    expect(body.body.id).toMatch(UUID);
    expect(body.body.createdAt).toMatch(ISO_MS);
    expect(Date.parse(body.body.createdAt)).toBeGreaterThanOrEqual(before - 1000);
    expect(Date.parse(body.body.createdAt)).toBeLessThanOrEqual(Date.now() + 1000);
    const rows = await stored(box);
    expect(rows).toHaveLength(1);
    expect({ id: rows[0]!.allowlist_id, pattern: rows[0]!.pattern }).toEqual({ id: body.body.id, pattern: "friend@example.com" });
    expect((await box.client.get("/api/mails/spam-allowlist")).body.body).toEqual([body.body]);
  });

  test("accepts an address or a *@domain wildcard, where the domain has a dot", async () => {
    const box = await newMailbox(server);
    const accepted = [
      "user@example.com",
      "*@example.com",
      "*@sub.example.co.uk",
      "user+tag@example.com",
      "first.last@example.com",
      "a@b.c",
      "ü@exämple.com",
      "日本@例え.jp",
      "<a@b.co>",
      "a@b..co",
      "a*@example.com",
      "**@example.com",
      "*@*.com",
      "%@example.com",
      "_@example.com",
      "'quoted'@example.com",
      "a@b.co;",
    ];
    for (const pattern of accepted) {
      const { body } = await add(box, pattern);
      expect({ pattern, status: body.status, stored: body.body?.pattern }).toEqual({ pattern, status: "success", stored: pattern });
    }
    expect((await patterns(box)).sort()).toEqual([...accepted].sort());
  });

  test("refuses anything else as a bad pattern", async () => {
    const box = await newMailbox(server);
    for (const pattern of [
      "abc", "@example.com", "a@b", "a@b.", "a@.b", "a@", "*", "*@", "*@b", "a b@c.de", " a@b.co", "a@b.co ", "a@b.co\n",
      "a\t@b.co", "a@@b.co", "a@b@c.de", "*@a@b.co", "a@b c.de", "example.com", "*.example.com", "@", ".",
    ]) {
      const { status, body } = await add(box, pattern);
      expect({ pattern, status, body }).toEqual({ pattern, status: 200, body: BAD_PATTERN });
    }
    expect(await stored(box)).toEqual([]);
  });

  test("the bad-pattern refusal is exact", async () => {
    const box = await newMailbox(server);
    const { text } = await add(box, "nope");
    expect(text).toBe(
      '{"status":"failed","message":"Pattern must be an email address (user@example.com) or domain wildcard (*@example.com)"}'
    );
  });

  test("pattern must be a non-empty string: every other JSON type is refused as missing", async () => {
    const box = await newMailbox(server);
    for (const [label, value] of JSON_VALUES) {
      if (label === "a string") continue;
      const { status, body } = await add(box, value);
      expect({ label, status, body }).toEqual({ label, status: 200, body: REQUIRED });
    }
    for (const body of [{}, { Pattern: "a@b.co" }, { patterns: ["a@b.co"] }, { email: "a@b.co" }]) {
      expect({ body, answer: (await box.client.post("/api/mails/spam-allowlist", body)).body }).toEqual({ body, answer: REQUIRED });
    }
    expect(await stored(box)).toEqual([]);
  });

  test("stores the pattern lower-cased, so another letter case is a duplicate", async () => {
    const box = await newMailbox(server);
    expect((await add(box, "Friend@Example.COM")).body.body.pattern).toBe("friend@example.com");
    expect((await add(box, "*@Example.COM")).body.body.pattern).toBe("*@example.com");
    for (const pattern of ["friend@example.com", "FRIEND@EXAMPLE.COM", "*@example.com", "*@EXAMPLE.com"]) {
      const { status, body } = await add(box, pattern);
      expect({ pattern, status, body }).toEqual({ pattern, status: 200, body: EXISTS });
    }
    expect((await patterns(box)).sort()).toEqual(["*@example.com", "friend@example.com"]);
  });

  test("an address and the wildcard of its domain are separate entries", async () => {
    const box = await newMailbox(server);
    expect((await add(box, "a@example.com")).body.status).toBe("success");
    expect((await add(box, "*@example.com")).body.status).toBe("success");
    expect((await add(box, "b@example.com")).body.status).toBe("success");
    expect(await patterns(box)).toHaveLength(3);
  });

  test("a pattern may be 320 bytes long, not 321", async () => {
    const box = await newMailbox(server);
    expect((await add(box, ofLength(320))).body.body.pattern).toBe(ofLength(320));
    const over = await add(box, ofLength(321));
    expect(over.status).toBe(200);
    expect(over.text).toBe('{"status":"failed","message":"Pattern must be 320 bytes or fewer"}');
    expect((await add(box, ofLength(5000))).body).toEqual(TOO_LONG);
    expect(await patterns(box)).toEqual([ofLength(320)]);
  });

  test("the length is counted in UTF-8 bytes of the lower-cased pattern", async () => {
    const box = await newMailbox(server);
    // "é" is two bytes: 154 of them and "@example.com" make 320.
    expect((await add(box, `${"é".repeat(154)}@example.com`)).body.status).toBe("success");
    expect((await add(box, `${"é".repeat(154)}x@example.com`)).body).toEqual(TOO_LONG);
    // 80 four-byte emoji are 320 bytes on their own.
    expect((await add(box, `${"😀".repeat(77)}@example.com`)).body.status).toBe("success");
    expect((await add(box, `${"😀".repeat(78)}@example.com`)).body).toEqual(TOO_LONG);
  });

  test("a user may hold 1000 entries; the 1001st is refused, a duplicate still reads as a duplicate", async () => {
    const box = await newMailbox(server);
    await server.db.query(
      "INSERT INTO spam_allowlist (user_id, pattern) SELECT $1, 'u' || g || '@example.com' FROM generate_series(1, 999) g",
      [box.id]
    );
    expect((await add(box, "last@example.com")).body.status).toBe("success");
    const over = await add(box, "over@example.com");
    expect(over.status).toBe(200);
    expect(over.text).toBe('{"status":"failed","message":"Allowlist limit of 1000 entries reached"}');
    expect((await add(box, "*@over.example")).body).toEqual(AT_LIMIT);
    expect((await add(box, "U5@Example.com")).body).toEqual(EXISTS);
    expect((await add(box, ofLength(321))).body).toEqual(TOO_LONG);
    expect((await add(box, "bad")).body).toEqual(BAD_PATTERN);
    expect(await patterns(box)).toHaveLength(1000);
    // Removing one makes room for one.
    expect((await remove(box, "u1@example.com")).body).toEqual(SUCCESS);
    expect((await add(box, "over@example.com")).body.status).toBe("success");
    expect((await add(box, "again@example.com")).body).toEqual(AT_LIMIT);
    // Another user's list is not affected.
    const other = await newMailbox(server);
    expect((await add(other, "over@example.com")).body.status).toBe("success");
  });

  test("bodies that are not a JSON object", async () => {
    const box = await newMailbox(server);
    for (const { label, body, headers, parsed } of NON_OBJECT_BODIES) {
      const response = await box.client.request("POST", "/api/mails/spam-allowlist", { body, headers });
      const expected = parsed === "bad-request" ? { status: 400, body: BAD_REQUEST } : { status: 200, body: REQUIRED };
      expect({ label, status: response.status, body: response.body }).toEqual({ label, ...expected });
    }
    for (const [label, options] of [
      ["text/plain", { body: '{"pattern":"a@b.co"}', headers: { "Content-Type": "text/plain" } }],
      ["form-encoded", { body: "pattern=a%40b.co", headers: { "Content-Type": "application/x-www-form-urlencoded" } }],
    ] as const) {
      const { status, body } = await box.client.request("POST", "/api/mails/spam-allowlist", options);
      expect({ label, status, body }).toEqual({ label, status: 200, body: REQUIRED });
    }
    expect(await stored(box)).toEqual([]);
  });

  test("unknown fields are ignored: an entry cannot be planted for another user", async () => {
    const box = await newMailbox(server);
    const other = await newMailbox(server);
    const { body } = await box.client.post("/api/mails/spam-allowlist", {
      pattern: "mine@example.com",
      user_id: other.id,
      id: "11111111-1111-4111-8111-111111111111",
      createdAt: "2000-01-01T00:00:00Z",
    });
    expect(body.status).toBe("success");
    expect(body.body.id).not.toBe("11111111-1111-4111-8111-111111111111");
    expect(Date.parse(body.body.createdAt)).toBeGreaterThan(Date.parse("2026-01-01T00:00:00Z"));
    expect(await patterns(other)).toEqual([]);
    expect(await patterns(box)).toEqual(["mine@example.com"]);
  });
});

describe("DELETE /api/mails/spam-allowlist/:pattern", () => {
  test("removes the entry, with an exact answer; a second attempt finds nothing", async () => {
    const box = await newMailbox(server);
    await add(box, "friend@example.com");
    await add(box, "keep@example.com");
    const { status, text, headers } = await remove(box, "friend@example.com");
    expect(status).toBe(200);
    expect(text).toBe('{"status":"success"}');
    expect(headers.get("content-type")).toBe(JSON_TYPE);
    expect(await patterns(box)).toEqual(["keep@example.com"]);
    const again = await remove(box, "friend@example.com");
    expect(again.status).toBe(200);
    expect(again.text).toBe('{"status":"failed","message":"Entry not found"}');
  });

  test("matches the pattern in any letter case, and with @ and * unencoded", async () => {
    const box = await newMailbox(server);
    await add(box, "friend@example.com");
    await add(box, "*@example.com");
    await add(box, "third@example.com");
    expect((await remove(box, "FRIEND@Example.COM")).body).toEqual(SUCCESS);
    expect((await box.client.delete("/api/mails/spam-allowlist/*@EXAMPLE.com")).body).toEqual(SUCCESS);
    expect((await box.client.delete("/api/mails/spam-allowlist/third@example.com/")).body).toEqual(SUCCESS);
    expect(await patterns(box)).toEqual([]);
  });

  test("is exact: a wildcard removes only itself, and % or _ are not wildcards", async () => {
    const box = await newMailbox(server);
    for (const pattern of ["a@example.com", "b@example.com", "*@example.com", "ab@example.com"]) await add(box, pattern);
    for (const pattern of ["%", "%@example.com", "_@example.com", "*", "@example.com", "example.com", "a@example.co", " a@example.com"]) {
      const { status, body } = await remove(box, pattern);
      expect({ pattern, status, body }).toEqual({ pattern, status: 200, body: NO_ENTRY });
    }
    expect((await remove(box, "*@example.com")).body).toEqual(SUCCESS);
    expect((await patterns(box)).sort()).toEqual(["a@example.com", "ab@example.com", "b@example.com"]);
  });

  test("takes the pattern, not the entry id", async () => {
    const box = await newMailbox(server);
    const { body } = await add(box, "friend@example.com");
    expect((await remove(box, body.body.id)).body).toEqual(NO_ENTRY);
    expect(await patterns(box)).toEqual(["friend@example.com"]);
  });

  test("removes patterns with unicode and with characters that need encoding", async () => {
    const box = await newMailbox(server);
    for (const pattern of ["ü@exämple.com", "user+tag@example.com", "%@example.com", "a/b@example.com", "q?x=1@example.com", "h#1@example.com"]) {
      expect({ pattern, added: (await add(box, pattern)).body.status }).toEqual({ pattern, added: "success" });
      expect({ pattern, removed: (await remove(box, pattern)).body }).toEqual({ pattern, removed: SUCCESS });
    }
    expect(await patterns(box)).toEqual([]);
  });

  test("never removes another user's entry", async () => {
    const first = await newMailbox(server);
    const second = await newMailbox(server);
    const admin = await adminMailbox(server);
    await add(first, "friend@example.com");
    expect((await remove(second, "friend@example.com")).body).toEqual(NO_ENTRY);
    expect((await remove(admin, "friend@example.com")).body).toEqual(NO_ENTRY);
    expect(await patterns(first)).toEqual(["friend@example.com"]);
  });

  test("an undecodable pattern is a 400 and a very long one is simply not found", async () => {
    const box = await newMailbox(server);
    const response = await rawHttp(
      server.ports.http,
      `DELETE /api/mails/spam-allowlist/%E0%A4%A HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nCookie: ${box.client.cookieHeader()}\r\n\r\n`
    );
    expect(response.status).toBe(400);
    expect(response.body).toBe('{"status":"error","message":"Bad Request"}');
    expect((await remove(box, ofLength(5000))).body).toEqual(NO_ENTRY);
  });

  test("a pattern with a raw slash is another path and reaches no route", async () => {
    const box = await newMailbox(server);
    await add(box, "a/b@example.com");
    const { status } = await box.client.delete("/api/mails/spam-allowlist/a/b@example.com");
    expect(status).toBe(404);
    expect(await patterns(box)).toEqual(["a/b@example.com"]);
  });
});
