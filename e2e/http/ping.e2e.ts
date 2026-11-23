import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { rawHttp, startServer, type TestServer } from "../harness";

let server: TestServer;

beforeAll(async () => {
  server = await startServer();
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

describe("GET /api/ping", () => {
  test("answers success without a session", async () => {
    const { status, body, headers, setCookies } = await server.client().get("/api/ping");
    expect(status).toBe(200);
    expect(body).toEqual({ status: "success" });
    expect(headers.get("content-type")).toBe("application/json; charset=utf-8");
    expect(setCookies).toEqual([]);
  });

  test("answers with a trailing slash and in any letter case", async () => {
    const client = server.client();
    for (const path of ["/api/ping/", "/API/PING", "/Api/Ping/"]) {
      const { status, body } = await client.get(path);
      expect({ path, status, body }).toEqual({ path, status: 200, body: { status: "success" } });
    }
  });

  test("answers HEAD with the headers of GET and no body", async () => {
    const client = server.client();
    const get = await client.get("/api/ping");
    const head = await client.head("/api/ping");
    expect(head.status).toBe(200);
    expect(head.text).toBe("");
    expect(head.headers.get("content-type")).toBe(get.headers.get("content-type"));
    expect(head.headers.get("etag")).toBe(get.headers.get("etag"));
  });

  test("is not answered for POST or DELETE", async () => {
    const client = server.client();
    for (const method of ["POST", "DELETE", "PUT", "PATCH"]) {
      const { status, body } = await client.request(method, "/api/ping");
      expect({ method, status, body }).toEqual({
        method,
        status: 404,
        body: { status: "failed", message: "Not found" },
      });
    }
  });

  test("does not match a longer path", async () => {
    const { status, body } = await server.client().get("/api/ping/extra");
    expect(status).toBe(404);
    expect(body).toEqual({ status: "failed", message: "Not found" });
  });

  test("ignores a query string", async () => {
    const { status, body } = await server.client().get("/api/ping?x=1&x=2");
    expect(status).toBe(200);
    expect(body).toEqual({ status: "success" });
  });

  test("carries a weak ETag over the exact body", async () => {
    const { headers, text } = await server.client().get("/api/ping");
    // {"status":"success"} is 20 bytes: 0x14.
    expect(text).toBe('{"status":"success"}');
    expect(headers.get("etag")).toBe('W/"14-Y53wuE/mmbSikKcT/WualL1N65U"');
  });

  test("answers over a raw connection", async () => {
    const response = await rawHttp(
      server.ports.http,
      "GET /api/ping HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    );
    expect(response.status).toBe(200);
    expect(response.body).toBe('{"status":"success"}');
    expect(response.headers["content-length"]).toBe("20");
  });
});
