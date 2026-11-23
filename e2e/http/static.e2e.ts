import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import crypto from "crypto";
import fs from "fs";
import path from "path";
import { ADMIN_PASSWORD, CLIENT_DIR, rawHttp, type RawResponse, type TestServer } from "../harness";
import { startServer } from "./core-helpers";
import { TEXT_TYPE, expectedSecurityHeaders, rawRequest, securityHeadersOf, sessionIdOf, sessionIdOfValue, parseSetCookie } from "./core-helpers";

/**
 * Both servers serve `build/client`. The files a build produces are few and
 * of few kinds, so these tests add a directory of their own to it for the
 * run — names and types a build would not have — and remove it afterwards.
 */
const FIXTURE = `e2e-static-${crypto.randomBytes(6).toString("hex")}`;
const fixtureDir = path.join(CLIENT_DIR, FIXTURE);

/** A modification time with a fraction of a millisecond, in seconds. */
const MTIME_SECONDS = 1_700_000_000.1239;
const MTIME_MS = 1_700_000_000_123;
const LAST_MODIFIED = "Tue, 14 Nov 2023 22:13:20 GMT";

const BINARY = Buffer.from(Array.from({ length: 1024 }, (_, i) => (i * 7 + 3) % 256));

/** `[extension, content type, whether the body is offered as a download]` */
const TYPES: [string, string, boolean][] = [
  ["html", "text/html;charset=utf-8", false],
  ["htm", "text/html;charset=utf-8", false],
  ["js", "text/javascript;charset=utf-8", false],
  ["mjs", "text/javascript;charset=utf-8", false],
  ["cjs", "text/javascript;charset=utf-8", false],
  ["ts", "text/javascript;charset=utf-8", false],
  ["tsx", "text/javascript;charset=utf-8", false],
  ["jsx", "text/javascript;charset=utf-8", false],
  ["mts", "text/javascript;charset=utf-8", false],
  ["css", "text/css;charset=utf-8", false],
  ["json", "application/json;charset=utf-8", false],
  ["map", "application/json;charset=utf-8", false],
  ["txt", "text/plain;charset=utf-8", false],
  ["text", "text/plain;charset=utf-8", false],
  ["log", "text/plain;charset=utf-8", false],
  ["conf", "text/plain;charset=utf-8", false],
  ["ini", "text/plain;charset=utf-8", false],
  ["md", "text/markdown", false],
  ["csv", "text/csv", false],
  ["yaml", "text/yaml", false],
  ["yml", "text/yaml", false],
  ["ics", "text/calendar", false],
  ["rtf", "text/rtf", false],
  ["vcf", "text/x-vcard", false],
  ["c", "text/x-c", false],
  ["scss", "text/x-scss", false],
  ["less", "text/less", false],
  ["manifest", "text/cache-manifest", false],
  ["appcache", "text/cache-manifest", false],
  ["png", "image/png", false],
  ["apng", "image/apng", false],
  ["jpg", "image/jpeg", false],
  ["jpeg", "image/jpeg", false],
  ["gif", "image/gif", false],
  ["svg", "image/svg+xml", false],
  ["ico", "image/x-icon", false],
  ["webp", "image/webp", false],
  ["avif", "image/avif", false],
  ["bmp", "image/x-ms-bmp", false],
  ["tiff", "image/tiff", false],
  ["heic", "image/heic", false],
  ["psd", "image/vnd.adobe.photoshop", false],
  ["woff", "font/woff", false],
  ["woff2", "font/woff2", false],
  ["ttf", "font/ttf", false],
  ["otf", "font/otf", false],
  ["mp3", "audio/mpeg", false],
  ["ogg", "audio/ogg", false],
  ["opus", "audio/ogg", false],
  ["wav", "audio/x-wav", false],
  ["m4a", "audio/x-m4a", false],
  ["flac", "audio/x-flac", false],
  ["aac", "audio/x-aac", false],
  ["weba", "audio/webm", false],
  ["mid", "audio/midi", false],
  ["mp4", "video/mp4", false],
  ["webm", "video/webm", false],
  ["mov", "video/quicktime", false],
  ["mkv", "video/x-matroska", false],
  ["ogv", "video/ogg", false],
  ["avi", "video/x-msvideo", false],
  ["m4v", "video/x-m4v", false],
  ["3gp", "video/3gpp", false],
  ["wasm", "application/wasm", false],
  ["geojson", "application/geo+json", false],
  ["xml", "application/xml", true],
  ["xhtml", "application/xhtml+xml", true],
  ["xsl", "application/xslt+xml", true],
  ["rss", "application/rss+xml", true],
  ["atom", "application/atom+xml", true],
  ["pdf", "application/pdf", true],
  ["zip", "application/zip", true],
  ["gz", "application/gzip", true],
  ["tar", "application/x-tar", true],
  ["7z", "application/x-7z-compressed", true],
  ["rar", "application/x-rar-compressed", true],
  ["jar", "application/java-archive", true],
  ["toml", "application/toml", true],
  ["webmanifest", "application/manifest+json", true],
  ["jsonld", "application/ld+json", true],
  ["json5", "application/json5", true],
  ["webapp", "application/x-web-app-manifest+json", true],
  ["es", "application/ecmascript", true],
  ["sh", "application/x-sh", true],
  ["sql", "application/x-sql", true],
  ["eot", "application/vnd.ms-fontobject", true],
  ["doc", "application/msword", true],
  ["docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document", true],
  ["xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", true],
  ["exe", "application/x-msdownload", true],
  ["dmg", "application/x-apple-diskimage", true],
  ["apk", "application/vnd.android.package-archive", true],
  ["swf", "application/x-shockwave-flash", true],
  ["pem", "application/x-x509-ca-cert", true],
  ["crt", "application/x-x509-ca-cert", true],
  ["key", "application/x-iwork-keynote-sffkey", true],
  ["m3u8", "application/vnd.apple.mpegurl", true],
  ["mpd", "application/dash+xml", true],
  ["kml", "application/vnd.google-earth.kml+xml", true],
  ["ai", "application/postscript", true],
  ["rs", "application/rls-services+xml", true],
  ["glb", "model/gltf-binary", true],
  ["gltf", "model/gltf+json", true],
  ["obj", "model/obj", true],
  ["stl", "model/stl", true],
  ["eml", "message/rfc822", true],
  ["bin", "application/octet-stream", true],
  ["py", "application/octet-stream", true],
  ["vue", "application/octet-stream", true],
  ["svelte", "application/octet-stream", true],
  ["jsonc", "application/octet-stream", true],
  ["lock", "application/octet-stream", true],
  ["env", "application/octet-stream", true],
  ["sqlite", "application/octet-stream", true],
  ["jxl", "application/octet-stream", true],
  ["unknownext", "application/octet-stream", true],
];

let server: TestServer;
let indexHtml: Buffer;
let indexStat: fs.Stats;

const write = (relative: string, content: string | Buffer) => {
  const file = path.join(fixtureDir, relative);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, content);
  fs.utimesSync(file, MTIME_SECONDS, MTIME_SECONDS);
};

beforeAll(async () => {
  // Whatever an interrupted run left behind.
  for (const name of fs.readdirSync(CLIENT_DIR)) {
    if (name.startsWith("e2e-static-")) fs.rmSync(path.join(CLIENT_DIR, name), { recursive: true, force: true });
  }
  write("ten.txt", "0123456789");
  write("empty.txt", "");
  write("data.bin", BINARY);
  write("page.html", "<p>fixture page</p>");
  write("with space.txt", "space");
  write("ünï cödé.txt", "unicode");
  write("100%.txt", "percent");
  write("plus+sign.txt", "plus");
  write(".hidden.txt", "hidden file");
  write(".dotdir/inside.txt", "inside a hidden directory");
  write("sub/index.html", "<p>sub index</p>");
  write("sub/.secret", "hidden in a directory");
  write("sub/deep/file.txt", "deep");
  write("noindex/file.txt", "no index here");
  write("noext", "no extension");
  write("UPPER.HTML", "<p>upper</p>");
  write("Mixed.Json", "{}");
  write("two.tar.gz", "gz");
  write("trailingdot.", "dot");
  write("...", "dots");
  write("names/with space.bin", "x");
  write("names/semi;colon.bin", "x");
  write("names/ünï.bin", "x");
  write("names/日本.bin", "x");
  write('names/quo"te.bin', "x");
  write("names/line\nfeed.bin", "x");
  write("names/carriage\rreturn.bin", "x");
  write("names/back\\slash.bin", "x");
  write(`names/${"n".repeat(200)}.bin`, "x");
  for (const [extension] of TYPES) write(`types/f.${extension}`, "x");

  indexHtml = fs.readFileSync(path.join(CLIENT_DIR, "index.html"));
  indexStat = fs.statSync(path.join(CLIENT_DIR, "index.html"));
  server = await startServer();
}, 120_000);

afterAll(async () => {
  await server?.stop();
  fs.rmSync(fixtureDir, { recursive: true, force: true });
});

const etagOf = (size: number, mtimeMs: number) => `W/"${size.toString(16)}-${Math.floor(mtimeMs).toString(16)}"`;
const fixtureEtag = (size: number) => etagOf(size, MTIME_MS);
const INDEX_ETAG = () => etagOf(indexStat.size, indexStat.mtimeMs);

/** The URL path of a fixture file, each segment percent-encoded. */
const url = (relative: string) => `/${FIXTURE}/${relative.split("/").map(encodeURIComponent).join("/")}`;

const raw = (method: string, target: string, headers: Record<string, string> = {}): Promise<RawResponse> =>
  rawHttp(server.ports.http, rawRequest(method, target, headers));

/** GET with `fetch`, the body as bytes. */
const fetchBytes = async (target: string, headers: Record<string, string> = {}, method = "GET") => {
  const response = await fetch(`${server.url}${target}`, { method, headers, redirect: "manual" });
  return { status: response.status, headers: response.headers, bytes: Buffer.from(await response.arrayBuffer()) };
};

const expectIndexDocument = (response: { status: number; headers: Headers | Record<string, string>; bytes?: Buffer; body?: string }) => {
  const header = (name: string) =>
    response.headers instanceof Headers ? response.headers.get(name) : (response.headers[name] ?? null);
  expect(response.status).toBe(200);
  expect(header("content-type")).toBe("text/html;charset=utf-8");
  expect(header("etag")).toBe(INDEX_ETAG());
  expect(header("content-length")).toBe(String(indexHtml.byteLength));
  if (response.bytes) expect(response.bytes.equals(indexHtml)).toBe(true);
  if (response.body !== undefined) expect(response.body).toBe(indexHtml.toString("utf8"));
};

describe("files of the built client", () => {
  test("the entry document at /", async () => {
    const response = await fetchBytes("/");
    expectIndexDocument(response);
    expect(response.headers.get("cache-control")).toBe("public, max-age=0");
    expect(response.headers.get("last-modified")).toBe(new Date(Math.floor(indexStat.mtimeMs)).toUTCString());
    expect(response.headers.get("accept-ranges")).toBe("bytes");
    expect(response.headers.get("content-disposition")).toBeNull();
    expect(response.headers.getSetCookie()).toEqual([]);
    expect(securityHeadersOf(response.headers)).toEqual(expectedSecurityHeaders(false));
  });

  test("the entry document by name", async () => {
    expectIndexDocument(await fetchBytes("/index.html"));
    expectIndexDocument(await fetchBytes("/index.html?v=1&x"));
  });

  test("every file the build produced, byte for byte, with validators from its size and modification time", async () => {
    const files: string[] = [];
    const walk = (dir: string, prefix: string) => {
      for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
        if (entry.name.startsWith("e2e-")) continue;
        if (entry.isDirectory()) walk(path.join(dir, entry.name), `${prefix}${entry.name}/`);
        else files.push(`${prefix}${entry.name}`);
      }
    };
    walk(CLIENT_DIR, "");
    expect(files.length).toBeGreaterThan(10);
    const known: Record<string, string> = {
      ".html": "text/html;charset=utf-8",
      ".js": "text/javascript;charset=utf-8",
      ".css": "text/css;charset=utf-8",
      ".json": "application/json;charset=utf-8",
      ".txt": "text/plain;charset=utf-8",
      ".png": "image/png",
      ".ico": "image/x-icon",
      ".md": "text/markdown",
      ".svg": "image/svg+xml",
    };
    for (const file of files) {
      const onDisk = fs.readFileSync(path.join(CLIENT_DIR, file));
      const stat = fs.statSync(path.join(CLIENT_DIR, file));
      const response = await fetchBytes(`/${file}`);
      expect({ file, status: response.status }).toEqual({ file, status: 200 });
      expect(response.bytes.equals(onDisk)).toBe(true);
      expect({ file, etag: response.headers.get("etag") }).toEqual({ file, etag: etagOf(stat.size, stat.mtimeMs) });
      expect(response.headers.get("last-modified")).toBe(new Date(Math.floor(stat.mtimeMs)).toUTCString());
      expect(response.headers.get("content-length")).toBe(String(stat.size));
      expect(response.headers.get("cache-control")).toBe("public, max-age=0");
      expect(response.headers.get("accept-ranges")).toBe("bytes");
      const expectedType = known[path.extname(file)];
      expect(expectedType).toBeDefined();
      expect({ file, type: response.headers.get("content-type") }).toEqual({ file, type: expectedType! });
      expect(response.headers.get("content-disposition")).toBeNull();
    }
  });

  test("the modification time is cut to the millisecond for the ETag and to the second for Last-Modified", async () => {
    const response = await fetchBytes(url("ten.txt"));
    expect(response.status).toBe(200);
    expect(response.bytes.toString()).toBe("0123456789");
    // 1700000000.1239 s: milliseconds .123, not rounded up to .124.
    expect(response.headers.get("etag")).toBe('W/"a-18bcfe5687b"');
    expect(response.headers.get("etag")).toBe(fixtureEtag(10));
    expect(response.headers.get("last-modified")).toBe(LAST_MODIFIED);
  });

  test("an empty file", async () => {
    const response = await fetchBytes(url("empty.txt"));
    expect(response.status).toBe(200);
    expect(response.bytes.byteLength).toBe(0);
    expect(response.headers.get("content-length")).toBe("0");
    expect(response.headers.get("etag")).toBe(fixtureEtag(0));
    expect(response.headers.get("content-type")).toBe("text/plain;charset=utf-8");
    expect(response.headers.get("accept-ranges")).toBe("bytes");
  });

  test("a binary file", async () => {
    const response = await fetchBytes(url("data.bin"));
    expect(response.status).toBe(200);
    expect(response.bytes.equals(BINARY)).toBe(true);
    expect(response.headers.get("content-type")).toBe("application/octet-stream");
  });

  test("answers HEAD with the headers of GET and no body", async () => {
    for (const target of ["/", "/manifest.json", url("ten.txt"), url("data.bin"), url("empty.txt")]) {
      const get = await raw("GET", target);
      const head = await raw("HEAD", target);
      expect({ target, status: head.status }).toEqual({ target, status: 200 });
      expect(head.body).toBe("");
      for (const name of ["content-type", "content-length", "etag", "last-modified", "cache-control", "accept-ranges"]) {
        expect({ target, name, value: head.headers[name] }).toEqual({ target, name, value: get.headers[name] });
      }
      expect(securityHeadersOf(head.headers)).toEqual(expectedSecurityHeaders(false));
    }
  });

  test("renews a session cookie like any other response", async () => {
    const client = server.client();
    await client.login("admin", ADMIN_PASSWORD);
    const page = await client.get("/");
    expect(page.status).toBe(200);
    expect(page.setCookies).toHaveLength(1);
    expect(sessionIdOfValue(parseSetCookie(page.setCookies[0]!).value)).toBe(sessionIdOf(client));
  });
});

describe("content types", () => {
  test.each(TYPES)(".%s is served as %s", async (extension, type) => {
    const response = await raw("GET", url(`types/f.${extension}`));
    expect(response.status).toBe(200);
    expect(response.headers["content-type"]).toBe(type);
    expect(response.body).toBe("x");
  });

  test("the extension is the text after the last dot, in any letter case", async () => {
    const cases: [string, string][] = [
      ["UPPER.HTML", "text/html;charset=utf-8"],
      ["Mixed.Json", "application/json;charset=utf-8"],
      ["two.tar.gz", "application/gzip"],
      ["noext", "application/octet-stream"],
      ["trailingdot.", "application/octet-stream"],
      ["with space.txt", "text/plain;charset=utf-8"],
    ];
    for (const [file, type] of cases) {
      const response = await raw("GET", url(file));
      expect({ file, status: response.status, type: response.headers["content-type"] }).toEqual({ file, status: 200, type });
    }
  });
});

describe("Content-Disposition on a file body", () => {
  test.each(TYPES)(".%s (%s) is offered as a download: %p", async (extension, _type, download) => {
    const response = await raw("GET", url(`types/f.${extension}`));
    expect(response.headers["content-disposition"]).toBe((download ? `filename="f.${extension}"` : undefined) as string);
  });

  test("names the file by its base name only", async () => {
    write("sub/deep/archive.zip", "zip");
    const response = await raw("GET", url("sub/deep/archive.zip"));
    expect(response.headers["content-disposition"]).toBe('filename="archive.zip"');
    expect((await raw("GET", url("noext"))).headers["content-disposition"]).toBe('filename="noext"');
    expect((await raw("GET", url("trailingdot."))).headers["content-disposition"]).toBe('filename="trailingdot."');
  });

  test("is sent with the body only: not for HEAD, 304 or 416, but for a range and an ignored range", async () => {
    const target = url("data.bin");
    const expected = 'filename="data.bin"';
    expect((await raw("GET", target)).headers["content-disposition"]).toBe(expected);
    expect((await raw("HEAD", target)).headers["content-disposition"]).toBeUndefined();

    const partial = await raw("GET", target, { Range: "bytes=0-9" });
    expect(partial.status).toBe(206);
    expect(partial.headers["content-disposition"]).toBe(expected);

    const ignored = await raw("GET", target, { Range: "bytes=0-1,4-5" });
    expect(ignored.status).toBe(200);
    expect(ignored.headers["content-disposition"]).toBe(expected);

    const unsatisfiable = await raw("GET", target, { Range: "bytes=5000-" });
    expect(unsatisfiable.status).toBe(416);
    expect(unsatisfiable.headers["content-disposition"]).toBeUndefined();

    const notModified = await raw("GET", target, { "If-None-Match": fixtureEtag(BINARY.byteLength) });
    expect(notModified.status).toBe(304);
    expect(notModified.headers["content-disposition"]).toBeUndefined();
    const headNotModified = await raw("HEAD", target, { "If-None-Match": "*" });
    expect(headNotModified.status).toBe(304);
    expect(headNotModified.headers["content-disposition"]).toBeUndefined();
  });

  test("carries the name's bytes as they are", async () => {
    // rawHttp reads header bytes as Latin-1, which is what the UTF-8 name looks like on the wire.
    const onTheWire = (name: string) => `filename="${Buffer.from(name, "utf8").toString("latin1")}"`;
    for (const name of ["with space.bin", "semi;colon.bin", "ünï.bin", "日本.bin", "back\\slash.bin", `${"n".repeat(200)}.bin`]) {
      const response = await raw("GET", url(`names/${name}`));
      expect({ name, status: response.status, header: response.headers["content-disposition"] }).toEqual({
        name,
        status: 200,
        header: onTheWire(name),
      });
    }
  });

  test("is left out for a name that would break the header: a quote or a line break", async () => {
    for (const name of ['quo"te.bin', "line\nfeed.bin", "carriage\rreturn.bin"]) {
      const response = await raw("GET", url(`names/${name}`));
      expect({ name, status: response.status, body: response.body }).toEqual({ name, status: 200, body: "x" });
      expect(response.headers["content-type"]).toBe("application/octet-stream");
      expect(response.headers["content-disposition"]).toBeUndefined();
    }
  });

  test("is not sent for anything that is not a file: the autoconfig document, API bodies, the text 404", async () => {
    expect((await raw("GET", "/.well-known/autoconfig/mail/config-v1.1.xml")).headers["content-disposition"]).toBeUndefined();
    expect((await raw("GET", "/api/ping")).headers["content-disposition"]).toBeUndefined();
    expect((await raw("POST", "/")).headers["content-disposition"]).toBeUndefined();
  });
});

describe("Range requests", () => {
  const ten = () => url("ten.txt");
  const range = (value: string, target = ten()) => raw("GET", target, { Range: value });

  test.each([
    ["bytes=0-4", "bytes 0-4/10", "01234"],
    ["bytes=5-", "bytes 5-9/10", "56789"],
    ["bytes=-3", "bytes 7-9/10", "789"],
    ["bytes=0-0", "bytes 0-0/10", "0"],
    ["bytes=9-9", "bytes 9-9/10", "9"],
    ["bytes=9-", "bytes 9-9/10", "9"],
    ["bytes=3-3", "bytes 3-3/10", "3"],
    ["bytes=0-9", "bytes 0-9/10", "0123456789"],
    ["bytes=0-", "bytes 0-9/10", "0123456789"],
    ["bytes=2-99999", "bytes 2-9/10", "23456789"],
    ["bytes=-10", "bytes 0-9/10", "0123456789"],
    ["bytes=-99999", "bytes 0-9/10", "0123456789"],
    ["bytes=-1", "bytes 9-9/10", "9"],
    ["bytes=01-02", "bytes 1-2/10", "12"],
    ["bytes=0-18446744073709551615", "bytes 0-9/10", "0123456789"],
    ["bytes=0-9223372036854775808", "bytes 0-9/10", "0123456789"],
    ["bytes=-9223372036854775807", "bytes 0-9/10", "0123456789"],
  ])("%s → 206 %s", async (header, contentRange, body) => {
    const response = await range(header);
    expect(response.status).toBe(206);
    expect(response.statusText).toBe("Partial Content");
    expect(response.headers["content-range"]).toBe(contentRange);
    expect(response.headers["content-length"]).toBe(String(body.length));
    expect(response.body).toBe(body);
    expect(response.headers["accept-ranges"]).toBe("bytes");
    expect(response.headers["content-type"]).toBe("text/plain;charset=utf-8");
    expect(response.headers.etag).toBe(fixtureEtag(10));
    expect(response.headers["last-modified"]).toBe(LAST_MODIFIED);
    expect(response.headers["cache-control"]).toBe("public, max-age=0");
    expect(securityHeadersOf(response.headers)).toEqual(expectedSecurityHeaders(false));
  });

  test.each([
    ["BYTES=1-2"],
    ["Bytes=1-2"],
    ["bytes = 1 - 2"],
    ["bytes=\t1-2"],
    ["bytes=1 -2"],
    ["bytes=1- 2"],
    ["  bytes=1-2  "],
  ])("the unit's letter case and spaces between the parts do not matter: %p", async (header) => {
    const response = await range(header);
    expect({ header, status: response.status, range: response.headers["content-range"], body: response.body }).toEqual({
      header,
      status: 206,
      range: "bytes 1-2/10",
      body: "12",
    });
  });

  test("a suffix with a space before it", async () => {
    const response = await range("bytes= -2");
    expect(response.status).toBe(206);
    expect(response.headers["content-range"]).toBe("bytes 8-9/10");
  });

  test.each([
    ["bytes=10-"],
    ["bytes=10-12"],
    ["bytes=11-"],
    ["bytes=99999-"],
    ["bytes=18446744073709551615-"],
    ["bytes=-0"],
    ["bytes=-00"],
  ])("%s → 416", async (header) => {
    const response = await range(header);
    expect(response.status).toBe(416);
    expect(response.statusText).toBe("Range Not Satisfiable");
    expect(response.headers["content-range"]).toBe("bytes */10");
    expect(response.headers["content-length"]).toBe("0");
    expect(response.body).toBe("");
    expect(response.headers["accept-ranges"]).toBe("bytes");
    // The file's own headers stay.
    expect(response.headers["content-type"]).toBe("text/plain;charset=utf-8");
    expect(response.headers.etag).toBe(fixtureEtag(10));
    expect(response.headers["last-modified"]).toBe(LAST_MODIFIED);
    expect(response.headers["cache-control"]).toBe("public, max-age=0");
    expect(securityHeadersOf(response.headers)).toEqual(expectedSecurityHeaders(false));
  });

  test.each([
    ["bytes=0-1,3-4"],
    ["bytes=0-1, 3-4"],
    ["bytes=1-2,"],
    ["bytes=,1-2"],
    ["garbage"],
    ["bytes"],
    ["bytes="],
    ["bytes=-"],
    ["bytes=a-b"],
    ["bytes=1-b"],
    ["bytes=5-2"],
    ["bytes=+1-2"],
    ["bytes=1.0-2"],
    ["bytes=1e0-2"],
    ["bytes=0x1-2"],
    ["bytes 1-2"],
    ["bytes:1-2"],
    ["items=1-2"],
    ["byte=1-2"],
    ["bytes=1-2;q=1"],
    ["bytes=--1"],
    ["bytes=1-2-3"],
    ["bytes=1"],
    ["1-2"],
    ["=1-2"],
    ["bytes=0-99999999999999999999999"],
    ["bytes=99999999999999999999999-"],
    ["bytes=-18446744073709551616"],
    ["bytes=18446744073709551616-"],
  ])("%p is not a range: the whole file, and no Accept-Ranges", async (header) => {
    const response = await range(header);
    expect({ header, status: response.status, body: response.body }).toEqual({ header, status: 200, body: "0123456789" });
    expect(response.headers["content-length"]).toBe("10");
    expect(response.headers["content-range"]).toBeUndefined();
    expect(response.headers["accept-ranges"]).toBeUndefined();
    expect(response.headers.etag).toBe(fixtureEtag(10));
  });

  test("an empty file has no satisfiable range, and a suffix of it is ignored", async () => {
    for (const header of ["bytes=0-", "bytes=0-0", "bytes=1-", "bytes=-0"]) {
      const response = await range(header, url("empty.txt"));
      expect({ header, status: response.status, range: response.headers["content-range"] }).toEqual({
        header,
        status: 416,
        range: "bytes */0",
      });
    }
    for (const header of ["bytes=-1", "bytes=-100"]) {
      const response = await range(header, url("empty.txt"));
      expect({ header, status: response.status }).toEqual({ header, status: 200 });
      expect(response.headers["content-length"]).toBe("0");
      expect(response.headers["accept-ranges"]).toBeUndefined();
    }
  });

  test("a range of a binary file is those bytes", async () => {
    for (const [header, start, end] of [
      ["bytes=0-255", 0, 256],
      ["bytes=100-611", 100, 612],
      ["bytes=-17", 1007, 1024],
      ["bytes=1023-", 1023, 1024],
    ] as const) {
      const response = await fetchBytes(url("data.bin"), { Range: header });
      expect(response.status).toBe(206);
      expect(response.bytes.equals(BINARY.subarray(start, end))).toBe(true);
      expect(response.headers.get("content-range")).toBe(`bytes ${start}-${end - 1}/1024`);
      expect(response.headers.get("content-length")).toBe(String(end - start));
    }
  });

  test("HEAD ignores the range", async () => {
    const response = await raw("HEAD", ten(), { Range: "bytes=0-4" });
    expect(response.status).toBe(200);
    expect(response.headers["content-length"]).toBe("10");
    expect(response.headers["content-range"]).toBeUndefined();
    expect(response.headers["accept-ranges"]).toBeUndefined();
    expect(response.body).toBe("");
    const beyond = await raw("HEAD", ten(), { Range: "bytes=50-" });
    expect(beyond.status).toBe(200);
    expect(beyond.headers["content-length"]).toBe("10");
  });

  test("a range of the entry document, also when it is the fallback for an unknown path", async () => {
    for (const target of ["/", "/index.html", "/some/client/route"]) {
      const response = await fetchBytes(target, { Range: "bytes=0-14" });
      expect({ target, status: response.status }).toEqual({ target, status: 206 });
      expect(response.bytes.equals(indexHtml.subarray(0, 15))).toBe(true);
      expect(response.headers.get("content-range")).toBe(`bytes 0-14/${indexHtml.byteLength}`);
    }
    const beyond = await raw("GET", "/some/client/route", { Range: `bytes=${indexHtml.byteLength}-` });
    expect(beyond.status).toBe(416);
    expect(beyond.headers["content-range"]).toBe(`bytes */${indexHtml.byteLength}`);
  });

  test("responses that are not files ignore the range", async () => {
    const xml = await raw("GET", "/.well-known/autoconfig/mail/config-v1.1.xml", { Range: "bytes=0-4" });
    expect(xml.status).toBe(200);
    expect(xml.headers["content-range"]).toBeUndefined();
    expect(xml.headers["accept-ranges"]).toBeUndefined();
    expect(xml.body.startsWith("<?xml")).toBe(true);
    expect(xml.body.length).toBeGreaterThan(100);

    const api = await raw("GET", "/api/ping", { Range: "bytes=0-4" });
    expect(api.status).toBe(200);
    expect(api.body).toBe('{"status":"success"}');
    expect(api.headers["accept-ranges"]).toBeUndefined();
    const beyond = await raw("GET", "/api/ping", { Range: "bytes=500-" });
    expect(beyond.status).toBe(200);

    const post = await raw("POST", ten(), { Range: "bytes=0-4" });
    expect(post.status).toBe(404);
    expect(post.body).toBe("Not Found");
  });

  test("a current validator wins over the range: 304", async () => {
    const response = await raw("GET", ten(), { Range: "bytes=0-4", "If-None-Match": fixtureEtag(10) });
    expect(response.status).toBe(304);
    expect(response.body).toBe("");
    expect(response.headers["content-range"]).toBeUndefined();
    // Asked with a Range, the response never had the header to keep.
    expect(response.headers["accept-ranges"]).toBeUndefined();
    const stale = await raw("GET", ten(), { Range: "bytes=0-4", "If-None-Match": '"something-else"' });
    expect(stale.status).toBe(206);
    expect(stale.body).toBe("01234");
  });

  test("If-Range lets the range through only for the file's exact Last-Modified", async () => {
    const withIfRange = (value: string) => raw("GET", ten(), { Range: "bytes=0-4", "If-Range": value });
    const current = await withIfRange(LAST_MODIFIED);
    expect(current.status).toBe(206);
    expect(current.body).toBe("01234");
    expect(current.headers["content-range"]).toBe("bytes 0-4/10");

    for (const value of [
      "Tue, 14 Nov 2023 22:13:21 GMT",
      "Tue, 14 Nov 2023 22:13:19 GMT",
      "Wed, 15 Nov 2023 22:13:20 GMT",
      // The ETag is a weak one, which never satisfies If-Range.
      fixtureEtag(10),
      fixtureEtag(10).slice(2),
      '"something-else"',
      "*",
      "garbage",
    ]) {
      const response = await withIfRange(value);
      expect({ value, status: response.status, body: response.body }).toEqual({ value, status: 200, body: "0123456789" });
      expect(response.headers["content-range"]).toBeUndefined();
      expect(response.headers["accept-ranges"]).toBeUndefined();
    }
  });

  test("when the Range header is repeated, the first one counts", async () => {
    const head =
      `GET ${ten()} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n` + "Range: bytes=1-2\r\nRange: bytes=5-6\r\n\r\n";
    const response = await rawHttp(server.ports.http, head);
    expect(response.status).toBe(206);
    expect(response.headers["content-range"]).toBe("bytes 1-2/10");
    expect(response.body).toBe("12");
  });
});

describe("conditional requests for a file", () => {
  const ten = () => url("ten.txt");
  const ETAG = fixtureEtag(10);

  test("If-None-Match with the file's ETag → 304 that keeps the validators and caching headers", async () => {
    const response = await raw("GET", ten(), { "If-None-Match": ETAG });
    expect(response.status).toBe(304);
    expect(response.statusText).toBe("Not Modified");
    expect(response.body).toBe("");
    expect(response.headers.etag).toBe(ETAG);
    expect(response.headers["last-modified"]).toBe(LAST_MODIFIED);
    expect(response.headers["cache-control"]).toBe("public, max-age=0");
    expect(response.headers["accept-ranges"]).toBe("bytes");
    expect(response.headers["content-type"]).toBeUndefined();
    expect(response.headers["content-length"]).toBeUndefined();
    expect(response.headers["transfer-encoding"]).toBeUndefined();
    expect(securityHeadersOf(response.headers)).toEqual(expectedSecurityHeaders(false));
  });

  test.each([
    ["the strong form of the tag", ETAG.slice(2)],
    ["the wildcard", "*"],
    ["a list that holds the tag", `"a", ${ETAG}, "b"`],
    ["a list without spaces", `"a",${ETAG}`],
    ["the tag with spaces around it", `  ${ETAG}  `],
    ["the strong form in a list", `W/"zzz", ${ETAG.slice(2)}`],
    // A tag is also compared with "W/" put in front of the file's own.
    ["the tag with a second weak prefix", `W/${ETAG}`],
  ])("If-None-Match matches %s", async (_label, value) => {
    const response = await raw("GET", ten(), { "If-None-Match": value });
    expect({ value, status: response.status }).toEqual({ value, status: 304 });
  });

  test.each([
    ["another tag", 'W/"a-18bcfe5687c"'],
    ["the tag without quotes", ETAG.replace(/"/g, "")],
    ["the tag in another letter case", ETAG.toUpperCase()],
    ["a tag with three weak prefixes", `W/W/${ETAG}`],
    ["the wildcard in a list", `*, "a"`],
    ["tags separated by spaces only", `"a" ${ETAG}`],
    ["garbage", "garbage"],
  ])("If-None-Match does not match %s", async (_label, value) => {
    const response = await raw("GET", ten(), { "If-None-Match": value });
    expect({ value, status: response.status, body: response.body }).toEqual({ value, status: 200, body: "0123456789" });
  });

  test.each([
    ["the file's Last-Modified", LAST_MODIFIED],
    ["a later time", "Tue, 14 Nov 2023 22:13:21 GMT"],
    ["a much later time", "Fri, 01 Jan 2100 00:00:00 GMT"],
    ["the same instant in ISO form", "2023-11-14T22:13:20.000Z"],
    ["the same instant with a zone offset", "Wed, 15 Nov 2023 00:13:20 +0200"],
    ["the same instant without the weekday", "14 Nov 2023 22:13:20 GMT"],
  ])("If-Modified-Since with %s → 304", async (_label, value) => {
    const response = await raw("GET", ten(), { "If-Modified-Since": value });
    expect({ value, status: response.status }).toEqual({ value, status: 304 });
    expect(response.headers.etag).toBe(ETAG);
  });

  test.each([
    ["an earlier time", "Tue, 14 Nov 2023 22:13:19 GMT"],
    ["a much earlier time", "Thu, 01 Jan 1970 00:00:00 GMT"],
    ["something that is not a date", "yesterday"],
    ["a number", "1700000000"],
    ["an empty-looking value", "0"],
  ])("If-Modified-Since with %s → 200", async (_label, value) => {
    const response = await raw("GET", ten(), { "If-Modified-Since": value });
    expect({ value, status: response.status, body: response.body }).toEqual({ value, status: 200, body: "0123456789" });
  });

  test("with both headers, both have to hold", async () => {
    const both = (noneMatch: string, modifiedSince: string) =>
      raw("GET", ten(), { "If-None-Match": noneMatch, "If-Modified-Since": modifiedSince }).then((response) => response.status);
    expect(await both(ETAG, LAST_MODIFIED)).toBe(304);
    expect(await both("*", LAST_MODIFIED)).toBe(304);
    expect(await both(ETAG, "Tue, 14 Nov 2023 22:13:19 GMT")).toBe(200);
    expect(await both('"other"', LAST_MODIFIED)).toBe(200);
    expect(await both("*", "not a date")).toBe(200);
  });

  test("Cache-Control: no-cache on the request asks for the file again", async () => {
    const status = (cacheControl: string) =>
      raw("GET", ten(), { "If-None-Match": ETAG, "Cache-Control": cacheControl }).then((response) => response.status);
    expect(await status("no-cache")).toBe(200);
    expect(await status("max-age=0, no-cache")).toBe(200);
    expect(await status("no-cache, no-store")).toBe(200);
    expect(await status("max-age=0,no-cache,no-store")).toBe(200);
    expect(await status("max-age=0 ,  no-cache  , x")).toBe(200);
    // Only the directive itself, spelled in lower case.
    expect(await status("No-Cache")).toBe(304);
    expect(await status("no-cache=1")).toBe(304);
    expect(await status("xno-cache")).toBe(304);
    expect(await status("max-age=0")).toBe(304);
    expect(await status("no-store")).toBe(304);
    // Pragma is not read.
    expect((await raw("GET", ten(), { "If-None-Match": ETAG, Pragma: "no-cache" })).status).toBe(304);
  });

  test("HEAD is answered 304 too; other methods are not conditional", async () => {
    const head = await raw("HEAD", ten(), { "If-None-Match": ETAG });
    expect(head.status).toBe(304);
    expect(head.headers.etag).toBe(ETAG);
    const post = await raw("POST", ten(), { "If-None-Match": "*" });
    expect(post.status).toBe(404);
  });

  test("the entry document is conditional as the fallback as well", async () => {
    for (const target of ["/", "/some/client/route"]) {
      const response = await raw("GET", target, { "If-None-Match": INDEX_ETAG() });
      expect({ target, status: response.status }).toEqual({ target, status: 304 });
      expect(response.headers.etag).toBe(INDEX_ETAG());
    }
  });
});

describe("paths that fall back to the entry document", () => {
  test.each([
    ["an unknown path", "/inbox"],
    ["a nested unknown path", "/mails/thread/123"],
    ["an unknown file name", "/missing.js"],
    ["an unknown file under a real directory", "/assets/missing.css"],
    ["a path that only starts like the API", "/apix/users/login"],
    ["a directory without a trailing slash", "/assets"],
    ["a directory with a trailing slash and no index.html", "/assets/"],
    ["the fixture's directory without an index", `/${FIXTURE}/noindex/`],
    ["a directory that has an index.html, without the slash", `/${FIXTURE}/sub`],
    ["a file with a slash after it", `/${FIXTURE}/ten.txt/`],
    ["a file below a file", `/${FIXTURE}/ten.txt/more`],
    ["a file in another letter case", "/MANIFEST.JSON"],
    ["a dotfile", `/${FIXTURE}/.hidden.txt`],
    ["a file in a dot directory", `/${FIXTURE}/.dotdir/inside.txt`],
    ["a dotfile in a directory", `/${FIXTURE}/sub/.secret`],
    ["a name made of dots", `/${FIXTURE}/...`],
    ["an encoded dotfile", `/${FIXTURE}/%2Ehidden.txt`],
    ["a plus sign standing for a space", `/${FIXTURE}/with+space.txt`],
    ["a doubly encoded name", `/${FIXTURE}/with%2520space.txt`],
  ])("%s", async (_label, target) => {
    expectIndexDocument(await raw("GET", target));
  });

  test.each([
    ["a parent segment that leaves the client directory", "/../package.json"],
    ["several of them", "/../../../../etc/passwd"],
    ["encoded parent segments", "/%2e%2e/%2e%2e/package.json"],
    ["mixed-case encoded parent segments", "/%2E%2e/package.json"],
    ["a parent segment behind an encoded slash", "/..%2fpackage.json"],
    ["a deeper one", `/${FIXTURE}/..%2f..%2f..%2fpackage.json`],
    ["encoded backslashes", `/${FIXTURE}/sub%5c..%5cten.txt`],
    ["a parent segment behind an encoded backslash", "/..%5cpackage.json"],
    ["an encoded parent segment behind an encoded slash", "/%2e%2e%2fpackage.json"],
    ["a NUL byte", `/${FIXTURE}/ten.txt%00`],
    ["a NUL byte in the middle", `/${FIXTURE}/ten%00.txt`],
    ["an escape that is not UTF-8", `/${FIXTURE}/%E0%A4%A`],
    ["a lone percent sign", `/${FIXTURE}/100%.txt`],
    ["a truncated escape", `/${FIXTURE}/ten.txt%2`],
    ["an escape that is not hex", `/${FIXTURE}/%zzten.txt`],
    ["an overlong encoding of a dot", "/%c0%ae%c0%ae/package.json"],
  ])("never a file outside the client, nor an error: %s", async (_label, target) => {
    expectIndexDocument(await raw("GET", target));
  });

  test("HEAD falls back the same way", async () => {
    const response = await raw("HEAD", "/inbox");
    expect(response.status).toBe(200);
    expect(response.headers.etag).toBe(INDEX_ETAG());
    expect(response.headers["content-length"]).toBe(String(indexHtml.byteLength));
    expect(response.body).toBe("");
  });
});

describe("paths that reach a file by another spelling", () => {
  const expectFile = (response: RawResponse, body: string) => {
    expect({ status: response.status, body: response.body }).toEqual({ status: 200, body });
  };

  test("a directory with a trailing slash serves its index.html", async () => {
    const response = await raw("GET", `/${FIXTURE}/sub/`);
    expectFile(response, "<p>sub index</p>");
    expect(response.headers["content-type"]).toBe("text/html;charset=utf-8");
    expect(response.headers.etag).toBe(fixtureEtag("<p>sub index</p>".length));
  });

  test("percent-encoded names: a space, unicode, a percent sign, a plus", async () => {
    expectFile(await raw("GET", `/${FIXTURE}/with%20space.txt`), "space");
    expectFile(await raw("GET", url("ünï cödé.txt")), "unicode");
    expectFile(await raw("GET", `/${FIXTURE}/%C3%BCn%C3%AF%20c%C3%B6d%C3%A9.txt`), "unicode");
    expectFile(await raw("GET", `/${FIXTURE}/%c3%bcn%c3%af%20c%c3%b6d%c3%a9.txt`), "unicode");
    expectFile(await raw("GET", `/${FIXTURE}/100%25.txt`), "percent");
    expectFile(await raw("GET", `/${FIXTURE}/plus+sign.txt`), "plus");
    expectFile(await raw("GET", `/${FIXTURE}/plus%2Bsign.txt`), "plus");
  });

  test("letters that need no encoding, encoded anyway", async () => {
    expectFile(await raw("GET", `/${FIXTURE}/%74%65%6E.txt`), "0123456789");
    expectFile(await raw("GET", `/${FIXTURE}/ten%2Etxt`), "0123456789");
  });

  test("an encoded slash is a slash", async () => {
    expectFile(await raw("GET", `/${FIXTURE}%2Fsub%2Fdeep%2Ffile.txt`), "deep");
    expectFile(await raw("GET", `/${FIXTURE}/sub%2f`), "<p>sub index</p>");
  });

  test("repeated slashes collapse", async () => {
    expectFile(await raw("GET", `//${FIXTURE}//sub///deep////file.txt`), "deep");
    expectFile(await raw("GET", `/${FIXTURE}/sub//`), "<p>sub index</p>");
  });

  test("dot segments are resolved before the path is looked up", async () => {
    expectFile(await raw("GET", `/${FIXTURE}/sub/../ten.txt`), "0123456789");
    expectFile(await raw("GET", `/${FIXTURE}/./sub/./deep/../deep/file.txt`), "deep");
    expectFile(await raw("GET", `/${FIXTURE}/sub/%2e%2e/ten.txt`), "0123456789");
    expectFile(await raw("GET", `/${FIXTURE}/sub/%2E%2E/ten.txt`), "0123456789");
    expectFile(await raw("GET", `/${FIXTURE}/sub/.%2e/ten.txt`), "0123456789");
    expectFile(await raw("GET", `/${FIXTURE}/%2e/ten.txt`), "0123456789");
    expectFile(await raw("GET", `/x/y/z/../../../${FIXTURE}/ten.txt`), "0123456789");
    // More parent segments than there are segments stop at the root.
    expectFile(await raw("GET", `/../../${FIXTURE}/ten.txt`), "0123456789");
  });

  test("a backslash is a slash", async () => {
    expectFile(await raw("GET", `/${FIXTURE}\\sub\\deep\\file.txt`), "deep");
    expectFile(await raw("GET", `/${FIXTURE}/sub\\..\\ten.txt`), "0123456789");
  });

  test("a query string is ignored", async () => {
    expectFile(await raw("GET", `/${FIXTURE}/ten.txt?download=1&x=../../etc/passwd`), "0123456789");
    expectFile(await raw("GET", `/${FIXTURE}/ten.txt?`), "0123456789");
  });

  test("an absolute-form request target", async () => {
    expectFile(await raw("GET", `http://elsewhere.example/${FIXTURE}/ten.txt`), "0123456789");
  });
});

describe("methods other than GET and HEAD outside the API", () => {
  test.each(["POST", "PUT", "DELETE", "PATCH", "OPTIONS", "TRACE", "PROPFIND", "PURGE"])(
    "%s is answered with a plain-text 404, file or not",
    async (method) => {
      for (const target of ["/", "/index.html", "/manifest.json", "/inbox", url("ten.txt"), "/.well-known/autoconfig/mail/config-v1.1.xml"]) {
        const response = await raw(method, target);
        expect({ method, target, status: response.status, body: response.body }).toEqual({
          method,
          target,
          status: 404,
          body: "Not Found",
        });
        expect(response.headers["content-type"]).toBe(TEXT_TYPE);
        expect(response.headers.etag).toBe('W/"9-0gXL1ngzMqISxa6S1zx3F4wtLyg"');
        expect(response.headers["content-length"]).toBe("9");
        expect(response.headers["cache-control"]).toBeUndefined();
        expect(response.headers["last-modified"]).toBeUndefined();
        expect(response.headers["accept-ranges"]).toBeUndefined();
        expect(securityHeadersOf(response.headers)).toEqual(expectedSecurityHeaders(false));
      }
    }
  );
});

describe("mail-client autoconfig", () => {
  const AUTOCONFIG = "/.well-known/autoconfig/mail/config-v1.1.xml";
  const xml = (domain: string, hostname: string) => `<?xml version="1.0" encoding="UTF-8"?>
<clientConfig version="1.1">
  <emailProvider id="${domain}">
    <domain>${domain}</domain>
    <displayName>${domain} Mail</displayName>
    <displayShortName>${domain}</displayShortName>
    <incomingServer type="imap">
      <hostname>${hostname}</hostname>
      <port>143</port>
      <socketType>STARTTLS</socketType>
      <authentication>password-cleartext</authentication>
      <username>%EMAILADDRESS%</username>
    </incomingServer>
    <outgoingServer type="smtp">
      <hostname>${hostname}</hostname>
      <port>587</port>
      <socketType>STARTTLS</socketType>
      <authentication>password-cleartext</authentication>
      <username>%EMAILADDRESS%</username>
    </outgoingServer>
  </emailProvider>
</clientConfig>`;

  test("is the exact document for the served domain and the app's host name", async () => {
    const response = await raw("GET", AUTOCONFIG);
    expect(response.status).toBe(200);
    expect(response.body).toBe(xml("mail.test", "app.mail.test"));
    expect(response.headers["content-type"]).toBe("application/xml; charset=utf-8");
    expect(response.headers["content-length"]).toBe(String(xml("mail.test", "app.mail.test").length));
    expect(response.headers.etag).toBe('W/"30d-SZFXhuBhHtIsyZcuIgQSPfszGNs"');
    // Built in memory, not a file: none of a file's headers.
    expect(response.headers["cache-control"]).toBeUndefined();
    expect(response.headers["last-modified"]).toBeUndefined();
    expect(response.headers["accept-ranges"]).toBeUndefined();
    expect(response.headers["set-cookie"]).toBeUndefined();
    expect(securityHeadersOf(response.headers)).toEqual(expectedSecurityHeaders(false));
  });

  test("answers with a trailing slash, in any letter case, with a query string, and HEAD", async () => {
    for (const target of [`${AUTOCONFIG}/`, "/.WELL-KNOWN/AutoConfig/Mail/CONFIG-V1.1.XML", `${AUTOCONFIG}?emailaddress=bob%40mail.test`]) {
      const response = await raw("GET", target);
      expect({ target, status: response.status }).toEqual({ target, status: 200 });
      expect(response.body).toBe(xml("mail.test", "app.mail.test"));
    }
    const head = await raw("HEAD", AUTOCONFIG);
    expect(head.status).toBe(200);
    expect(head.body).toBe("");
    expect(head.headers["content-type"]).toBe("application/xml; charset=utf-8");
    expect(head.headers["content-length"]).toBe("781");
  });

  test("is conditional on its ETag", async () => {
    const response = await raw("GET", AUTOCONFIG, { "If-None-Match": 'W/"30d-SZFXhuBhHtIsyZcuIgQSPfszGNs"' });
    expect(response.status).toBe(304);
    expect(response.body).toBe("");
    expect(response.headers["content-type"]).toBeUndefined();
    // There is no Last-Modified to compare a date with.
    const byDate = await raw("GET", AUTOCONFIG, { "If-Modified-Since": "Fri, 01 Jan 2100 00:00:00 GMT" });
    expect(byDate.status).toBe(200);
  });

  test("only this exact path: anything near it is the entry document", async () => {
    for (const target of [
      `${AUTOCONFIG}/extra`,
      `${AUTOCONFIG}//`,
      "/.well-known/autoconfig/mail/config-v1.1.xmlx",
      "/.well-known/autoconfig/mail/config-v1x1.xml",
      "/.well-known/autoconfig/mail/",
      "/.well-known/autoconfig/mail/config-v1.2.xml",
      "/x/.well-known/autoconfig/mail/config-v1.1.xml",
      "/.well-known/autoconfig/mail/config-v1.1%2Exml",
    ]) {
      expectIndexDocument(await raw("GET", target));
    }
  });

  test("uses the mail domain as the host name when APP_HOSTNAME is unset or empty", async () => {
    for (const APP_HOSTNAME of [undefined, ""]) {
      const bare = await startServer({ env: { APP_HOSTNAME } });
      try {
        const response = await rawHttp(bare.ports.http, rawRequest("GET", AUTOCONFIG));
        expect(response.status).toBe(200);
        expect(response.body).toBe(xml("mail.test", "mail.test"));
      } finally {
        await bare.stop();
      }
    }
  }, 120_000);

  test("names the configured domain and host", async () => {
    const other = await startServer({ env: { EMAIL_DOMAIN: "Example.ORG", APP_HOSTNAME: "Mail.Example.org:8443" } });
    try {
      const response = await rawHttp(other.ports.http, rawRequest("GET", AUTOCONFIG));
      expect(response.body).toBe(xml("Example.ORG", "Mail.Example.org:8443"));
    } finally {
      await other.stop();
    }
  }, 90_000);

  test("falls back to 'mydomain' when EMAIL_DOMAIN is unset or empty", async () => {
    for (const EMAIL_DOMAIN of [undefined, ""]) {
      const nameless = await startServer({ env: { EMAIL_DOMAIN, APP_HOSTNAME: undefined } });
      try {
        const response = await rawHttp(nameless.ports.http, rawRequest("GET", AUTOCONFIG));
        expect(response.body).toBe(xml("mydomain", "mydomain"));
      } finally {
        await nameless.stop();
      }
    }
  }, 120_000);
});
