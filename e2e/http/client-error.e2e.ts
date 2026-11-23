import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { type Alarm, type ApiResult, type TestServer } from "../harness";
import { startServer } from "./core-helpers";
import { JSON_TYPE, NOT_FOUND } from "./core-helpers";

const TITLE = "🚨 Mail Server Error: Client JS Error";

let server: TestServer;

beforeAll(async () => {
  server = await startServer();
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

/**
 * The server sends only the first alarm of a kind per minute, so every case
 * that reads an alarm restarts the server first: a fresh process has sent none.
 */
const freshServer = async () => {
  await server.restart();
  server.alarms.reset();
};

const post = (body: unknown): Promise<ApiResult> => server.client().post("/api/client-error", body);

/** Posts one report to a fresh process and returns the alarm it raised. */
const alarmFor = async (body: unknown): Promise<Alarm> => {
  await freshServer();
  const result = await post(body);
  expect(result.status).toBe(200);
  expect(result.text).toBe('{"status":"success"}');
  // The alarm is delivered before the report is answered.
  expect(server.alarms.alarms).toHaveLength(1);
  return server.alarms.alarms[0]!;
};

const descriptionFor = async (body: unknown) => (await alarmFor(body)).description;

describe("POST /api/client-error", () => {
  test("answers success and forwards the report as one alarm", async () => {
    await freshServer();
    const before = Date.now();
    const result = await post({
      message: "TypeError: x is not a function",
      stack: "TypeError: x is not a function\n    at render (https://app.mail.test/assets/index.js:10:5)",
      url: "https://app.mail.test/inbox",
      filename: "https://app.mail.test/assets/index.js",
      lineno: 10,
      colno: 5,
    });
    expect(result.status).toBe(200);
    expect(result.text).toBe('{"status":"success"}');
    expect(result.headers.get("content-type")).toBe(JSON_TYPE);
    expect(result.setCookies).toEqual([]);

    expect(server.alarms.alarms).toHaveLength(1);
    const alarm = server.alarms.alarms[0]!;
    expect(Object.keys(alarm)).toEqual(["title", "description", "color", "timestamp"]);
    expect(alarm.title).toBe(TITLE);
    expect(alarm.description).toBe(
      "**URL:** https://app.mail.test/inbox\n" +
        "**Message:** TypeError: x is not a function\n" +
        "**Source:** https://app.mail.test/assets/index.js:10:5\n" +
        "```\nTypeError: x is not a function\n    at render (https://app.mail.test/assets/index.js:10:5)\n```"
    );
    expect(alarm.color).toBe(0xff0000);
    expect(alarm.timestamp).toMatch(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/);
    expect(Date.parse(alarm.timestamp!)).toBeGreaterThanOrEqual(before - 1000);
    expect(Date.parse(alarm.timestamp!)).toBeLessThanOrEqual(Date.now() + 1000);
  });

  test("sends only the first alarm of a minute; later reports are still accepted", async () => {
    await freshServer();
    for (const message of ["first", "second", "third"]) {
      const result = await post({ message });
      expect(result.body).toEqual({ status: "success" });
    }
    expect(server.alarms.alarms.map((alarm) => alarm.description)).toEqual(["**Message:** first"]);
  });

  test("needs no session, and does not touch one", async () => {
    await freshServer();
    const result = await post({ message: "anonymous" });
    expect(result.status).toBe(200);
    expect(result.setCookies).toEqual([]);
    expect(await server.db.query("SELECT 1 FROM sessions")).toEqual([]);
  });

  test("is answered for POST at exactly this path, in any letter case", async () => {
    await freshServer();
    for (const path of ["/api/client-error/", "/API/Client-Error", "/api/client-error?from=beacon"]) {
      const result = await server.client().post(path, { message: path });
      expect({ path, status: result.status, body: result.body }).toEqual({ path, status: 200, body: { status: "success" } });
    }
    await freshServer();
    for (const [method, path] of [
      ["GET", "/api/client-error"],
      ["HEAD", "/api/client-error"],
      ["PUT", "/api/client-error"],
      ["DELETE", "/api/client-error"],
      ["POST", "/api/client-error/extra"],
      ["POST", "/api/client-errors"],
    ] as const) {
      const result = await server.client().request(method, path, method === "POST" ? { json: {} } : {});
      expect({ method, path, status: result.status }).toEqual({ method, path, status: 404 });
      if (method !== "HEAD") expect(result.body).toEqual(NOT_FOUND);
    }
    expect(server.alarms.alarms).toEqual([]);
  });

  test("answers success when the alarm cannot be delivered or no webhook is set", async () => {
    await server.restart({ DISCORD_ALARM_WEBHOOK: "http://127.0.0.1:1/unreachable" });
    server.alarms.reset();
    expect((await post({ message: "nobody hears this" })).body).toEqual({ status: "success" });

    await server.restart({ DISCORD_ALARM_WEBHOOK: undefined });
    expect((await post({ message: "nor this" })).body).toEqual({ status: "success" });
    await server.restart({ DISCORD_ALARM_WEBHOOK: "" });
    expect((await post({ message: "nor this" })).body).toEqual({ status: "success" });

    expect(server.alarms.alarms).toEqual([]);
    await server.restart({ DISCORD_ALARM_WEBHOOK: server.alarms.url });
  }, 60_000);
});

describe("POST /api/client-error — the body it is given", () => {
  test("an empty object", async () => {
    expect(await descriptionFor({})).toBe("**Message:** (no message)");
  });

  test("no body at all", async () => {
    await freshServer();
    const result = await server.client().request("POST", "/api/client-error");
    expect(result.body).toEqual({ status: "success" });
    expect(server.alarms.alarms.map((alarm) => alarm.description)).toEqual(["**Message:** (no message)"]);
  });

  test("an array", async () => {
    expect(await descriptionFor([{ message: "inside an array" }])).toBe("**Message:** (no message)");
  });

  test("a beacon's text/plain body is not read", async () => {
    await freshServer();
    const result = await server.client().request("POST", "/api/client-error", {
      body: JSON.stringify({ message: "sent as text" }),
      headers: { "Content-Type": "text/plain;charset=UTF-8" },
    });
    expect(result.body).toEqual({ status: "success" });
    expect(server.alarms.alarms.map((alarm) => alarm.description)).toEqual(["**Message:** (no message)"]);
  });

  test("a beacon's JSON blob is read", async () => {
    await freshServer();
    const result = await server.client().request("POST", "/api/client-error", {
      body: new Blob([JSON.stringify({ message: "sent as a blob" })], { type: "application/json" }),
    });
    expect(result.body).toEqual({ status: "success" });
    expect(server.alarms.alarms.map((alarm) => alarm.description)).toEqual(["**Message:** sent as a blob"]);
  });

  test("only a message", async () => {
    expect(await descriptionFor({ message: "Script error." })).toBe("**Message:** Script error.");
  });

  test("every field of the wrong type is treated as absent", async () => {
    expect(
      await descriptionFor({ message: 42, stack: ["at x"], url: { href: "https://x" }, filename: 7, lineno: "3", colno: "4" })
    ).toBe("**Message:** (no message)");
    expect(await descriptionFor({ message: null, stack: null, url: null, filename: null, lineno: null, colno: null })).toBe(
      "**Message:** (no message)"
    );
    expect(await descriptionFor({ message: true, stack: false, url: 0, filename: [], lineno: true, colno: {} })).toBe(
      "**Message:** (no message)"
    );
  });

  test("empty strings: an empty message is kept, the other fields are left out", async () => {
    expect(await descriptionFor({ message: "", stack: "", url: "", filename: "", lineno: 1, colno: 2 })).toBe("**Message:** ");
  });

  test("text is passed through as it is", async () => {
    const message = "`code` **bold** <b>html</b> @everyone \"quotes\" \\ backslash\ttab\nnewline ünï 日本 🔥";
    expect(await descriptionFor({ message, url: "javascript:alert(1)//\n**Message:** forged" })).toBe(
      `**URL:** javascript:alert(1)//\n**Message:** forged\n**Message:** ${message}`
    );
  });

  test("fields the route does not know are ignored", async () => {
    expect(await descriptionFor({ message: "m", userAgent: "x", extra: { deep: [1, 2] } })).toBe("**Message:** m");
  });
});

describe("POST /api/client-error — the source location", () => {
  const source = async (fields: Record<string, unknown>) => {
    const description = await descriptionFor({ message: "m", ...fields });
    const line = description.split("\n").find((entry) => entry.startsWith("**Source:** "));
    return line?.slice("**Source:** ".length);
  };

  test("a file name alone", async () => {
    expect(await source({ filename: "app.js" })).toBe("app.js");
  });

  test("a file name and a line", async () => {
    expect(await source({ filename: "app.js", lineno: 12 })).toBe("app.js:12");
  });

  test("a file name, a line and a column", async () => {
    expect(await source({ filename: "app.js", lineno: 12, colno: 34 })).toBe("app.js:12:34");
  });

  test("a column without a line is dropped", async () => {
    expect(await source({ filename: "app.js", colno: 34 })).toBe("app.js");
    expect(await source({ filename: "app.js", lineno: "12", colno: 34 })).toBe("app.js");
  });

  test("a line without a file name gives no source at all", async () => {
    expect(await source({ lineno: 12, colno: 34 })).toBeUndefined();
    expect(await source({ filename: "", lineno: 12, colno: 34 })).toBeUndefined();
  });

  test("zero is a line and a column like any other", async () => {
    expect(await source({ filename: "app.js", lineno: 0, colno: 0 })).toBe("app.js:0:0");
  });

  test("a column that is not a number is dropped", async () => {
    expect(await source({ filename: "app.js", lineno: 12, colno: "34" })).toBe("app.js:12");
    expect(await source({ filename: "app.js", lineno: 12, colno: null })).toBe("app.js:12");
  });

  test.each([
    ["-3", "-7", "-3:-7"],
    ["1.5", "2.25", "1.5:2.25"],
    ["1.0", "2.00", "1:2"],
    ["-0", "-0.0", "0:0"],
    ["1e3", "1E+2", "1000:100"],
    ["1e21", "1e-7", "1e+21:1e-7"],
    ["123456789012345678901", "0.000001", "123456789012345680000:0.000001"],
    ["9007199254740993", "0.1", "9007199254740992:0.1"],
    ["1.7976931348623157e308", "5e-324", "1.7976931348623157e+308:5e-324"],
    ["0.30000000000000004", "100000000000000000000", "0.30000000000000004:100000000000000000000"],
    ["1e400", "-1e400", "Infinity:-Infinity"],
  ])("numbers print as JavaScript prints them: %s and %s", async (lineno, colno, expected) => {
    await freshServer();
    const result = await server.client().request("POST", "/api/client-error", {
      body: `{"message":"m","filename":"app.js","lineno":${lineno},"colno":${colno}}`,
      headers: { "Content-Type": "application/json" },
    });
    expect(result.body).toEqual({ status: "success" });
    expect(server.alarms.alarms.map((alarm) => alarm.description)).toEqual([`**Message:** m\n**Source:** app.js:${expected}`]);
  });
});

describe("POST /api/client-error — what is cut", () => {
  test("a stack of 1000 characters is sent whole", async () => {
    const stack = "s".repeat(1000);
    expect(await descriptionFor({ message: "m", stack })).toBe(`**Message:** m\n\`\`\`\n${stack}\n\`\`\``);
  });

  test("a longer stack is cut at 1000 characters", async () => {
    const stack = `${"s".repeat(999)}XY${"z".repeat(500)}`;
    expect(await descriptionFor({ message: "m", stack })).toBe(`**Message:** m\n\`\`\`\n${"s".repeat(999)}X\n\`\`\``);
  });

  test("the 1000 are UTF-16 units", async () => {
    // 500 two-unit characters fill the limit exactly; the 501st is cut off.
    const stack = `${"🔥".repeat(500)}🔥tail`;
    expect(await descriptionFor({ message: "m", stack })).toBe(`**Message:** m\n\`\`\`\n${"🔥".repeat(500)}\n\`\`\``);
    // Three-byte characters count one each.
    const wide = "日".repeat(1200);
    expect(await descriptionFor({ message: "m", stack: wide })).toBe(`**Message:** m\n\`\`\`\n${"日".repeat(1000)}\n\`\`\``);
  });

  test("a cut through the middle of a character leaves its first half", async () => {
    const stack = `${"s".repeat(999)}🔥tail`;
    const description = await descriptionFor({ message: "m", stack });
    expect(description).toBe(`**Message:** m\n\`\`\`\n${"s".repeat(999)}\ud83d\n\`\`\``);
  });

  test("the whole description is cut at 4000 characters", async () => {
    const url = `https://app.mail.test/${"p".repeat(5000)}`;
    const description = await descriptionFor({ message: "never shown", url, stack: "never shown" });
    expect(description).toHaveLength(4000);
    expect(description).toBe(`**URL:** ${url}`.slice(0, 4000));
  });

  test("a message is not cut on its own, only with the description", async () => {
    const message = "m".repeat(3980);
    const description = await descriptionFor({ message, filename: "a-file-name-that-runs-over.js" });
    expect(description).toHaveLength(4000);
    expect(description).toBe(`**Message:** ${message}\n**Source:** a-file-name-that-runs-over.js`.slice(0, 4000));
  });
});
