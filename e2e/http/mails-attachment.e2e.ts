import fs from "fs";
import path from "path";
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { rawHttp, readMail, startServer, type TestServer } from "../harness";
import { JSON_TYPE, NOT_FOUND, adminMailbox, attachmentPath, newMailbox, writeAttachment, type Mailbox } from "./mails-helpers";

let server: TestServer;
let box: Mailbox;

const MTIME = new Date("2026-01-02T03:04:05.000Z");
const MTIME_HEX = MTIME.getTime().toString(16);
const ID = "22222222-2222-4222-8222-222222222222";
const CONTENT = "file-content-ünï 😀\n";
const SIZE = Buffer.byteLength(CONTENT);

const attachment = (id: string, extra: Record<string, unknown> = {}) => ({
  content: { data: id },
  contentType: "text/plain",
  filename: "a.txt",
  size: 1,
  ...extra,
});

beforeAll(async () => {
  server = await startServer();
  box = await newMailbox(server);
  writeAttachment(server, ID, CONTENT, MTIME);
  await box.mail({ attachments: [attachment(ID, { contentType: "text/html", filename: "evil.html" })] });
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

const get = (id: string, client = box.client, headers?: Record<string, string>) =>
  client.get(`/api/mails/attachment/${id}`, headers ? { headers } : undefined);

describe("GET /api/mails/attachment/:id — a stored file", () => {
  test("is sent as a download of unknown type, whatever type the mail recorded", async () => {
    const { status, text, headers } = await get(ID);
    expect(status).toBe(200);
    expect(text).toBe(CONTENT);
    expect(headers.get("content-type")).toBe("application/octet-stream");
    expect(headers.get("content-length")).toBe(String(SIZE));
    expect(headers.get("content-disposition")).toBe(`filename="${ID}"`);
    expect(headers.get("x-content-type-options")).toBe("nosniff");
  });

  test("carries a weak ETag of the file's size and modification time in hex", async () => {
    const { headers } = await get(ID);
    expect(headers.get("etag")).toBe(`W/"${SIZE.toString(16)}-${MTIME_HEX}"`);
  });

  test("sends the bytes unchanged", async () => {
    const bytes = new Uint8Array(70_000);
    for (let i = 0; i < bytes.length; i++) bytes[i] = (i * 31) % 256;
    const id = "33333333-3333-4333-8333-333333333333";
    writeAttachment(server, id, bytes);
    await box.mail({ attachments: [attachment(id)] });
    const response = await fetch(`${server.url}/api/mails/attachment/${id}`, {
      headers: { Cookie: box.client.cookieHeader()! },
    });
    expect(response.status).toBe(200);
    expect(new Uint8Array(await response.arrayBuffer())).toEqual(bytes);
  });

  test("an empty file is an empty 200", async () => {
    const id = "44444444-4444-4444-8444-444444444444";
    writeAttachment(server, id, "", MTIME);
    await box.mail({ attachments: [attachment(id)] });
    const { status, text, headers } = await get(id);
    expect(status).toBe(200);
    expect(text).toBe("");
    expect(headers.get("content-length")).toBe("0");
    expect(headers.get("etag")).toBe(`W/"0-${MTIME_HEX}"`);
  });

  test("answers HEAD with the headers and no body", async () => {
    const head = await box.client.head(`/api/mails/attachment/${ID}`);
    expect(head.status).toBe(200);
    expect(head.text).toBe("");
    expect(head.headers.get("content-length")).toBe(String(SIZE));
    expect(head.headers.get("content-type")).toBe("application/octet-stream");
    expect(head.headers.get("etag")).toBe(`W/"${SIZE.toString(16)}-${MTIME_HEX}"`);
  });

  test("answers 304 to a matching If-None-Match", async () => {
    const etag = `W/"${SIZE.toString(16)}-${MTIME_HEX}"`;
    for (const value of [etag, etag.slice(2), "*"]) {
      const again = await get(ID, box.client, { "If-None-Match": value });
      expect({ value, status: again.status, text: again.text }).toEqual({ value, status: 304, text: "" });
      expect(again.headers.get("etag")).toBe(etag);
    }
    expect((await get(ID, box.client, { "If-None-Match": 'W/"0-0"' })).status).toBe(200);
  });

  test("answers a byte range with 206", async () => {
    const bytes = Buffer.from(CONTENT);
    const first = await get(ID, box.client, { Range: "bytes=0-3" });
    expect(first.status).toBe(206);
    expect(first.text).toBe("file");
    expect(first.headers.get("content-range")).toBe(`bytes 0-3/${SIZE}`);
    expect(first.headers.get("content-length")).toBe("4");
    expect(first.headers.get("accept-ranges")).toBe("bytes");
    const suffix = await get(ID, box.client, { Range: "bytes=-5" });
    expect(suffix.status).toBe(206);
    expect(suffix.headers.get("content-range")).toBe(`bytes ${SIZE - 5}-${SIZE - 1}/${SIZE}`);
    expect(suffix.text).toBe(bytes.subarray(SIZE - 5).toString());
    const open = await get(ID, box.client, { Range: "bytes=5-" });
    expect(open.status).toBe(206);
    expect(open.headers.get("content-range")).toBe(`bytes 5-${SIZE - 1}/${SIZE}`);
  });

  test("answers a range past the end with 416", async () => {
    const response = await get(ID, box.client, { Range: `bytes=${SIZE + 10}-${SIZE + 20}` });
    expect(response.status).toBe(416);
    expect(response.text).toBe("");
    expect(response.headers.get("content-range")).toBe(`bytes */${SIZE}`);
  });

  test("ignores the query string and a trailing slash, and takes the id in any letter case of the route", async () => {
    for (const spelled of [`${ID}?download=1`, `${ID}/`]) {
      const { status, text } = await get(spelled);
      expect({ spelled, status, text }).toEqual({ spelled, status: 200, text: CONTENT });
    }
    const upperRoute = await box.client.get(`/api/mails/ATTACHMENT/${ID}`);
    expect(upperRoute.text).toBe(CONTENT);
  });

  test("does not mark the mail read", async () => {
    const id = "55555555-5555-4555-8555-555555555555";
    writeAttachment(server, id, "x");
    const mail = await box.mail({ attachments: [attachment(id)] });
    await get(id);
    expect((await readMail(server, mail.mail_id))!.read).toBe(false);
  });

  test("is found among several attachments of one mail and across mails", async () => {
    const first = "66666666-6666-4666-8666-666666666661";
    const second = "66666666-6666-4666-8666-666666666662";
    writeAttachment(server, first, "first");
    writeAttachment(server, second, "second");
    await box.mail({ attachments: [attachment(first), attachment(second, { filename: "b.txt" })] });
    await box.mail({ attachments: [attachment(second)] });
    expect((await get(first)).text).toBe("first");
    expect((await get(second)).text).toBe("second");
  });

  test("is served for sent mail, spam, drafts and deleted mail", async () => {
    const rows = [{ sent: true }, { is_spam: true }, { draft: true }, { deleted: true }];
    for (const [i, row] of rows.entries()) {
      const id = `77777777-7777-4777-8777-77777777777${i}`;
      writeAttachment(server, id, `content ${i}`);
      await box.mail({ ...row, attachments: [attachment(id)] });
      const { status, text } = await get(id);
      expect({ row, status, text }).toEqual({ row, status: 200, text: `content ${i}` });
    }
  });
});

describe("GET /api/mails/attachment/:id — an expunged mail", () => {
  test("its attachment is Not found, like one no mail names, though the file is on disk", async () => {
    const id = "88888888-8888-4888-8888-888888888881";
    writeAttachment(server, id, "expunged");
    await box.mail({ expunged: true, attachments: [attachment(id)] });
    const { status, text, headers } = await get(id);
    expect(status).toBe(200);
    expect(text).toBe('{"status":"failed","message":"Not found"}');
    expect(headers.get("content-disposition")).toBeNull();
    expect(fs.existsSync(attachmentPath(server, id))).toBe(true);
  });

  test("an attachment an expunged mail shares with a live one is served for the live one", async () => {
    // An IMAP MOVE leaves the source expunged and its copy naming the same file.
    const id = "88888888-8888-4888-8888-888888888882";
    writeAttachment(server, id, "moved");
    const live = await box.mail({ attachments: [attachment(id)] });
    await box.mail({ expunged: true, attachments: [attachment(id)] });
    expect((await get(id)).text).toBe("moved");
    await server.db.query("UPDATE mails SET expunged = TRUE WHERE mail_id = $1", [live.mail_id]);
    expect((await get(id)).body).toEqual(NOT_FOUND);
  });
});

describe("GET /api/mails/attachment/:id — refusals", () => {
  test("an id no mail of the user names is Not found, with an exact body", async () => {
    const { status, text, headers } = await get("99999999-9999-4999-8999-999999999999");
    expect(status).toBe(200);
    expect(text).toBe('{"status":"failed","message":"Not found"}');
    expect(headers.get("content-type")).toBe(JSON_TYPE);
    expect(headers.get("content-disposition")).toBeNull();
  });

  test("another user's attachment is Not found, for a user and for admin", async () => {
    const other = await newMailbox(server);
    const admin = await adminMailbox(server);
    for (const client of [other.client, admin.client]) {
      const { status, body } = await get(ID, client);
      expect(status).toBe(200);
      expect(body).toEqual(NOT_FOUND);
    }
  });

  test("the id must match the stored one exactly: no other letter case, no partial match", async () => {
    const mixed = "AbCdEf00-1111-4111-8111-111111111111";
    writeAttachment(server, mixed, "mixed");
    await box.mail({ attachments: [attachment(mixed)] });
    expect((await get(mixed)).text).toBe("mixed");
    for (const id of [mixed.toLowerCase(), mixed.toUpperCase(), mixed.slice(0, 8), `${mixed}0`, "%20", "x".repeat(3000)]) {
      const { status, body } = await get(id);
      expect({ id: id.slice(0, 40), status, body }).toEqual({ id: id.slice(0, 40), status: 200, body: NOT_FOUND });
    }
  });

  test("a mail's attachment whose file is gone answers a bare failed status", async () => {
    const id = "88888888-8888-4888-8888-888888888888";
    await box.mail({ attachments: [attachment(id)] });
    const { status, text, headers } = await get(id);
    expect(status).toBe(200);
    expect(text).toBe('{"status":"failed"}');
    expect(headers.get("content-type")).toBe(JSON_TYPE);
    expect(headers.get("etag")).toBe('W/"13-s4irxbhWs8tlAyz2jRLN7icHP8c"');
  });

  test("a file on disk that no mail names is Not found", async () => {
    const id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    writeAttachment(server, id, "orphan");
    expect((await get(id)).body).toEqual(NOT_FOUND);
  });

  test("attachments stored as an object or with no content id match nothing", async () => {
    writeAttachment(server, "obj-att", "obj");
    await box.mail({ attachments: { content: { data: "obj-att" } } });
    await box.mail({ attachments: [{ filename: "no-content.bin" }, { content: {} }, { content: { data: null } }] });
    expect((await get("obj-att")).body).toEqual(NOT_FOUND);
    expect((await get("null")).body).toEqual(NOT_FOUND);
    expect((await get("undefined")).body).toEqual(NOT_FOUND);
  });

  test("POST and DELETE are not answered", async () => {
    expect((await box.client.post(`/api/mails/attachment/${ID}`, {})).status).toBe(404);
    expect((await box.client.delete(`/api/mails/attachment/${ID}`)).status).toBe(404);
    expect(fs.existsSync(attachmentPath(server, ID))).toBe(true);
  });
});

describe("GET /api/mails/attachment/:id — ids that are not UUIDs", () => {
  test("any stored id is served, the file name in Content-Disposition being its last path segment", async () => {
    writeAttachment(server, "plain-name", "plain", MTIME);
    writeAttachment(server, "sp ace.txt", "spaced", MTIME);
    writeAttachment(server, "ünï-😀", "unicode", MTIME);
    await box.mail({ attachments: [attachment("plain-name"), attachment("sp ace.txt"), attachment("ünï-😀")] });
    const plain = await get("plain-name");
    expect(plain.text).toBe("plain");
    expect(plain.headers.get("content-disposition")).toBe('filename="plain-name"');
    expect(plain.headers.get("etag")).toBe(`W/"5-${MTIME_HEX}"`);
    const spaced = await get("sp%20ace.txt");
    expect(spaced.text).toBe("spaced");
    expect(spaced.headers.get("content-type")).toBe("application/octet-stream");
    const unicode = await get(encodeURIComponent("ünï-😀"));
    expect(unicode.text).toBe("unicode");
  });

  // NOTE: the id is joined onto ./attachments/ without any check. Ids are
  // generated by the server today, so this is not reachable from outside —
  // but a stored id containing "../" (a bad import, a future write path, a
  // tampered row) makes this route serve any file the process can read.
  test("a stored id with ../ in it escapes the attachments directory", async () => {
    fs.writeFileSync(path.join(server.cwd, "outside.txt"), "outside the attachments directory");
    fs.mkdirSync(path.join(server.cwd, "attachments"), { recursive: true });
    await box.mail({ attachments: [attachment("../outside.txt")] });
    const response = await rawHttp(
      server.ports.http,
      `GET /api/mails/attachment/..%2Foutside.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nCookie: ${box.client.cookieHeader()}\r\n\r\n`
    );
    expect(response.status).toBe(200);
    expect(response.body).toBe("outside the attachments directory");
    expect(response.headers["content-disposition"]).toBe('filename="outside.txt"');
    // Without a mail of the user naming that id, the same request is refused.
    const other = await newMailbox(server);
    expect((await get("..%2Foutside.txt", other.client)).body).toEqual(NOT_FOUND);
  });

  test("a stored id naming a directory answers the bare failed status", async () => {
    fs.mkdirSync(path.join(server.cwd, "attachments", "a-directory"), { recursive: true });
    await box.mail({ attachments: [attachment("a-directory")] });
    const { status, text } = await get("a-directory");
    expect(status).toBe(200);
    expect(text).toBe('{"status":"failed"}');
  });
});
