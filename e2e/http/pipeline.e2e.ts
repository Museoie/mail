import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import crypto from "crypto";
import net from "net";
import zlib from "zlib";
import { ADMIN_PASSWORD, CLIENT_DIR, rawHttp, type ApiClient, type RawResponse, type TestServer } from "../harness";
import { startServer } from "./core-helpers";
import fs from "fs";
import path from "path";
import {
  ANONYMOUS_LOGIN_TEXT,
  BAD_REQUEST,
  HSTS,
  INTERNAL_ERROR,
  INVALID_BODY,
  JSON_TYPE,
  NOT_FOUND,
  NOT_LOGGED_IN,
  TEXT_TYPE,
  expectedSecurityHeaders,
  rawRequest,
  securityHeadersOf,
} from "./core-helpers";

const UNSUPPORTED = { status: "error", message: "Unsupported Media Type" };
const TOO_LARGE = { status: "error", message: "Payload Too Large" };
const TEN_MIB = 10 * 1024 * 1024;

/** A route with no rate limit that reports whether it saw the body it was sent. */
const PROBE = "/api/users/set-info";
const PROBE_BODY = '{"email":"someone@elsewhere.example"}';
/** The body was parsed as JSON and its `email` read. */
const READ = { status: "failed", message: "username is required and must be a string." };
/** The body was taken for `{}`: not JSON, or not read. */
const NOT_READ = { status: "failed", message: "email is required and must be a string." };

let dev: TestServer;
let prod: TestServer;

beforeAll(async () => {
  dev = await startServer();
  prod = await startServer({ production: true });
}, 120_000);

afterAll(async () => {
  await dev?.stop();
  await prod?.stop();
});

const raw = (method: string, target: string, headers: Record<string, string> = {}, body?: string | Uint8Array) =>
  rawHttp(dev.ports.http, rawRequest(method, target, headers, body));

const post = (
  body: string | Uint8Array,
  headers: Record<string, string> = {},
  target = PROBE,
  client: ApiClient = dev.client()
) => client.request("POST", target, { body: body as BodyInit, headers: { "Content-Type": "application/json", ...headers } });

const weakEtag = (body: string) => {
  const bytes = Buffer.from(body, "utf8");
  return `W/"${bytes.byteLength.toString(16)}-${crypto.createHash("sha1").update(bytes).digest("base64").slice(0, 27)}"`;
};

describe("security headers", () => {
  /** One request of each kind of response the server makes. */
  const everyKind = async (server: TestServer) => {
    const client = server.client();
    const signedIn = server.client();
    await signedIn.login("admin", ADMIN_PASSWORD);
    const index = fs.statSync(path.join(CLIENT_DIR, "index.html"));
    const responses: [string, number, Headers][] = [];
    const add = async (label: string, status: number, pending: Promise<{ status: number; headers: Headers }>) => {
      const result = await pending;
      expect({ label, status: result.status }).toEqual({ label, status });
      responses.push([label, result.status, result.headers]);
    };
    await add("an API answer", 200, client.get("/api/ping"));
    await add("an API answer to HEAD", 200, client.head("/api/ping"));
    await add("a failed login", 200, client.post("/api/users/login", { username: "admin", password: "wrong" }));
    await add("a login that sets a cookie", 200, server.client().post("/api/users/login", { username: "admin", password: ADMIN_PASSWORD }));
    await add("a signed-in answer", 200, signedIn.get("/api/users/login"));
    await add("an unknown API path", 404, client.get("/api/nothing"));
    await add("an API path that needs a session", 401, client.post("/api/push/subscribe", {}));
    await add("a body that is not JSON", 400, client.request("POST", "/api/users/login", { body: "{", headers: { "Content-Type": "application/json" } }));
    await add("a charset it cannot read", 415, client.request("POST", "/api/users/login", { body: "{}", headers: { "Content-Type": "application/json; charset=latin1" } }));
    await add("a body over the limit", 413, client.request("POST", "/api/users/login", { body: Buffer.alloc(TEN_MIB + 1, 0x20), headers: { "Content-Type": "application/json" } }));
    await add("a parameter it cannot decode", 400, client.get("/api/push/refresh/%ZZ"));
    await add("a not-modified API answer", 304, client.get("/api/ping", { headers: { "If-None-Match": "*" } }));
    await add("the entry document", 200, client.get("/"));
    await add("a file", 200, client.get("/manifest.json"));
    await add("the fallback for a client route", 200, client.get("/some/client/route"));
    await add("part of a file", 206, client.get("/manifest.json", { headers: { Range: "bytes=0-9" } }));
    await add("a range beyond a file", 416, client.get("/manifest.json", { headers: { Range: `bytes=${index.size + 100000}-` } }));
    await add("a not-modified file", 304, client.get("/manifest.json", { headers: { "If-None-Match": "*" } }));
    await add("the autoconfig document", 200, client.get("/.well-known/autoconfig/mail/config-v1.1.xml"));
    await add("a POST outside the API", 404, client.request("POST", "/"));
    await add("a body that is not JSON outside the API", 400, client.request("POST", "/x", { body: "{", headers: { "Content-Type": "application/json" } }));
    for (let i = 0; i < 5; i++) await client.post("/api/client-error", {});
    await add("a rate-limit refusal", 429, client.post("/api/client-error", {}));
    // set-info for an address that never asked for a token throws inside the route.
    await add("a server error", 500, server.client().post("/api/users/set-info", { email: "headers@elsewhere.example", username: "x", password: "x", token: "t" }));
    return responses;
  };

  test("are on every kind of response, without HSTS outside production", async () => {
    for (const [label, status, headers] of await everyKind(dev)) {
      expect({ label, status, headers: securityHeadersOf(headers) }).toEqual({
        label,
        status,
        headers: expectedSecurityHeaders(false),
      });
    }
  });

  test("are on every kind of response, with HSTS in production", async () => {
    for (const [label, status, headers] of await everyKind(prod)) {
      expect({ label, status, headers: securityHeadersOf(headers) }).toEqual({
        label,
        status,
        headers: expectedSecurityHeaders(true),
      });
    }
  });

  test("HSTS is sent in production however the request arrived", async () => {
    const plain = await prod.client({ forwarded: false }).get("/api/ping");
    expect(plain.headers.get("strict-transport-security")).toBe(HSTS);
    const http = await prod.client().get("/", { headers: { "X-Forwarded-Proto": "http" } });
    expect(http.headers.get("strict-transport-security")).toBe(HSTS);
    const bare = await rawHttp(prod.ports.http, rawRequest("GET", "/api/ping"));
    expect(bare.headers["strict-transport-security"]).toBe(HSTS);
  });

  test("a header of the same name is sent once", async () => {
    const response = await raw("GET", "/api/ping");
    const names = response.raw
      .slice(0, response.raw.indexOf("\r\n\r\n"))
      .split("\r\n")
      .slice(1)
      .map((line) => line.slice(0, line.indexOf(":")).toLowerCase());
    for (const name of ["content-security-policy", "x-content-type-options", "x-frame-options", "referrer-policy", "content-type", "etag", "content-length", "date"]) {
      expect({ name, count: names.filter((entry) => entry === name).length }).toEqual({ name, count: 1 });
    }
    expect(names).not.toContain("transfer-encoding");
    expect(names).not.toContain("strict-transport-security");
    expect(names).not.toContain("set-cookie");
  });
});

describe("a JSON request body", () => {
  test("is read when it is an object or an array", async () => {
    expect((await post(PROBE_BODY)).body).toEqual(READ);
    expect((await post("{}")).body).toEqual(NOT_READ);
    expect((await post("[]")).body).toEqual(INVALID_BODY);
    expect((await post(`[${PROBE_BODY}]`)).body).toEqual(INVALID_BODY);
  });

  test("may be surrounded by JSON whitespace", async () => {
    for (const body of [` ${PROBE_BODY} `, `\t\r\n ${PROBE_BODY}\n\n`, `\n\n\n${PROBE_BODY}`, PROBE_BODY.replace(":", " :\t")]) {
      const result = await post(body);
      expect({ body, status: result.status, answer: result.body }).toEqual({ body, status: 200, answer: READ });
    }
  });

  test.each([
    ["an unterminated object", "{"],
    ["an unterminated array", "["],
    ["a missing value", '{"a":}'],
    ["a missing colon", '{"a" 1}'],
    ["single quotes", "{'a':1}"],
    ["an unquoted key", "{a:1}"],
    ["a trailing comma in an object", '{"a":1,}'],
    ["a trailing comma in an array", "[1,]"],
    ["a leading comma", "[,1]"],
    ["text after the value", '{"a":1}x'],
    ["two values", '{"a":1}{"b":2}'],
    ["two values on two lines", "{}\n{}"],
    ["a comment after", '{"a":1}//x'],
    ["a comment before", "/* c */{}"],
    ["mismatched brackets", "{]"],
    ["a closing bracket alone", "]"],
    ["a number with a leading zero", '{"a":01}'],
    ["a number with a leading plus", '{"a":+1}'],
    ["a number without an integer part", '{"a":.5}'],
    ["a number without a fraction after the dot", '{"a":1.}'],
    ["a number with an empty exponent", '{"a":1e}'],
    ["a hex number", '{"a":0x10}'],
    ["NaN", '{"a":NaN}'],
    ["Infinity", '{"a":Infinity}'],
    ["a negative Infinity", '{"a":-Infinity}'],
    ["undefined", '{"a":undefined}'],
    ["a capitalised literal", '{"a":True}'],
    ["a truncated literal", '{"a":tru}'],
    ["an unknown escape", '{"a":"\\x41"}'],
    ["a short unicode escape", '{"a":"\\u12"}'],
    ["a unicode escape that is not hex", '{"a":"\\u12zz"}'],
    ["an unterminated string", '{"a":"b}'],
    ["a raw line break in a string", '{"a":"line\nbreak"}'],
    ["a raw tab in a string", '{"a":"tab\there"}'],
    ["a raw NUL in a string", '{"a":"nul\u0000"}'],
    ["a duplicate comma", '{"a":1,,"b":2}'],
    ["a non-string key", '{1:"a"}'],
    ["a key without a value at the end", '{"a"}'],
    ["a form feed before the value", "\f{}"],
    ["a vertical tab before the value", "\v{}"],
    ["a no-break space before the value", " {}"],
    ["a line separator before the value", " {}"],
    ["a byte order mark in the middle", '{"a":﻿1}'],
    ["form-encoded text", "email=someone%40elsewhere.example"],
    ["XML", "<email>someone@elsewhere.example</email>"],
  ])("is refused with 400 when it is malformed: %s", async (_label, body) => {
    const result = await post(body);
    expect(result.status).toBe(400);
    expect(result.text).toBe('{"status":"error","message":"Bad Request"}');
    expect(result.headers.get("content-type")).toBe(JSON_TYPE);
    expect(result.headers.get("etag")).toBe('W/"2a-W5zQRWqFCrvsbeTdYQNPYWgVTUM"');
    expect(result.setCookies).toEqual([]);
  });

  test.each([
    ["a string", '"text"'],
    ["a number", "42"],
    ["zero", "0"],
    ["a negative number", "-1"],
    ["a number with an exponent", "1e5"],
    ["true", "true"],
    ["false", "false"],
    ["null", "null"],
    ["a string after whitespace", '  "text"'],
    ["an empty string", '""'],
  ])("is refused with 400 when its top level is %s", async (_label, body) => {
    const result = await post(body);
    expect(result.status).toBe(400);
    expect(result.body).toEqual(BAD_REQUEST);
  });

  test("is refused with 400 when it is only whitespace", async () => {
    for (const body of [" ", "   ", "\n", "\r\n", "\t", " \n\t\r "]) {
      const result = await post(body);
      expect({ body, status: result.status, answer: result.body }).toEqual({ body, status: 400, answer: BAD_REQUEST });
    }
  });

  test("an empty body is an empty object", async () => {
    expect((await post("")).body).toEqual(NOT_READ);
    const declared = await raw("POST", PROBE, { "Content-Type": "application/json", "Content-Length": "0" });
    expect(JSON.parse(declared.body)).toEqual(NOT_READ);
    const none = await raw("POST", PROBE, { "Content-Type": "application/json" });
    expect(JSON.parse(none.body)).toEqual(NOT_READ);
  });

  test("a byte order mark at the start is skipped", async () => {
    expect((await post(`﻿${PROBE_BODY}`)).body).toEqual(READ);
    expect((await post(`﻿ \n${PROBE_BODY}`)).body).toEqual(READ);
    expect((await post("﻿")).body).toEqual(NOT_READ);
    // Anywhere else it is not whitespace.
    expect((await post(` ﻿${PROBE_BODY}`)).status).toBe(400);
    expect((await post(`${PROBE_BODY}﻿`)).status).toBe(400);
  });

  // NOTE: the decoder drops one mark and the parser's own clean-up a second,
  // so two in a row are skipped and only a third is an error.
  test("two byte order marks are skipped as well, three are not", async () => {
    expect((await post(`﻿﻿${PROBE_BODY}`)).body).toEqual(READ);
    expect((await post("﻿﻿")).body).toEqual(NOT_READ);
    expect((await post(`﻿﻿﻿${PROBE_BODY}`)).status).toBe(400);
    expect((await post("﻿﻿﻿")).status).toBe(400);
  });

  test("accepts what JSON.parse accepts beyond the ordinary", async () => {
    const accepted: [string, string, unknown][] = [
      ["every escape", '{"email":"\\u0061\\/\\b\\f\\n\\r\\t\\"\\\\@b.co"}', READ],
      ["an escaped surrogate pair", '{"email":"\\ud83d\\udd11@b.co"}', READ],
      ["a lone escaped surrogate", '{"email":"\\ud800@b.co"}', READ],
      ["a lone low surrogate", '{"email":"\\udc00@b.co"}', READ],
      ["an upper-case hex escape", '{"email":"\\u00E9@b.co"}', READ],
      ["a repeated key, the last one winning", '{"email":5,"email":"a@b.co"}', READ],
      ["a repeated key, the last one losing", '{"email":"a@b.co","email":5}', NOT_READ],
      ["a __proto__ key", '{"__proto__":{"email":"a@b.co"}}', NOT_READ],
      ["a constructor key", '{"constructor":{"prototype":{}},"email":"a@b.co"}', READ],
      ["a number beyond a double", '{"email":"a@b.co","n":1e999}', READ],
      ["a negative number beyond a double", '{"email":"a@b.co","n":-1e999}', READ],
      ["a number of five hundred digits", `{"email":"a@b.co","n":${"9".repeat(500)}}`, READ],
      ["a tiny number", '{"email":"a@b.co","n":1e-999}', READ],
      ["negative zero", '{"email":"a@b.co","n":-0}', READ],
      ["a number with an upper-case exponent", '{"email":"a@b.co","n":1E+2}', READ],
      ["an empty key", '{"":1,"email":"a@b.co"}', READ],
      ["raw unicode", '{"email":"ünï@b.co","日本":"語","🔑":1}', READ],
      ["a raw delete character", '{"email":"a\u007f@b.co"}', READ],
      ["raw line and paragraph separators", '{"email":"a  @b.co"}', READ],
    ];
    for (const [label, body, answer] of accepted) {
      const result = await post(body);
      expect({ label, status: result.status, answer: result.body }).toEqual({ label, status: 200, answer });
    }
  });

  test("bytes that are not UTF-8 are replaced, not refused", async () => {
    const body = Buffer.concat([Buffer.from('{"email":"a'), Buffer.from([0xff, 0xfe, 0xc3]), Buffer.from('@b.co"}')]);
    const result = await post(body);
    expect(result.status).toBe(200);
    expect(result.body).toEqual(READ);
  });

  test("nesting may be very deep", async () => {
    for (const depth of [100, 5_000, 100_000]) {
      const array = await post("[".repeat(depth) + "]".repeat(depth));
      expect({ depth, status: array.status, answer: array.body }).toEqual({ depth, status: 200, answer: INVALID_BODY });
      const object = await post(`{"email":"a@b.co","deep":${'{"a":'.repeat(depth)}1${"}".repeat(depth)}}`);
      expect({ depth, status: object.status, answer: object.body }).toEqual({ depth, status: 200, answer: READ });
      const mixed = await post(`{"deep":${'[{"a":'.repeat(depth)}null${"}]".repeat(depth)},"email":"a@b.co"}`);
      expect({ depth, status: mixed.status, answer: mixed.body }).toEqual({ depth, status: 200, answer: READ });
    }
  });

  test("deep nesting that does not close properly is still malformed", async () => {
    for (const body of ["[".repeat(50_000), "[".repeat(50_000) + "]".repeat(49_999), "[".repeat(50_000) + "}" + "]".repeat(49_999), `${"[".repeat(50_000)}${"]".repeat(50_000)}]`]) {
      const result = await post(body);
      expect(result.status).toBe(400);
      expect(result.body).toEqual(BAD_REQUEST);
    }
  });

  // NOTE: a NUL character reaches the database, which refuses it as text, and
  // the route fails instead of answering "not found".
  test("a NUL character in a looked-up field is a 500", async () => {
    const result = await prod.client().request("POST", "/api/users/login", {
      body: '{"username":"a\\u0000b","password":"x"}',
      headers: { "Content-Type": "application/json" },
    });
    expect(result.status).toBe(500);
    expect(result.body).toEqual(INTERNAL_ERROR);
  });
});

describe("which requests are read as JSON", () => {
  test.each([
    ["application/json"],
    ["Application/JSON"],
    ["APPLICATION/JSON; CHARSET=UTF-8"],
    ["application/json;charset=utf-8"],
    ["application/json; charset=UTF-8"],
    ['application/json; charset="utf-8"'],
    ["application/json ; charset=utf-8"],
    ["  application/json  "],
    ["application/json; boundary=x"],
    ["application/json;"],
    ["application/json; charset=utf-16"],
    ["application/json; charset=utf-32be"],
    ["application/json; charset=utf-7"],
    ["application/json; charset=UTF-anything"],
    ["application/json; charset="],
    ["application/json; x-charset=latin1"],
    ["application/json; charset=utf-8; charset=latin1"],
    ["application/json; notcharset=latin1"],
  ])("Content-Type %p: yes", async (type) => {
    const result = await post(PROBE_BODY, { "Content-Type": type });
    expect({ type, status: result.status, answer: result.body }).toEqual({ type, status: 200, answer: READ });
  });

  test.each([
    ["text/plain"],
    ["text/json"],
    ["text/plain;charset=UTF-8"],
    ["application/jsonx"],
    ["application/x-json"],
    ["application/vnd.api+json"],
    ["application/ld+json"],
    ["application/json+x"],
    ["application/x-www-form-urlencoded"],
    ["multipart/form-data; boundary=x"],
    ["application/octet-stream"],
    ["json"],
    ["application/json, text/plain"],
    ["application/json charset=utf-8"],
    ["application / json"],
    ["*/*"],
  ])("Content-Type %p: no, the body is not read at all", async (type) => {
    const result = await post(PROBE_BODY, { "Content-Type": type });
    expect({ type, status: result.status, answer: result.body }).toEqual({ type, status: 200, answer: NOT_READ });
    // Nor checked: what would be malformed JSON passes.
    const malformed = await post("{", { "Content-Type": type });
    expect({ type, status: malformed.status, answer: malformed.body }).toEqual({ type, status: 200, answer: NOT_READ });
  });

  test("without a Content-Type the body is not read", async () => {
    const response = await raw("POST", PROBE, {}, PROBE_BODY);
    expect(JSON.parse(response.body)).toEqual(NOT_READ);
  });

  test("a repeated Content-Type header is one value, which is not JSON", async () => {
    for (const types of [
      ["text/plain", "application/json"],
      ["application/json", "text/plain"],
      ["application/json", "application/json"],
    ]) {
      const head =
        `POST ${PROBE} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 1\r\n` +
        types.map((type) => `Content-Type: ${type}\r\n`).join("") +
        "\r\n{";
      const response = await rawHttp(dev.ports.http, head);
      expect({ types, status: response.status, answer: JSON.parse(response.body) }).toEqual({
        types,
        status: 200,
        answer: NOT_READ,
      });
    }
  });

  test.each([
    ["application/json; charset=latin1"],
    ["application/json; charset=iso-8859-1"],
    ["application/json; charset=ISO-8859-1"],
    ["application/json;charset=windows-1252"],
    ["application/json; charset=us-ascii"],
    ["application/json; charset=utf8"],
    ["application/json; charset=UTF8"],
    ["application/json; charset=unicode"],
    ['application/json; charset="latin1"'],
    ["application/json; charset = latin1"],
    ["application/json;\tcharset=latin1"],
    ["application/json; CHARSET=LATIN1"],
    ["application/json; charset=latin1; charset=utf-8"],
    ['application/json; note="; charset=latin1"'],
    ["application/json; a=b;charset=shift_jis"],
  ])("Content-Type %p: refused with 415", async (type) => {
    const result = await post(PROBE_BODY, { "Content-Type": type });
    expect(result.status).toBe(415);
    expect(result.text).toBe('{"status":"error","message":"Unsupported Media Type"}');
    expect(result.headers.get("content-type")).toBe(JSON_TYPE);
  });

  test("the charset is not looked at when there is no body", async () => {
    const response = await raw("POST", PROBE, { "Content-Type": "application/json; charset=latin1", "Content-Length": "0" });
    expect(response.status).toBe(200);
    expect(JSON.parse(response.body)).toEqual(NOT_READ);
  });

  test("the body of GET and HEAD is ignored, whatever it claims to be", async () => {
    const get = await raw("GET", "/api/users/login", { "Content-Type": "application/json" }, "{");
    expect(get.status).toBe(200);
    expect(get.body).toBe(ANONYMOUS_LOGIN_TEXT);
    const head = await raw("HEAD", "/api/users/login", { "Content-Type": "application/json; charset=latin1" }, "{");
    expect(head.status).toBe(200);
    const page = await raw("GET", "/", { "Content-Type": "application/json", "Content-Encoding": "br" }, "{");
    expect(page.status).toBe(200);
  });

  test("the body of every other method is read", async () => {
    for (const method of ["POST", "PUT", "PATCH", "DELETE", "OPTIONS"]) {
      const malformed = await raw(method, "/api/users/login", { "Content-Type": "application/json" }, "{");
      expect({ method, status: malformed.status, body: JSON.parse(malformed.body) }).toEqual({
        method,
        status: 400,
        body: BAD_REQUEST,
      });
    }
    const fine = await raw("DELETE", "/api/users/login", { "Content-Type": "application/json" }, '{"any":"thing"}');
    expect(fine.status).toBe(200);
    expect(fine.body).toBe('{"status":"success"}');
  });

  test("a chunked body is read like any other", async () => {
    const chunks = ['{"email":', '"someone@', 'elsewhere.example"}'];
    const chunked = chunks.map((chunk) => `${chunk.length.toString(16)}\r\n${chunk}\r\n`).join("") + "0\r\n\r\n";
    const response = await rawHttp(
      dev.ports.http,
      `POST ${PROBE} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n${chunked}`
    );
    expect(response.status).toBe(200);
    expect(JSON.parse(response.body)).toEqual(READ);

    const malformed = await rawHttp(
      dev.ports.http,
      `POST ${PROBE} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n1\r\n{\r\n0\r\n\r\n`
    );
    expect(malformed.status).toBe(400);
    const empty = await rawHttp(
      dev.ports.http,
      `POST ${PROBE} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n`
    );
    expect(empty.status).toBe(200);
    expect(JSON.parse(empty.body)).toEqual(NOT_READ);
  });
});

describe("a compressed JSON body", () => {
  const gzip = zlib.gzipSync(PROBE_BODY);
  const deflate = zlib.deflateSync(PROBE_BODY);

  test("gzip and deflate are undone", async () => {
    for (const [encoding, body] of [
      ["gzip", gzip],
      ["GZIP", gzip],
      ["Gzip", gzip],
      ["deflate", deflate],
      ["DEFLATE", deflate],
      ["gzip", zlib.gzipSync(PROBE_BODY, { level: 0 })],
      ["gzip", zlib.gzipSync(PROBE_BODY, { level: 9 })],
      ["deflate", zlib.deflateSync(PROBE_BODY, { level: 0 })],
    ] as const) {
      const result = await post(body, { "Content-Encoding": encoding });
      expect({ encoding, status: result.status, answer: result.body }).toEqual({ encoding, status: 200, answer: READ });
    }
  });

  test("identity is no encoding", async () => {
    for (const encoding of ["identity", "IDENTITY", "Identity"]) {
      expect((await post(PROBE_BODY, { "Content-Encoding": encoding })).body).toEqual(READ);
    }
  });

  test.each([["br"], ["zstd"], ["compress"], ["x-gzip"], ["x-deflate"], ["gzip, deflate"], ["gzip,gzip"], ["identity, gzip"], ["deflate-raw"], ["none"], ["gzip;q=1"], ["utf-8"]])(
    "Content-Encoding %p is refused with 415",
    async (encoding) => {
      const result = await post(gzip, { "Content-Encoding": encoding });
      expect(result.status).toBe(415);
      expect(result.body).toEqual(UNSUPPORTED);
    }
  );

  test("a repeated Content-Encoding header is one value, which is unknown", async () => {
    const head =
      `POST ${PROBE} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\n` +
      `Content-Encoding: identity\r\nContent-Encoding: identity\r\nContent-Length: ${PROBE_BODY.length}\r\n\r\n${PROBE_BODY}`;
    const response = await rawHttp(dev.ports.http, head);
    expect(response.status).toBe(415);
  });

  test("the encoding only matters for a body that is read", async () => {
    const notJson = await post(PROBE_BODY, { "Content-Type": "text/plain", "Content-Encoding": "br" });
    expect(notJson.body).toEqual(NOT_READ);
    const empty = await raw("POST", PROBE, { "Content-Type": "application/json", "Content-Encoding": "br", "Content-Length": "0" });
    expect(empty.status).toBe(200);
    expect(JSON.parse(empty.body)).toEqual(NOT_READ);
  });

  test("an empty compressed body is an empty object", async () => {
    expect((await post(zlib.gzipSync(""), { "Content-Encoding": "gzip" })).body).toEqual(NOT_READ);
    expect((await post(zlib.deflateSync(""), { "Content-Encoding": "deflate" })).body).toEqual(NOT_READ);
  });

  test("several gzip members in a row are one body", async () => {
    const half = Math.floor(PROBE_BODY.length / 2);
    const members = Buffer.concat([zlib.gzipSync(PROBE_BODY.slice(0, half)), zlib.gzipSync(PROBE_BODY.slice(half))]);
    expect((await post(members, { "Content-Encoding": "gzip" })).body).toEqual(READ);
    const withEmpty = Buffer.concat([zlib.gzipSync(""), gzip, zlib.gzipSync("")]);
    expect((await post(withEmpty, { "Content-Encoding": "gzip" })).body).toEqual(READ);
  });

  test.each([
    ["plain text sent as gzip", "gzip", () => Buffer.from(PROBE_BODY)],
    ["plain text sent as deflate", "deflate", () => Buffer.from(PROBE_BODY)],
    ["deflate without its zlib wrapper", "deflate", () => zlib.deflateRawSync(PROBE_BODY)],
    ["gzip sent as deflate", "deflate", () => gzip],
    ["deflate sent as gzip", "gzip", () => deflate],
    ["gzip cut short", "gzip", () => gzip.subarray(0, gzip.length - 5)],
    ["gzip cut in its header", "gzip", () => gzip.subarray(0, 6)],
    ["deflate cut short", "deflate", () => deflate.subarray(0, deflate.length - 3)],
    ["gzip followed by other bytes", "gzip", () => Buffer.concat([gzip, Buffer.from("trailing")])],
    ["deflate followed by other bytes", "deflate", () => Buffer.concat([deflate, Buffer.from("zz")])],
    ["gzip with a damaged checksum", "gzip", () => {
      const damaged = Buffer.from(gzip);
      damaged[damaged.length - 6] ^= 0xff;
      return damaged;
    }],
    ["gzip with a damaged length", "gzip", () => {
      const damaged = Buffer.from(gzip);
      damaged[damaged.length - 1] ^= 0x01;
      return damaged;
    }],
    ["deflate with a damaged checksum", "deflate", () => {
      const damaged = Buffer.from(deflate);
      damaged[damaged.length - 1] ^= 0xff;
      return damaged;
    }],
    ["compressed text that is not JSON", "gzip", () => zlib.gzipSync("{")],
    ["a compressed scalar", "deflate", () => zlib.deflateSync("42")],
    ["brotli sent as gzip", "gzip", () => zlib.brotliCompressSync(PROBE_BODY)],
  ] as [string, string, () => Buffer][])("is refused with 400: %s", async (_label, encoding, body) => {
    const result = await post(body(), { "Content-Encoding": encoding });
    expect(result.status).toBe(400);
    expect(result.body).toEqual(BAD_REQUEST);
  });

  test("is limited to 10 MiB after decompression", async () => {
    const spaces = (bytes: number) => Buffer.concat([Buffer.from(PROBE_BODY), Buffer.alloc(bytes - PROBE_BODY.length, 0x20)]);
    const fits = await post(zlib.gzipSync(spaces(TEN_MIB)), { "Content-Encoding": "gzip" });
    expect({ status: fits.status, answer: fits.body }).toEqual({ status: 200, answer: READ });
    const over = await post(zlib.gzipSync(spaces(TEN_MIB + 1)), { "Content-Encoding": "gzip" });
    expect({ status: over.status, answer: over.body }).toEqual({ status: 413, answer: TOO_LARGE });
    const bomb = await post(zlib.deflateSync(Buffer.alloc(64 * 1024 * 1024, 0x20)), { "Content-Encoding": "deflate" });
    expect({ status: bomb.status, answer: bomb.body }).toEqual({ status: 413, answer: TOO_LARGE });
  });
});

describe("the 10 MiB limit on a JSON body", () => {
  const padded = (bytes: number) => Buffer.concat([Buffer.from(PROBE_BODY), Buffer.alloc(bytes - PROBE_BODY.length, 0x20)]);

  test("a body of exactly 10 MiB is read", async () => {
    const result = await post(padded(TEN_MIB));
    expect({ status: result.status, answer: result.body }).toEqual({ status: 200, answer: READ });
  });

  test("one byte more is refused with 413", async () => {
    const result = await post(padded(TEN_MIB + 1));
    expect(result.status).toBe(413);
    expect(result.text).toBe('{"status":"error","message":"Payload Too Large"}');
    expect(result.headers.get("content-type")).toBe(JSON_TYPE);
  });

  test("a declared length over the limit is refused before the body arrives", async () => {
    const response = await rawHttp(
      dev.ports.http,
      `POST ${PROBE} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: ${TEN_MIB + 1}\r\n\r\n{}`
    );
    expect(response.status).toBe(413);
    expect(JSON.parse(response.body)).toEqual(TOO_LARGE);
  });

  test("a chunked body over the limit is refused as it passes it", async () => {
    const response = await new Promise<RawResponse>((resolve, reject) => {
      const socket = net.createConnection({ port: dev.ports.http, host: "127.0.0.1" });
      const received: Buffer[] = [];
      socket.on("error", () => undefined);
      socket.on("data", (chunk) => received.push(Buffer.from(chunk)));
      socket.on("close", () => {
        const text = Buffer.concat(received).toString("latin1");
        const match = /^HTTP\/1\.1 (\d{3})/.exec(text);
        if (!match) return reject(new Error(`no response: ${text.slice(0, 100)}`));
        resolve({ status: Number(match[1]), statusText: "", headers: {}, body: text.slice(text.indexOf("\r\n\r\n") + 4), raw: text });
      });
      socket.on("connect", () => {
        socket.write(`POST ${PROBE} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n`);
        const megabyte = Buffer.alloc(1024 * 1024, 0x20);
        for (let i = 0; i < 11; i++) {
          socket.write(`${megabyte.length.toString(16)}\r\n`);
          socket.write(megabyte);
          socket.write("\r\n");
        }
        socket.write("0\r\n\r\n");
      });
      setTimeout(() => socket.destroy(), 15_000);
    });
    expect(response.status).toBe(413);
    expect(response.body).toContain('"Payload Too Large"');
  });

  test("the limit is checked before the content encoding, the charset before the limit", async () => {
    const encoded = await rawHttp(
      dev.ports.http,
      `POST ${PROBE} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Encoding: br\r\nContent-Length: ${TEN_MIB + 1}\r\n\r\n{}`
    );
    expect(encoded.status).toBe(413);
    const charset = await rawHttp(
      dev.ports.http,
      `POST ${PROBE} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json; charset=latin1\r\nContent-Length: ${TEN_MIB + 1}\r\n\r\n{}`
    );
    expect(charset.status).toBe(415);
  });

  test("a body that is not JSON is not measured", async () => {
    const result = await post(Buffer.alloc(TEN_MIB + 4096, 0x61), { "Content-Type": "text/plain" });
    expect({ status: result.status, answer: result.body }).toEqual({ status: 200, answer: NOT_READ });
  });
});

describe("body errors outside the API are answered in plain text", () => {
  test.each([
    ["a malformed body", "{", { "Content-Type": "application/json" }, 400, "Bad Request", 'W/"b-EFiDB1U+dmqzx9Mo2UjcZ1SJPO8"'],
    ["a scalar body", "42", { "Content-Type": "application/json" }, 400, "Bad Request", 'W/"b-EFiDB1U+dmqzx9Mo2UjcZ1SJPO8"'],
    ["another charset", "{}", { "Content-Type": "application/json; charset=latin1" }, 415, "Unsupported Media Type", undefined],
    ["an unknown encoding", "{}", { "Content-Type": "application/json", "Content-Encoding": "br" }, 415, "Unsupported Media Type", undefined],
  ] as [string, string, Record<string, string>, number, string, string | undefined][])(
    "%s",
    async (_label, body, headers, status, text, etag) => {
      for (const target of ["/", "/somewhere/else", "/index.html", "/apix", "/api-docs", "/.well-known/autoconfig/mail/config-v1.1.xml"]) {
        const result = await post(body, headers, target);
        expect({ target, status: result.status, text: result.text }).toEqual({ target, status, text });
        expect(result.headers.get("content-type")).toBe(TEXT_TYPE);
        expect(result.headers.get("etag")).toBe(etag ?? weakEtag(text));
        expect(securityHeadersOf(result.headers)).toEqual(expectedSecurityHeaders(false));
      }
    }
  );

  test("a body over the limit", async () => {
    const result = await post(Buffer.alloc(TEN_MIB + 1, 0x20), {}, "/somewhere/else");
    expect(result.status).toBe(413);
    expect(result.text).toBe("Payload Too Large");
    expect(result.headers.get("content-type")).toBe(TEXT_TYPE);
  });

  // NOTE: routing treats /API like /api, but the error handler decides JSON or
  // text by the exact lower-case prefix, so an upper-case API path gets text.
  test("an API path in another letter case counts as outside the API here", async () => {
    for (const target of ["/API/users/login", "/Api/ping", "/aPi"]) {
      const result = await post("{", {}, target);
      expect({ target, status: result.status, text: result.text }).toEqual({ target, status: 400, text: "Bad Request" });
      expect(result.headers.get("content-type")).toBe(TEXT_TYPE);
    }
    for (const target of ["/api", "/api/", "/api/users/login", "/api/nothing", "/api?x=1", "/api/PING"]) {
      const result = await post("{", {}, target);
      expect({ target, status: result.status, body: result.body }).toEqual({ target, status: 400, body: BAD_REQUEST });
    }
  });
});

describe("a body error is answered before anything else is looked at", () => {
  test("before routing: an unknown API path is still a 400", async () => {
    expect((await post("{", {}, "/api/nothing/here")).status).toBe(400);
    expect((await post("{}", {}, "/api/nothing/here")).status).toBe(404);
  });

  test("before the session gate", async () => {
    expect((await post("{", {}, "/api/push/subscribe")).status).toBe(400);
    expect((await post("{}", {}, "/api/push/subscribe")).body).toEqual(NOT_LOGGED_IN);
  });

  test("before the method is matched", async () => {
    const response = await raw("PUT", "/api/ping", { "Content-Type": "application/json" }, "{");
    expect(response.status).toBe(400);
  });
});

describe("unknown API paths", () => {
  const NOT_FOUND_TEXT = '{"status":"failed","message":"Not found"}';

  test.each(["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "TRACE", "PROPFIND", "QUERY", "LINK"])(
    "%s is answered with the JSON 404",
    async (method) => {
      for (const target of ["/api/nothing", "/api/users/nothing", "/api/users", "/api/nothing/at/all?x=1"]) {
        const response = await raw(method, target);
        expect({ method, target, status: response.status, body: response.body }).toEqual({
          method,
          target,
          status: 404,
          body: NOT_FOUND_TEXT,
        });
        expect(response.headers["content-type"]).toBe(JSON_TYPE);
        expect(response.headers.etag).toBe('W/"29-bqFQf04cP+qEAj/lqMkazPSGU3k"');
        expect(response.headers["content-length"]).toBe("41");
        expect(securityHeadersOf(response.headers)).toEqual(expectedSecurityHeaders(false));
      }
    }
  );

  test("HEAD gets the same headers and no body", async () => {
    const response = await raw("HEAD", "/api/nothing");
    expect(response.status).toBe(404);
    expect(response.body).toBe("");
    expect(response.headers["content-type"]).toBe(JSON_TYPE);
    expect(response.headers["content-length"]).toBe("41");
    expect(response.headers.etag).toBe('W/"29-bqFQf04cP+qEAj/lqMkazPSGU3k"');
  });

  test("the API root itself is not a route", async () => {
    for (const target of ["/api", "/api/", "/API", "/Api/", "/api?x=1", "/api/?", "/api//", "/api/%20"]) {
      for (const method of ["GET", "POST", "DELETE"]) {
        const response = await raw(method, target);
        expect({ method, target, status: response.status, body: response.body }).toEqual({
          method,
          target,
          status: 404,
          body: NOT_FOUND_TEXT,
        });
      }
    }
  });

  test("a 404 is not turned into a 304", async () => {
    const response = await raw("GET", "/api/nothing", { "If-None-Match": 'W/"29-bqFQf04cP+qEAj/lqMkazPSGU3k"' });
    expect(response.status).toBe(404);
    expect(response.body).toBe(NOT_FOUND_TEXT);
    const any = await raw("GET", "/api/nothing", { "If-None-Match": "*" });
    expect(any.status).toBe(404);
  });

  test("a path that only starts with the letters 'api' is not the API", async () => {
    for (const target of ["/apix", "/api-v2/ping", "/apiping", "/api.json", "/ap", "/a/api/ping"]) {
      const get = await raw("GET", target);
      expect({ target, status: get.status, type: get.headers["content-type"] }).toEqual({
        target,
        status: 200,
        type: "text/html;charset=utf-8",
      });
      const posted = await raw("POST", target);
      expect({ target, status: posted.status, body: posted.body }).toEqual({ target, status: 404, body: "Not Found" });
    }
  });
});

describe("how the request target is read", () => {
  const PING = '{"status":"success"}';

  test.each([
    ["/api/users/../ping"],
    ["/api/users/../users/../ping"],
    ["/api/./ping"],
    ["/api/x/./../ping"],
    ["/x/../api/ping"],
    ["/../api/ping"],
    ["/../../../api/ping"],
    ["/api/x/%2e%2e/ping"],
    ["/api/x/%2E%2E/ping"],
    ["/api/x/.%2e/ping"],
    ["/api/%2e/ping"],
    ["/%2e%2e/api/ping"],
    ["/api/ping/x/.."],
    ["/api/ping/."],
    ["/api\\ping"],
    ["/api/x\\..\\ping"],
    ["/api/ping?x=/../../y"],
    ["/api/ping?"],
    ["/api/ping?a=1&a=2&b"],
    ["http://elsewhere.example/api/ping"],
    ["http://elsewhere.example:8080/api/x/../ping?q#"],
    ["HTTP://ELSEWHERE.example/api/ping"],
    ["/API/PING"],
    ["/Api/Ping/"],
  ])("%s reaches the ping route", async (target) => {
    const response = await raw("GET", target);
    expect({ target, status: response.status, body: response.body }).toEqual({ target, status: 200, body: PING });
  });

  test.each([
    ["a doubled slash after the mount", "/api//ping"],
    ["a doubled slash in a route", "/api/users//login"],
    ["two trailing slashes", "/api/ping//"],
    ["an encoded letter", "/api/%70ing"],
    ["an encoded slash inside the route", "/api/users%2Flogin"],
    ["a trailing space", "/api/ping%20"],
    ["a matrix parameter", "/api/ping;v=1"],
    ["a longer path", "/api/ping/extra"],
    ["a trailing dot", "/api/ping."],
    ["an encoded trailing slash", "/api/ping%2F"],
  ])("%s is another path: the JSON 404", async (_label, target) => {
    const response = await raw("GET", target);
    expect({ target, status: response.status, body: JSON.parse(response.body) }).toEqual({ target, status: 404, body: NOT_FOUND });
  });

  test("a doubled slash in front of the API mount is not the API at all", async () => {
    const get = await raw("GET", "//api/ping");
    expect(get.status).toBe(200);
    expect(get.headers["content-type"]).toBe("text/html;charset=utf-8");
    const posted = await raw("POST", "//api/ping");
    expect({ status: posted.status, body: posted.body }).toEqual({ status: 404, body: "Not Found" });
    // Nor is a path whose mount is spelled with an escape, or joined to the rest by one.
    for (const target of ["/%61pi/ping", "/api%2Fping", "/api%2fusers%2flogin", "/%2Fapi/ping"]) {
      const encoded = await raw("GET", target);
      expect({ target, status: encoded.status, type: encoded.headers["content-type"] }).toEqual({
        target,
        status: 200,
        type: "text/html;charset=utf-8",
      });
    }
  });

  test("a route parameter that cannot be decoded is a 400", async () => {
    for (const target of ["/api/push/refresh/%E0%A4%A", "/api/push/refresh/%", "/api/push/refresh/%zz", "/api/push/refresh/%C3", "/api/push/refresh/abc%FFdef", "/api/push/refresh/%u0041"]) {
      const response = await raw("GET", target);
      expect({ target, status: response.status, body: JSON.parse(response.body) }).toEqual({
        target,
        status: 400,
        body: BAD_REQUEST,
      });
      expect(response.headers["content-type"]).toBe(JSON_TYPE);
    }
    // One that can be decoded reaches the route.
    const fine = await raw("GET", "/api/push/refresh/%E2%9C%93%20%2F%3F");
    expect(fine.status).toBe(200);
    expect(JSON.parse(fine.body)).toEqual({ status: "failed", message: "No subscription found" });
  });
});

describe("HTTP details", () => {
  test("an HTTP/1.1 request without a Host header is refused with 400", async () => {
    const response = await rawHttp(dev.ports.http, "GET /api/ping HTTP/1.1\r\nConnection: close\r\n\r\n");
    expect(response.status).toBe(400);
    const posted = await rawHttp(dev.ports.http, "POST /api/users/login HTTP/1.1\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
    expect(posted.status).toBe(400);
  });

  test("an HTTP/1.0 request needs none, and an empty or repeated one will do", async () => {
    const old = await rawHttp(dev.ports.http, "GET /api/ping HTTP/1.0\r\n\r\n");
    expect(old.status).toBe(200);
    expect(old.body).toBe('{"status":"success"}');
    const empty = await rawHttp(dev.ports.http, "GET /api/ping HTTP/1.1\r\nHost:\r\nConnection: close\r\n\r\n");
    expect(empty.status).toBe(200);
    const twice = await rawHttp(dev.ports.http, "GET /api/ping HTTP/1.1\r\nHost: a.example\r\nHost: b.example\r\nConnection: close\r\n\r\n");
    expect(twice.status).toBe(200);
  });

  test("the Host header plays no part in routing", async () => {
    for (const host of ["mail.test", "evil.example", "localhost:1", "[::1]", "a b", "*"]) {
      const response = await rawHttp(dev.ports.http, `GET /api/ping HTTP/1.1\r\nHost: ${host}\r\nConnection: close\r\n\r\n`);
      expect({ host, status: response.status }).toEqual({ host, status: 200 });
    }
  });

  test("several requests on one connection are answered in order", async () => {
    const response = await rawHttp(
      dev.ports.http,
      "GET /api/ping HTTP/1.1\r\nHost: localhost\r\n\r\n" +
        "GET /api/nothing HTTP/1.1\r\nHost: localhost\r\n\r\n" +
        "POST /api/users/set-info HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}" +
        "GET /api/ping HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    );
    const statuses = [...response.raw.matchAll(/HTTP\/1\.1 (\d{3})/g)].map((match) => Number(match[1]));
    expect(statuses).toEqual([200, 404, 200, 200]);
    expect(response.raw.endsWith('{"status":"success"}')).toBe(true);
  });

  test.each(["ACL", "BIND", "CHECKOUT", "COPY", "LOCK", "M-SEARCH", "MERGE", "MKACTIVITY", "MKCALENDAR", "MKCOL", "MOVE", "NOTIFY", "PROPPATCH", "PURGE", "REBIND", "REPORT", "SEARCH", "SOURCE", "SUBSCRIBE", "UNBIND", "UNLINK", "UNLOCK", "UNSUBSCRIBE", "CONNECT"])(
    "the uncommon method %s is answered like any method without a route",
    async (method) => {
      const api = await raw(method, "/api/ping");
      expect({ method, status: api.status, body: api.body }).toEqual({
        method,
        status: 404,
        body: '{"status":"failed","message":"Not found"}',
      });
      const page = await raw(method, "/");
      expect({ method, status: page.status, body: page.body }).toEqual({ method, status: 404, body: "Not Found" });
    }
  );

  test.each(["FOO", "get", "Get", "GETX", "PATCHY", "BREW", "TRACK", "DEBUG", "X-CUSTOM", "UPDATE", "DESCRIBE"])(
    "a method the server does not know (%s) gets no answer: the connection is closed",
    async (method) => {
      const response = await rawHttp(dev.ports.http, `${method} /api/ping HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n`);
      expect({ method, status: response.status, raw: response.raw }).toEqual({ method, status: 0, raw: "" });
      // The server is none the worse for it.
      expect((await raw("GET", "/api/ping")).status).toBe(200);
    }
  );

  test("JSON answers carry their length and are not chunked", async () => {
    for (const [method, target] of [
      ["GET", "/api/ping"],
      ["GET", "/api/users/login"],
      ["GET", "/api/nothing"],
      ["POST", "/api/users/set-info"],
      ["POST", "/"],
    ]) {
      const response = await raw(method!, target!);
      expect(response.headers["content-length"]).toBe(String(Buffer.byteLength(response.body)));
      expect(response.headers["transfer-encoding"]).toBeUndefined();
      expect(response.headers.date).toMatch(/^[A-Z][a-z]{2}, \d{2} [A-Z][a-z]{2} \d{4} \d{2}:\d{2}:\d{2} GMT$/);
    }
  });
});

describe("validators on API answers", () => {
  test("the ETag is a weak hash of the exact body", async () => {
    const client = dev.client();
    const cases: [string, string, unknown?][] = [
      ["GET", "/api/ping"],
      ["GET", "/api/users/login"],
      ["GET", "/api/push/public-key"],
      ["GET", "/api/nothing"],
      ["POST", "/api/users/set-info", {}],
      ["POST", "/api/users/login", []],
      ["POST", "/api/push/subscribe", {}],
      ["GET", "/.well-known/autoconfig/mail/config-v1.1.xml"],
      ["POST", "/"],
    ];
    for (const [method, target, json] of cases) {
      const result = await client.request(method, target, json === undefined ? {} : { json });
      expect({ target, etag: result.headers.get("etag") }).toEqual({ target, etag: weakEtag(result.text) });
      expect(result.headers.get("etag")).toMatch(/^W\/"[0-9a-f]+-[A-Za-z0-9+/]{27}"$/);
    }
    // A body with characters of more than one byte: the length is in bytes.
    await dev.db.query(
      "INSERT INTO users (username, password, email) VALUES ('ünï-🔑', 'x', 'unicode@elsewhere.example') ON CONFLICT DO NOTHING"
    );
    const signedIn = dev.client();
    await signedIn.login("admin", ADMIN_PASSWORD);
    const [session] = await dev.db.query("SELECT session_id FROM sessions ORDER BY updated DESC LIMIT 1");
    await dev.db.query("UPDATE sessions SET session_username = 'ünï-🔑 日本' WHERE session_id = $1", [session!.session_id]);
    const who = await signedIn.get("/api/users/login");
    expect(who.body.body.user.username).toBe("ünï-🔑 日本");
    expect(who.headers.get("etag")).toBe(weakEtag(who.text));
    expect(who.headers.get("content-length")).toBe(String(Buffer.byteLength(who.text)));
  });

  test("If-None-Match with the ETag of a GET answer → 304 without a body or a content type", async () => {
    for (const target of ["/api/ping", "/api/users/login", "/api/push/public-key"]) {
      const first = await raw("GET", target);
      const etag = first.headers.etag!;
      const again = await raw("GET", target, { "If-None-Match": etag });
      expect({ target, status: again.status }).toEqual({ target, status: 304 });
      expect(again.statusText).toBe("Not Modified");
      expect(again.body).toBe("");
      expect(again.headers.etag).toBe(etag);
      expect(again.headers["content-type"]).toBeUndefined();
      expect(again.headers["content-length"]).toBeUndefined();
      expect(again.headers["transfer-encoding"]).toBeUndefined();
      expect(securityHeadersOf(again.headers)).toEqual(expectedSecurityHeaders(false));

      const head = await raw("HEAD", target, { "If-None-Match": etag });
      expect(head.status).toBe(304);
    }
  });

  test("the tag is compared as in a list, weakly, or by the wildcard", async () => {
    const etag = (await raw("GET", "/api/ping")).headers.etag!;
    const status = (value: string, extra: Record<string, string> = {}) =>
      raw("GET", "/api/ping", { "If-None-Match": value, ...extra }).then((response) => response.status);
    expect(await status("*")).toBe(304);
    expect(await status(etag.slice(2))).toBe(304);
    expect(await status(`"x", ${etag}`)).toBe(304);
    expect(await status(`"x",${etag.slice(2)} , "y"`)).toBe(304);
    expect(await status('"x"')).toBe(200);
    expect(await status(etag.replace(/"/g, ""))).toBe(200);
    expect(await status(etag, { "Cache-Control": "no-cache" })).toBe(200);
    expect(await status(etag, { "Cache-Control": "max-age=0" })).toBe(304);
  });

  test("a repeated If-None-Match header is read as one list", async () => {
    const etag = (await raw("GET", "/api/ping")).headers.etag!;
    const response = await rawHttp(
      dev.ports.http,
      `GET /api/ping HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nIf-None-Match: "nope"\r\nIf-None-Match: ${etag}\r\n\r\n`
    );
    expect(response.status).toBe(304);
  });

  test("there is no Last-Modified on an API answer, so If-Modified-Since never matches", async () => {
    const response = await raw("GET", "/api/ping", { "If-Modified-Since": "Fri, 01 Jan 2100 00:00:00 GMT" });
    expect(response.status).toBe(200);
    expect(response.headers["last-modified"]).toBeUndefined();
    const both = await raw("GET", "/api/ping", { "If-None-Match": "*", "If-Modified-Since": "Fri, 01 Jan 2100 00:00:00 GMT" });
    expect(both.status).toBe(200);
  });

  test("only GET and HEAD are conditional", async () => {
    const posted = await dev.client().post("/api/users/set-info", {}, { headers: { "If-None-Match": "*" } });
    expect(posted.status).toBe(200);
    expect(posted.body).toEqual(NOT_READ);
    const deleted = await dev.client().delete("/api/users/login", { headers: { "If-None-Match": "*" } });
    expect(deleted.status).toBe(200);
  });

  test("a 304 for a signed-in caller still renews the session cookie", async () => {
    const client = dev.client();
    await client.login("admin", ADMIN_PASSWORD);
    const first = await client.get("/api/users/login");
    const again = await client.get("/api/users/login", { headers: { "If-None-Match": first.headers.get("etag")! } });
    expect(again.status).toBe(304);
    expect(again.text).toBe("");
    expect(again.setCookies).toHaveLength(1);
  });

  test("HEAD has the length of the body GET would send", async () => {
    for (const target of ["/api/ping", "/api/users/login", "/api/nothing", "/.well-known/autoconfig/mail/config-v1.1.xml"]) {
      const get = await raw("GET", target);
      const head = await raw("HEAD", target);
      expect(head.status).toBe(get.status);
      expect(head.body).toBe("");
      expect(head.headers["content-length"]).toBe(get.headers["content-length"]);
      expect(head.headers.etag).toBe(get.headers.etag);
      expect(head.headers["content-type"]).toBe(get.headers["content-type"]);
    }
  });
});
