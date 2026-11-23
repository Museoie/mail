import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import crypto from "crypto";
import { waitFor, type ApiResult, type TestServer } from "../harness";
import { startServer } from "./core-helpers";
import { JSON_TYPE, NOT_FOUND, eventId, mailgunSignature } from "./core-helpers";

/** Made up for this run; it signs nothing outside it. */
const KEY = crypto.randomBytes(24).toString("hex");

const SUCCESS = { status: "success" };
const NOT_CONFIGURED = { status: "failed", message: "Webhook not configured" };
const INVALID_SIGNATURE = { status: "failed", message: "Invalid signature" };
const MALFORMED = { status: "failed", message: "Malformed event payload" };

const KNOWN_EVENTS = [
  "accepted",
  "rejected",
  "delivered",
  "failed",
  "opened",
  "clicked",
  "unsubscribed",
  "complained",
  "stored",
  "permanent_fail",
  "temporary_fail",
];

/** Production posture: every client has its own address and its own sixty requests. */
let server: TestServer;

beforeAll(async () => {
  server = await startServer({ production: true, env: { MAILGUN_WEBHOOK_SIGNING_KEY: KEY } });
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

const nowSeconds = () => Math.floor(Date.now() / 1000);

/** Posts one signed event. */
const send = (eventData: unknown, target: TestServer = server): Promise<ApiResult> =>
  target.client().post("/api/mailgun-events", { signature: mailgunSignature(KEY), "event-data": eventData });

interface EventRow {
  event_id: string;
  event: string;
  message_id: string | null;
  recipient: string | null;
  severity: string | null;
  reason: string | null;
  occurred_at: Date;
  received_at: Date;
  raw: unknown;
  raw_text: string;
  updated: Date;
}

const rowOf = async (id: string, target: TestServer = server): Promise<EventRow | undefined> =>
  (await target.db.query<EventRow>("SELECT *, raw::text AS raw_text FROM mailgun_events WHERE event_id = $1", [id]))[0];

const rowCount = async () => (await server.db.query("SELECT COUNT(*)::int AS count FROM mailgun_events"))[0]!.count as number;

const isNow = (at: Date) => Math.abs(at.getTime() - Date.now()) < 15_000;

/** Sends an event that must be stored, and returns its row. */
const stored = async (fields: Record<string, unknown>): Promise<EventRow> => {
  const id = eventId();
  const result = await send({ id, event: "delivered", ...fields });
  expect({ status: result.status, body: result.body }).toEqual({ status: 200, body: SUCCESS });
  const row = await rowOf(id);
  expect(row).toBeDefined();
  return row!;
};

/** Sends an event that must be refused as malformed and leave no row. */
const expectMalformed = async (eventData: unknown) => {
  const before = await rowCount();
  const result = await send(eventData);
  expect({ status: result.status, body: result.body }).toEqual({ status: 200, body: MALFORMED });
  expect(await rowCount()).toBe(before);
};

describe("POST /api/mailgun-events without a signing key", () => {
  test("refuses every event with a 200, signed or not", async () => {
    for (const env of [{}, { MAILGUN_WEBHOOK_SIGNING_KEY: "" }]) {
      const unconfigured = await startServer({ env });
      try {
        const signed = await send({ id: eventId(), event: "delivered" }, unconfigured);
        expect(signed.status).toBe(200);
        expect(signed.text).toBe('{"status":"failed","message":"Webhook not configured"}');
        expect(signed.headers.get("content-type")).toBe(JSON_TYPE);
        for (const body of [{}, [], { signature: mailgunSignature("") }]) {
          expect((await unconfigured.client().post("/api/mailgun-events", body)).body).toEqual(NOT_CONFIGURED);
        }
        expect((await unconfigured.client().request("POST", "/api/mailgun-events")).body).toEqual(NOT_CONFIGURED);
        expect(await unconfigured.db.query("SELECT 1 FROM mailgun_events")).toEqual([]);
      } finally {
        await unconfigured.stop();
      }
    }
  }, 120_000);
});

describe("POST /api/mailgun-events — the signature", () => {
  const event = () => ({ id: eventId("sig"), event: "delivered" });
  const post = (signature: unknown, eventData: unknown = event()) =>
    server.client().post("/api/mailgun-events", { signature, "event-data": eventData });
  const expectRefused = async (signature: unknown) => {
    const data = event();
    const result = await post(signature, data);
    expect({ signature, status: result.status, body: result.body }).toEqual({
      signature,
      status: 200,
      body: INVALID_SIGNATURE,
    });
    expect(await rowOf(data.id)).toBeUndefined();
  };
  const expectAccepted = async (signature: unknown) => {
    const data = event();
    const result = await post(signature, data);
    expect({ signature, status: result.status, body: result.body }).toEqual({ signature, status: 200, body: SUCCESS });
    expect(await rowOf(data.id)).toBeDefined();
  };

  test("accepts HMAC-SHA256 of timestamp + token under the signing key", async () => {
    const signature = mailgunSignature(KEY);
    expect(signature.signature).toMatch(/^[0-9a-f]{64}$/);
    await expectAccepted(signature);
  });

  test("refuses a missing signature block", async () => {
    const data = event();
    for (const body of [{ "event-data": data }, {}, [], [{ signature: mailgunSignature(KEY), "event-data": data }]]) {
      const result = await server.client().post("/api/mailgun-events", body);
      expect(result.status).toBe(200);
      expect(result.body).toEqual(INVALID_SIGNATURE);
    }
    expect(await rowOf(data.id)).toBeUndefined();
  });

  test("refuses a body that is not JSON it reads", async () => {
    const signature = mailgunSignature(KEY);
    const form = await server.client().request("POST", "/api/mailgun-events", {
      body: new URLSearchParams({ ...signature, event: "delivered" }),
    });
    expect(form.body).toEqual(INVALID_SIGNATURE);
    const none = await server.client().request("POST", "/api/mailgun-events");
    expect(none.body).toEqual(INVALID_SIGNATURE);
  });

  test.each([
    ["null", null],
    ["a string", "signature"],
    ["a number", 1],
    ["zero", 0],
    ["true", true],
    ["false", false],
    ["an empty array", []],
    ["an array of the three values", ["timestamp", "token", "signature"]],
    ["an empty object", {}],
  ])("refuses a signature block that is %s", async (_label, signature) => {
    await expectRefused(signature);
  });

  test.each(["timestamp", "token", "signature"])("refuses a %s that is missing, empty or not a string", async (field) => {
    const good = mailgunSignature(KEY);
    const { [field as keyof typeof good]: _dropped, ...without } = good;
    await expectRefused(without);
    for (const value of ["", null, 0, 1, true, false, [], [good[field as keyof typeof good]], { value: 1 }]) {
      await expectRefused({ ...good, [field]: value });
    }
  });

  test("refuses a numeric timestamp even when the signature would match its text", async () => {
    const good = mailgunSignature(KEY);
    await expectRefused({ ...good, timestamp: Number(good.timestamp) });
  });

  test("refuses a signature made with another key, or over other values", async () => {
    await expectRefused(mailgunSignature("another-key"));
    await expectRefused(mailgunSignature(""));
    await expectRefused(mailgunSignature(KEY.toUpperCase()));
    const good = mailgunSignature(KEY);
    await expectRefused({ ...good, token: `${good.token}x` });
    await expectRefused({ ...good, timestamp: String(Number(good.timestamp) + 1) });
    await expectRefused({ ...good, signature: good.signature.replace(/.$/, (c) => (c === "0" ? "1" : "0")) });
    // The token and the timestamp are concatenated in that order.
    await expectRefused({
      ...good,
      signature: crypto.createHmac("sha256", KEY).update(good.token + good.timestamp).digest("hex"),
    });
  });

  test("refuses a signature in another encoding or of another length", async () => {
    const good = mailgunSignature(KEY);
    const bytes = Buffer.from(good.signature, "hex");
    await expectRefused({ ...good, signature: bytes.toString("base64") });
    await expectRefused({ ...good, signature: bytes.toString("base64url") });
    await expectRefused({ ...good, signature: good.signature.slice(0, 62) });
    await expectRefused({ ...good, signature: `${good.signature}00` });
    await expectRefused({ ...good, signature: `0x${good.signature}` });
    await expectRefused({ ...good, signature: ` ${good.signature}` });
    await expectRefused({ ...good, signature: "zz" });
    await expectRefused({ ...good, signature: "sha256=" + good.signature });
  });

  test("reads the signature's hex leniently: any letter case, and nothing after the first non-hex pair", async () => {
    const good = mailgunSignature(KEY);
    await expectAccepted({ ...good, signature: good.signature.toUpperCase() });
    // Decoding stops at what is not hex, so trailing junk does not change the bytes.
    for (const tail of ["zz", " ", "\n", "0", "g0", "-and-anything-else"]) {
      const withTail = mailgunSignature(KEY);
      await expectAccepted({ ...withTail, signature: withTail.signature + tail });
    }
  });

  test("refuses a timestamp more than fifteen minutes old or ahead", async () => {
    for (const offset of [-930, -3600, -86_400, 930, 3600, 86_400]) {
      await expectRefused(mailgunSignature(KEY, String(nowSeconds() + offset)));
    }
    await expectRefused(mailgunSignature(KEY, "0"));
    await expectRefused(mailgunSignature(KEY, "1"));
    await expectRefused(mailgunSignature(KEY, String(Date.now())));
    await expectRefused(mailgunSignature(KEY, "-" + nowSeconds()));
  });

  test("accepts a timestamp inside the fifteen minutes, either way", async () => {
    for (const offset of [-870, -600, 600, 870]) {
      await expectAccepted(mailgunSignature(KEY, String(nowSeconds() + offset)));
    }
  });

  test("refuses a timestamp that is not a number", async () => {
    for (const timestamp of [
      "now",
      "NaN",
      "Infinity",
      "-Infinity",
      `${nowSeconds()}s`,
      `1 ${nowSeconds()}`,
      `${nowSeconds()},5`,
      `1_${nowSeconds()}`,
      new Date().toISOString(),
      " ",
      "\t",
      "--5",
      "0x",
      "-0x10",
      "1e",
      ".",
      `\u0085${nowSeconds()}`,
      `${nowSeconds()}᠎`,
    ]) {
      await expectRefused(mailgunSignature(KEY, timestamp));
    }
  });

  test("reads the timestamp as JavaScript reads a number from text", async () => {
    const seconds = nowSeconds();
    for (const timestamp of [
      `${seconds}.5`,
      `${seconds}.`,
      `+${seconds}`,
      `  ${seconds}  `,
      `\t${seconds}\n`,
      ` ${seconds} `,
      `﻿${seconds}`,
      `000${seconds}`,
      `${seconds}e0`,
      `${seconds / 1000}E3`,
      `${seconds}0e-1`,
      `0x${seconds.toString(16)}`,
      `0X${seconds.toString(16).toUpperCase()}`,
      `0o${seconds.toString(8)}`,
      `0b${seconds.toString(2)}`,
    ]) {
      await expectAccepted(mailgunSignature(KEY, timestamp));
    }
  });

  test("the signature covers nothing of the event: one signature admits any number of events", async () => {
    const signature = mailgunSignature(KEY);
    for (let i = 0; i < 3; i++) await expectAccepted(signature);
  });
});

describe("POST /api/mailgun-events — malformed event data", () => {
  test.each([
    ["missing", undefined],
    ["null", null],
    ["a string", "event"],
    ["a number", 7],
    ["true", true],
    ["an empty array", []],
    ["an array holding an event", [{ id: "in-an-array", event: "delivered" }]],
    ["an empty object", {}],
  ])("event data that is %s", async (_label, eventData) => {
    await expectMalformed(eventData);
  });

  test.each([
    ["missing", undefined],
    ["empty", ""],
    ["null", null],
    ["zero", 0],
    ["a number", 12345],
    ["true", true],
    ["false", false],
    ["an array", ["an-id"]],
    ["an object", { value: "an-id" }],
  ])("an id that is %s", async (_label, id) => {
    await expectMalformed({ id, event: "delivered" });
  });

  test.each([
    ["missing", undefined],
    ["empty", ""],
    ["null", null],
    ["zero", 0],
    ["a number", 3],
    ["true", true],
    ["an array", ["delivered"]],
    ["an object", { name: "delivered" }],
    ["unknown", "bounced"],
    ["in another letter case", "Delivered"],
    ["padded with a space", "delivered "],
    ["a known name with a suffix", "delivered2"],
    ["an inherited property name", "toString"],
    ["another inherited property name", "length"],
  ])("an event that is %s", async (_label, event) => {
    await expectMalformed({ id: eventId(), event });
  });

  test("an id longer than 128 characters", async () => {
    const row = await (async () => {
      const id = "i".repeat(128 - 13) + crypto.randomBytes(6).toString("hex") + "-";
      expect(id).toHaveLength(128);
      expect((await send({ id, event: "delivered" })).body).toEqual(SUCCESS);
      return rowOf(id);
    })();
    expect(row?.event_id).toHaveLength(128);
    await expectMalformed({ id: `${"i".repeat(117)}${crypto.randomBytes(6).toString("hex")}`, event: "delivered" });
  });

  test("the id's length is counted in UTF-16 units", async () => {
    const unique = crypto.randomBytes(4).toString("hex");
    const fits = `${unique}${"🆔".repeat(60)}`;
    expect(fits).toHaveLength(128);
    expect((await send({ id: fits, event: "delivered" })).body).toEqual(SUCCESS);
    expect((await rowOf(fits))?.event_id).toBe(fits);
    await expectMalformed({ id: `${unique}x${"🆔".repeat(60)}`, event: "delivered" });
  });

  test("a recipient longer than 320 characters", async () => {
    const row = await stored({ recipient: "r".repeat(320) });
    expect(row.recipient).toHaveLength(320);
    await expectMalformed({ id: eventId(), event: "delivered", recipient: "r".repeat(321) });
  });

  test("a severity longer than 32 characters", async () => {
    const row = await stored({ severity: "s".repeat(32) });
    expect(row.severity).toHaveLength(32);
    await expectMalformed({ id: eventId(), event: "delivered", severity: "s".repeat(33) });
  });

  test("a message id longer than 1024 characters", async () => {
    const row = await stored({ message: { headers: { "message-id": "m".repeat(1024) } } });
    expect(row.message_id).toHaveLength(1024);
    await expectMalformed({ id: eventId(), event: "delivered", message: { headers: { "message-id": "m".repeat(1025) } } });
  });

  test("a reason longer than 2048 characters", async () => {
    const row = await stored({ "delivery-status": { message: "r".repeat(2048) } });
    expect(row.reason).toHaveLength(2048);
    await expectMalformed({ id: eventId(), event: "delivered", "delivery-status": { message: "r".repeat(2049) } });

    // The limit is on the joined reason: code, message and description with their separators.
    const joined = await stored({
      "delivery-status": { code: 550, message: "m".repeat(1000), description: "d".repeat(1039) },
    });
    expect(joined.reason).toHaveLength(2048);
    await expectMalformed({
      id: eventId(),
      event: "delivered",
      "delivery-status": { code: 550, message: "m".repeat(1000), description: "d".repeat(1040) },
    });
  });

  test("the limits are checked in a fixed order, and only on strings", async () => {
    // Over every limit at once, with an unknown event: all the same answer.
    await expectMalformed({
      id: "x".repeat(200),
      event: "nope",
      recipient: "r".repeat(400),
      severity: "s".repeat(40),
    });
    // Values of other types are not measured; they are stored as NULL.
    const row = await stored({
      recipient: ["r".repeat(400)],
      severity: { level: "s".repeat(40) },
      message: { headers: { "message-id": ["m".repeat(2000)] } },
    });
    expect(row).toMatchObject({ recipient: null, severity: null, message_id: null });
  });
});

describe("POST /api/mailgun-events — what is stored", () => {
  test.each(KNOWN_EVENTS)("stores a %s event", async (event) => {
    const id = eventId(event);
    const result = await send({ id, event });
    expect(result.status).toBe(200);
    expect(result.text).toBe('{"status":"success"}');
    expect(result.headers.get("content-type")).toBe(JSON_TYPE);
    expect(result.setCookies).toEqual([]);
    expect((await rowOf(id))?.event).toBe(event);
  });

  test("a full event, column by column", async () => {
    const id = eventId("full");
    const data = {
      id,
      event: "failed",
      timestamp: 1521472262.908181,
      recipient: "bob@elsewhere.example",
      "recipient-domain": "elsewhere.example",
      severity: "permanent",
      reason: "bounce",
      "log-level": "error",
      "delivery-status": {
        "attempt-no": 1,
        code: 550,
        message: "5.1.1 The email account that you tried to reach does not exist.",
        description: "",
        "session-seconds": 0.4331989288330078,
        tls: true,
        "certificate-verified": true,
        "mx-host": "mx.elsewhere.example",
      },
      message: {
        headers: {
          "message-id": "20260101120000.1.ABCDEF@mail.test",
          from: "Administrator <admin@mail.test>",
          to: "Bob <bob@elsewhere.example>",
          subject: "Hello ünï 🔔",
        },
        attachments: [],
        size: 512,
      },
      flags: { "is-routed": false, "is-authenticated": true, "is-system-test": false, "is-test-mode": false },
      envelope: { transport: "smtp", sender: "admin@mail.test", "sending-ip": "209.61.154.250", targets: "bob@elsewhere.example" },
      tags: ["one", "two"],
      "user-variables": { campaign: null, nested: { deep: [1, 2, { three: 3 }] } },
      storage: null,
    };
    const result = await send(data);
    expect(result.body).toEqual(SUCCESS);

    const row = (await rowOf(id))!;
    expect(Object.keys(row).sort()).toEqual([
      "event",
      "event_id",
      "message_id",
      "occurred_at",
      "raw",
      "raw_text",
      "reason",
      "received_at",
      "recipient",
      "severity",
      "updated",
    ]);
    expect(row).toMatchObject({
      event_id: id,
      event: "failed",
      message_id: "20260101120000.1.ABCDEF@mail.test",
      recipient: "bob@elsewhere.example",
      severity: "permanent",
      // From delivery-status, not from the event's own "reason"; the empty description is left out.
      reason: "550 · 5.1.1 The email account that you tried to reach does not exist.",
    });
    expect(row.occurred_at.toISOString()).toBe("2018-03-19T15:11:02.908Z");
    expect(isNow(row.received_at)).toBe(true);
    expect(isNow(row.updated)).toBe(true);
    expect(row.raw).toEqual(data);
  });

  test("a minimal event", async () => {
    const id = eventId("minimal");
    expect((await send({ id, event: "delivered" })).body).toEqual(SUCCESS);
    const row = (await rowOf(id))!;
    expect(row).toMatchObject({
      event_id: id,
      event: "delivered",
      message_id: null,
      recipient: null,
      severity: null,
      reason: null,
    });
    expect(isNow(row.occurred_at)).toBe(true);
    expect(isNow(row.received_at)).toBe(true);
    expect(row.raw_text).toBe(`{"id": "${id}", "event": "delivered"}`);
  });

  test("optional fields of the wrong type are stored as NULL, and kept in raw", async () => {
    const data = {
      recipient: 42,
      severity: true,
      message: { headers: { "message-id": { id: "x" } } },
      "delivery-status": "550 mailbox unavailable",
    };
    const row = await stored(data);
    expect(row).toMatchObject({ recipient: null, severity: null, message_id: null, reason: null });
    expect(row.raw).toMatchObject(data);
  });

  test("empty strings are stored as they are", async () => {
    const row = await stored({ recipient: "", severity: "", message: { headers: { "message-id": "" } } });
    expect(row).toMatchObject({ recipient: "", severity: "", message_id: "" });
  });

  test.each([
    ["message is missing", {}],
    ["message is null", { message: null }],
    ["message is a string", { message: "text" }],
    ["message is a number", { message: 5 }],
    ["message is an array", { message: [{ headers: { "message-id": "x" } }] }],
    ["headers is missing", { message: {} }],
    ["headers is null", { message: { headers: null } }],
    ["headers is a string", { message: { headers: "message-id: x" } }],
    ["headers is an array of pairs", { message: { headers: [["message-id", "x"]] } }],
    ["the header is missing", { message: { headers: { "Message-Id": "wrong-case" } } }],
    ["the header is null", { message: { headers: { "message-id": null } } }],
    ["the header is a number", { message: { headers: { "message-id": 20260101 } } }],
  ])("no message id when %s", async (_label, fields) => {
    expect((await stored(fields)).message_id).toBeNull();
  });

  test("text columns keep unicode, quotes, newlines and leading or trailing space", async () => {
    const fields = {
      recipient: "  Bób \"the\" <bob@elsewhere.example>\n",
      severity: "pérmanent\t",
      message: { headers: { "message-id": "<ünï'--;DROP TABLE mailgun_events;@mail.test> 🔔" } },
      "delivery-status": { code: "5.7.1", message: "  blocked — «spam»  ", description: "line one\nline two" },
    };
    const row = await stored(fields);
    expect(row).toMatchObject({
      recipient: fields.recipient,
      severity: fields.severity,
      message_id: fields.message.headers["message-id"],
      reason: "5.7.1 ·   blocked — «spam»   · line one\nline two",
    });
  });

  test("a second event with the same id changes nothing", async () => {
    const id = eventId("duplicate");
    expect((await send({ id, event: "accepted", recipient: "first@elsewhere.example", timestamp: 1700000000 })).body).toEqual(
      SUCCESS
    );
    const first = (await rowOf(id))!;
    const second = await send({ id, event: "delivered", recipient: "second@elsewhere.example", timestamp: 1800000000 });
    expect(second.status).toBe(200);
    expect(second.body).toEqual(SUCCESS);
    const after = (await rowOf(id))!;
    expect(after).toEqual(first);
    expect(after).toMatchObject({ event: "accepted", recipient: "first@elsewhere.example" });
    expect((await server.db.query("SELECT 1 FROM mailgun_events WHERE event_id = $1", [id])).length).toBe(1);
  });

  test("ids are compared exactly", async () => {
    const id = eventId("exact");
    for (const variant of [id, id.toUpperCase(), ` ${id}`, `${id} `]) {
      expect((await send({ id: variant, event: "delivered" })).body).toEqual(SUCCESS);
      expect((await rowOf(variant))?.event_id).toBe(variant);
    }
  });

  test("an event PostgreSQL cannot store is still answered with success, and leaves no row", async () => {
    const before = await rowCount();
    // A NUL character fits neither a text column nor JSONB.
    const inColumn = eventId("nul-column");
    expect((await send({ id: inColumn, event: "delivered", recipient: "a\u0000b@elsewhere.example" })).body).toEqual(SUCCESS);
    const inRaw = eventId("nul-raw");
    expect((await send({ id: inRaw, event: "delivered", note: "nul \u0000 inside" })).body).toEqual(SUCCESS);
    const inId = `${eventId("nul-id")}\u0000`;
    expect((await send({ id: inId, event: "delivered" })).body).toEqual(SUCCESS);
    expect(await rowOf(inColumn)).toBeUndefined();
    expect(await rowOf(inRaw)).toBeUndefined();
    expect(await rowCount()).toBe(before);
  });
});

describe("POST /api/mailgun-events — the reason, from delivery-status", () => {
  test.each([
    ["code, message and description", { code: 550, message: "No such user", description: "Mailbox not found" }, "550 · No such user · Mailbox not found"],
    ["a code alone", { code: 421 }, "421"],
    ["a message alone", { message: "Greylisted" }, "Greylisted"],
    ["a description alone", { description: "Timed out" }, "Timed out"],
    ["a code and a description", { code: 451, description: "Try later" }, "451 · Try later"],
    ["a string code", { code: "5.1.1", message: "Bad mailbox" }, "5.1.1 · Bad mailbox"],
    ["a zero code", { code: 0, message: "ok" }, "0 · ok"],
    ["a negative code", { code: -1 }, "-1"],
    ["a fractional code", { code: 5.5 }, "5.5"],
    ["a code that is true", { code: true }, "true"],
    ["a code that is false", { code: false }, "false"],
    ["a code that is an array", { code: [5, 7, 1] }, "5,7,1"],
    ["a code that is a nested array with gaps", { code: [1, [2, null], null, "x"] }, "1,2,,,x"],
    ["a code that is an object", { code: { smtp: 550 } }, "[object Object]"],
    ["a code that is an array of objects", { code: [{}, {}] }, "[object Object],[object Object]"],
    ["message and description that are numbers", { code: 550, message: 5, description: 6 }, "550"],
    ["message and description that are objects", { message: { text: "x" }, description: ["y"] }, null],
    ["a null code", { code: null, message: "No code" }, "No code"],
    ["an empty-string code", { code: "", message: "Empty code" }, "Empty code"],
    ["an empty-array code", { code: [], message: "Empty array" }, "Empty array"],
    ["parts that are only whitespace", { code: "  ", message: "\t\n", description: "   " }, null],
    ["an empty message between two parts", { code: 550, message: "", description: "Details" }, "550 · Details"],
    ["an empty object", {}, null],
    ["unrelated fields only", { "attempt-no": 3, tls: true }, null],
  ] as [string, Record<string, unknown>, string | null][])("%s", async (_label, status, reason) => {
    expect((await stored({ "delivery-status": status })).reason).toBe(reason);
  });

  test.each([
    ["missing", undefined],
    ["null", null],
    ["a string", "550 No such user"],
    ["a number", 550],
    ["true", true],
    ["an empty array", []],
    ["an array of parts", [550, "No such user"]],
  ])("no reason when delivery-status is %s", async (_label, status) => {
    expect((await stored({ "delivery-status": status })).reason).toBeNull();
  });

  test("numbers in the code print as JavaScript prints them", async () => {
    const cases: [string, string][] = [
      ["550.0", "550"],
      ["-0", "0"],
      ["1e21", "1e+21"],
      ["1e-7", "1e-7"],
      ["0.000001", "0.000001"],
      ["123456789012345678901", "123456789012345680000"],
      ["1.5e300", "1.5e+300"],
      ["1e400", "Infinity"],
      ["-1e400", "-Infinity"],
      ["[1e400, 2.50, -0]", "Infinity,2.5,0"],
    ];
    for (const [code, reason] of cases) {
      const id = eventId("code");
      const signature = JSON.stringify(mailgunSignature(KEY));
      const result = await server.client().request("POST", "/api/mailgun-events", {
        body: `{"signature":${signature},"event-data":{"id":"${id}","event":"delivered","delivery-status":{"code":${code}}}}`,
        headers: { "Content-Type": "application/json" },
      });
      expect(result.body).toEqual(SUCCESS);
      expect({ code, reason: (await rowOf(id))?.reason }).toEqual({ code, reason });
    }
  });
});

describe("POST /api/mailgun-events — when the event occurred", () => {
  test.each([
    ["whole seconds", 1700000000, "2023-11-14T22:13:20.000Z"],
    ["fractional seconds, cut to milliseconds", 1521472262.908181, "2018-03-19T15:11:02.908Z"],
    ["fractions are cut, not rounded", 1700000000.9999, "2023-11-14T22:13:20.999Z"],
    ["one second after the epoch", 1, "1970-01-01T00:00:01.000Z"],
    ["half a millisecond after the epoch", 0.0005, "1970-01-01T00:00:00.000Z"],
    ["the last second before 2100", 4102444799, "2099-12-31T23:59:59.000Z"],
    ["just under the limit", 4102444799.9995, "2099-12-31T23:59:59.999Z"],
  ])("%s", async (_label, timestamp, expected) => {
    expect((await stored({ timestamp })).occurred_at.toISOString()).toBe(expected);
  });

  test.each([
    ["missing", undefined],
    ["null", null],
    ["zero", 0],
    ["negative", -1700000000],
    ["2100 exactly", 4102444800],
    ["far in the future", 1e15],
    ["in milliseconds", 1700000000000],
    ["a numeric string", "1700000000"],
    ["an ISO string", "2023-11-14T22:13:20.000Z"],
    ["true", true],
    ["an array", [1700000000]],
    ["an object", { seconds: 1700000000 }],
  ])("a timestamp that is %s falls back to the time of arrival", async (_label, timestamp) => {
    expect(isNow((await stored({ timestamp })).occurred_at)).toBe(true);
  });

  test("a timestamp beyond what a number can hold falls back too", async () => {
    for (const literal of ["1e400", "-1e400", "1e309"]) {
      const id = eventId("big");
      const signature = JSON.stringify(mailgunSignature(KEY));
      const result = await server.client().request("POST", "/api/mailgun-events", {
        body: `{"signature":${signature},"event-data":{"id":"${id}","event":"delivered","timestamp":${literal}}}`,
        headers: { "Content-Type": "application/json" },
      });
      expect(result.body).toEqual(SUCCESS);
      const row = (await rowOf(id))!;
      expect(isNow(row.occurred_at)).toBe(true);
      // JSON has no infinity: the raw copy holds null.
      expect(row.raw).toEqual({ id, event: "delivered", timestamp: null });
    }
  });
});

describe("POST /api/mailgun-events — the raw copy", () => {
  const LIMIT = 16 * 1024;
  const REDACTED = { redacted: "event-data exceeded 16384 bytes" };

  /** Event data whose JSON.stringify is exactly `bytes` long in UTF-8. */
  const dataOfSize = (bytes: number, filler = "x", extra: Record<string, unknown> = {}) => {
    const data: Record<string, unknown> = { id: eventId("raw"), event: "delivered", ...extra, pad: "" };
    const room = bytes - Buffer.byteLength(JSON.stringify(data));
    const fillerBytes = Buffer.byteLength(filler);
    data.pad = filler.repeat(Math.floor(room / fillerBytes)) + "x".repeat(room % fillerBytes);
    expect(Buffer.byteLength(JSON.stringify(data))).toBe(bytes);
    return data as { id: string } & Record<string, unknown>;
  };

  test("is the event data as it was sent, whatever it holds", async () => {
    const data = {
      id: eventId("raw"),
      event: "opened",
      string: "text ünï 🔔 \"quoted\" back\\slash \n\t",
      numbers: [0, -1, 1.5, 1e21, 1e-7, 123456789, 0.1],
      booleans: [true, false],
      nothing: null,
      nested: { a: { b: { c: [[], {}, [[1]]] } } },
      "key with spaces": 1,
      "": "empty key",
      "ünï-kéy": "unicode key",
    };
    expect((await send(data)).body).toEqual(SUCCESS);
    expect((await rowOf(data.id))!.raw).toEqual(data);
  });

  test("keeps numbers as JSON.stringify writes them", async () => {
    const id = eventId("raw-numbers");
    const signature = JSON.stringify(mailgunSignature(KEY));
    const result = await server.client().request("POST", "/api/mailgun-events", {
      body:
        `{"signature":${signature},"event-data":{"id":"${id}","event":"delivered",` +
        `"a":1.0,"b":-0.0,"c":1e2,"d":1.50,"e":1E+21,"f":1e-7,"g":123456789012345678901,"h":0.1e1,"i":5e-1,"j":-0}}`,
      headers: { "Content-Type": "application/json" },
    });
    expect(result.body).toEqual(SUCCESS);
    expect((await rowOf(id))!.raw_text).toBe(
      `{"a": 1, "b": 0, "c": 100, "d": 1.5, "e": 1000000000000000000000, "f": 0.0000001, ` +
        `"g": 123456789012345680000, "h": 1, "i": 0.5, "j": 0, "id": "${id}", "event": "delivered"}`
    );
  });

  test("the last of a repeated key wins", async () => {
    const id = eventId("raw-repeat");
    const signature = JSON.stringify(mailgunSignature(KEY));
    const result = await server.client().request("POST", "/api/mailgun-events", {
      body:
        `{"signature":${signature},"event-data":{"id":"ignored","event":"opened","recipient":"first@elsewhere.example",` +
        `"id":"${id}","event":"delivered","recipient":"last@elsewhere.example"}}`,
      headers: { "Content-Type": "application/json" },
    });
    expect(result.body).toEqual(SUCCESS);
    const row = (await rowOf(id))!;
    expect(row).toMatchObject({ event: "delivered", recipient: "last@elsewhere.example" });
    expect(row.raw).toEqual({ id, event: "delivered", recipient: "last@elsewhere.example" });
    expect(await rowOf("ignored")).toBeUndefined();
  });

  test("is stored whole at exactly 16 KiB", async () => {
    const data = dataOfSize(LIMIT);
    expect((await send(data)).body).toEqual(SUCCESS);
    expect((await rowOf(data.id))!.raw).toEqual(data);
  });

  test("is replaced by a marker one byte over 16 KiB; the columns are still filled", async () => {
    const data = dataOfSize(LIMIT + 1, "x", {
      recipient: "bob@elsewhere.example",
      severity: "temporary",
      timestamp: 1700000000,
      "delivery-status": { code: 451, message: "Try later" },
      message: { headers: { "message-id": "kept@mail.test" } },
    });
    expect((await send(data)).body).toEqual(SUCCESS);
    const row = (await rowOf(data.id))!;
    expect(row.raw).toEqual(REDACTED);
    expect(row.raw_text).toBe('{"redacted": "event-data exceeded 16384 bytes"}');
    expect(row).toMatchObject({
      event: "delivered",
      recipient: "bob@elsewhere.example",
      severity: "temporary",
      reason: "451 · Try later",
      message_id: "kept@mail.test",
    });
    expect(row.occurred_at.toISOString()).toBe("2023-11-14T22:13:20.000Z");
  });

  test("the size is counted in UTF-8 bytes, not characters", async () => {
    const fits = dataOfSize(LIMIT, "é");
    expect(JSON.stringify(fits).length).toBeLessThan(LIMIT);
    expect((await send(fits)).body).toEqual(SUCCESS);
    expect((await rowOf(fits.id))!.raw).toEqual(fits);

    const over = dataOfSize(LIMIT + 1, "日");
    expect(JSON.stringify(over).length).toBeLessThan(LIMIT);
    expect((await send(over)).body).toEqual(SUCCESS);
    expect((await rowOf(over.id))!.raw).toEqual(REDACTED);
  });

  test("the size is that of the data written back as JSON, not of the bytes received", async () => {
    // Sent with escapes, spaces and long-hand numbers, the same data is far larger on the wire.
    const post = async (data: { id: string }) => {
      const padded = JSON.stringify(data, null, 8)
        .replace(/x/g, "\\u0078")
        .replace('"event"', '"number": 1.000e+2,\n"event"');
      expect(Buffer.byteLength(padded)).toBeGreaterThan(LIMIT * 4);
      const result = await server.client().request("POST", "/api/mailgun-events", {
        body: `{"signature":${JSON.stringify(mailgunSignature(KEY))},"event-data":${padded}}`,
        headers: { "Content-Type": "application/json" },
      });
      expect(result.body).toEqual(SUCCESS);
      return (await rowOf(data.id))!.raw;
    };
    // `"number":100,` is 13 bytes of the written-back JSON.
    const fits = dataOfSize(LIMIT - 13);
    expect(await post(fits)).toEqual({ ...fits, number: 100 });
    const over = dataOfSize(LIMIT - 12);
    expect(await post(over)).toEqual(REDACTED);
  });

  test("numbers count with the length JavaScript writes them in", async () => {
    // 1e21 is written "1e+21" and 1e-7 "1e-7": 9 bytes for the two.
    const numbers = '"big":1e21,"small":1e-7,';
    const post = async (data: { id: string }) => {
      const text = JSON.stringify(data).replace('"event"', `${numbers}"event"`);
      const result = await server.client().request("POST", "/api/mailgun-events", {
        body: `{"signature":${JSON.stringify(mailgunSignature(KEY))},"event-data":${text}}`,
        headers: { "Content-Type": "application/json" },
      });
      expect(result.body).toEqual(SUCCESS);
      return (await rowOf(data.id))!.raw;
    };
    const written = Buffer.byteLength(JSON.stringify({ big: 1e21, small: 1e-7 })) - 1;
    expect(written).toBe(25);
    const fits = dataOfSize(LIMIT - written);
    expect(await post(fits)).toEqual({ ...fits, big: 1e21, small: 1e-7 });
    const over = dataOfSize(LIMIT - written + 1);
    expect(await post(over)).toEqual(REDACTED);
  });
});

describe("POST /api/mailgun-events — paths and methods", () => {
  test("answers with a trailing slash, in any letter case and with a query string", async () => {
    for (const path of ["/api/mailgun-events/", "/API/MAILGUN-EVENTS", "/Api/Mailgun-Events/", "/api/mailgun-events?source=mailgun"]) {
      const id = eventId("path");
      const result = await server.client().post(path, { signature: mailgunSignature(KEY), "event-data": { id, event: "delivered" } });
      expect({ path, body: result.body }).toEqual({ path, body: SUCCESS });
      expect(await rowOf(id)).toBeDefined();
    }
  });

  test("is not answered for other methods or below the path", async () => {
    const client = server.client();
    for (const [method, path] of [
      ["GET", "/api/mailgun-events"],
      ["PUT", "/api/mailgun-events"],
      ["DELETE", "/api/mailgun-events"],
      ["PATCH", "/api/mailgun-events"],
      ["POST", "/api/mailgun-events/extra"],
      ["POST", "/api/mailgun-event"],
    ] as const) {
      const result = await client.request(method, path);
      expect({ method, path, status: result.status, body: result.body }).toEqual({ method, path, status: 404, body: NOT_FOUND });
    }
    expect((await client.head("/api/mailgun-events")).status).toBe(404);
  });

  test("needs no session and touches none", async () => {
    const result = await send({ id: eventId(), event: "delivered" });
    expect(result.setCookies).toEqual([]);
    expect(await server.db.query("SELECT 1 FROM sessions")).toEqual([]);
  });
});

describe("POST /api/mailgun-events — alarms", () => {
  let alarmed: TestServer;
  beforeAll(async () => {
    alarmed = await startServer({ production: true, env: { MAILGUN_WEBHOOK_SIGNING_KEY: KEY } });
  }, 90_000);
  afterAll(async () => {
    await alarmed?.stop();
  });

  /** A fresh process has sent no alarm yet; only the first of a kind per minute goes out. */
  const fresh = async () => {
    await alarmed.restart();
    alarmed.alarms.reset();
  };
  const alarms = () => alarmed.alarms.alarms.map(({ title, description }) => ({ title, description }));
  const sendTo = async (eventData: Record<string, unknown>) => {
    const result = await send({ id: eventId("alarm"), ...eventData }, alarmed);
    expect(result.body).toEqual(SUCCESS);
  };
  /** Sends a complaint last and waits for its alarm, so that anything sent before it has had its chance. */
  const settle = async () => {
    await sendTo({ event: "complained", recipient: "marker@elsewhere.example" });
    await waitFor(() => alarms().some((alarm) => alarm.description.includes("marker@elsewhere.example")), {
      what: "the marker alarm",
    });
  };
  const MARKER = { title: "🚨 Mail Server Error: Mailgun: complained", description: "**Event:** complained\n**Recipient:** marker@elsewhere.example" };

  test("a complaint raises an alarm with every field it has", async () => {
    await fresh();
    await sendTo({
      event: "complained",
      recipient: "bob@elsewhere.example",
      severity: "permanent",
      "delivery-status": { code: 550, message: "Reported as spam" },
      message: { headers: { "message-id": "complaint@mail.test" } },
    });
    await waitFor(() => alarmed.alarms.alarms.length > 0, { what: "the alarm" });
    const [alarm] = alarmed.alarms.alarms;
    expect(Object.keys(alarm!)).toEqual(["title", "description", "color", "timestamp"]);
    expect(alarm).toMatchObject({
      title: "🚨 Mail Server Error: Mailgun: complained",
      description:
        "**Event:** complained\n" +
        "**Recipient:** bob@elsewhere.example\n" +
        "**Severity:** permanent\n" +
        "**Reason:** 550 · Reported as spam\n" +
        "**Message-Id:** complaint@mail.test",
      color: 0xff0000,
    });
    expect(Math.abs(Date.parse(alarm!.timestamp!) - Date.now())).toBeLessThan(15_000);
  });

  test("a permanent_fail raises one, with only the lines it has values for", async () => {
    await fresh();
    await sendTo({ event: "permanent_fail", recipient: "", severity: "", message: { headers: { "message-id": "" } } });
    await waitFor(() => alarmed.alarms.alarms.length > 0, { what: "the alarm" });
    expect(alarms()).toEqual([
      { title: "🚨 Mail Server Error: Mailgun: permanent_fail", description: "**Event:** permanent_fail" },
    ]);
  });

  test("a failed event raises one only when its severity is exactly 'permanent'", async () => {
    await fresh();
    for (const severity of ["temporary", "Permanent", "PERMANENT", " permanent", "permanent ", "", undefined, null, 1, true, ["permanent"]]) {
      await sendTo({ event: "failed", severity });
    }
    await settle();
    expect(alarms()).toEqual([MARKER]);

    await sendTo({ event: "failed", severity: "permanent", recipient: "gone@elsewhere.example" });
    await waitFor(() => alarmed.alarms.alarms.length > 1, { what: "the failure alarm" });
    expect(alarms()[1]).toEqual({
      title: "🚨 Mail Server Error: Mailgun: failed",
      description: "**Event:** failed\n**Recipient:** gone@elsewhere.example\n**Severity:** permanent",
    });
  });

  test("no other event raises one, whatever its severity", async () => {
    await fresh();
    for (const event of ["accepted", "rejected", "delivered", "opened", "clicked", "unsubscribed", "stored", "temporary_fail"]) {
      await sendTo({ event, severity: "permanent", recipient: "quiet@elsewhere.example" });
    }
    await settle();
    expect(alarms()).toEqual([MARKER]);
  });

  test("refused events raise none", async () => {
    await fresh();
    expect((await send({ id: eventId(), event: "complained", recipient: "r".repeat(321) }, alarmed)).body).toEqual(MALFORMED);
    expect((await send({ event: "complained" }, alarmed)).body).toEqual(MALFORMED);
    const unsigned = await alarmed.client().post("/api/mailgun-events", {
      signature: mailgunSignature("wrong-key"),
      "event-data": { id: eventId(), event: "complained" },
    });
    expect(unsigned.body).toEqual(INVALID_SIGNATURE);
    await settle();
    expect(alarms()).toEqual([MARKER]);
  });

  test("each kind of event has its own minute: one alarm per kind, however many arrive", async () => {
    await fresh();
    for (let round = 0; round < 3; round++) {
      await sendTo({ event: "complained", recipient: `c${round}@elsewhere.example` });
      await sendTo({ event: "permanent_fail", recipient: `p${round}@elsewhere.example` });
      await sendTo({ event: "failed", severity: "permanent", recipient: `f${round}@elsewhere.example` });
    }
    await waitFor(() => alarmed.alarms.alarms.length >= 3, { what: "one alarm per kind" });
    // A report from the browser is a fourth kind, delivered before it is answered:
    // by now every alarm the events were going to raise has had its turn.
    await alarmed.client().post("/api/client-error", { message: "marker" });
    expect(alarms().map((alarm) => alarm.title).sort()).toEqual([
      "🚨 Mail Server Error: Client JS Error",
      "🚨 Mail Server Error: Mailgun: complained",
      "🚨 Mail Server Error: Mailgun: failed",
      "🚨 Mail Server Error: Mailgun: permanent_fail",
    ]);
    expect(alarms().find((alarm) => alarm.title.endsWith("complained"))!.description).toContain("c0@elsewhere.example");
  });

  test("a repeated event id still raises its alarm, though nothing new is stored", async () => {
    const id = eventId("alarm-duplicate");
    await fresh();
    expect((await send({ id, event: "complained", recipient: "first@elsewhere.example" }, alarmed)).body).toEqual(SUCCESS);
    await waitFor(() => alarmed.alarms.alarms.length > 0, { what: "the first alarm" });
    await fresh();
    expect((await send({ id, event: "complained", recipient: "again@elsewhere.example" }, alarmed)).body).toEqual(SUCCESS);
    await waitFor(() => alarmed.alarms.alarms.length > 0, { what: "the second alarm" });
    expect(alarms()[0]!.description).toBe("**Event:** complained\n**Recipient:** again@elsewhere.example");
    expect((await rowOf(id, alarmed))!.recipient).toBe("first@elsewhere.example");
  });

  test("an event that could not be stored still raises its alarm", async () => {
    await fresh();
    const id = eventId("alarm-unstorable");
    expect((await send({ id, event: "permanent_fail", recipient: "x@elsewhere.example", note: "nul \u0000" }, alarmed)).body).toEqual(
      SUCCESS
    );
    await waitFor(() => alarmed.alarms.alarms.length > 0, { what: "the alarm" });
    expect(alarms()).toEqual([
      {
        title: "🚨 Mail Server Error: Mailgun: permanent_fail",
        description: "**Event:** permanent_fail\n**Recipient:** x@elsewhere.example",
      },
    ]);
    expect(await rowOf(id, alarmed)).toBeUndefined();
  });
});
