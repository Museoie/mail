import { afterAll, afterEach, beforeAll, expect, test } from "bun:test";
import type { TestServer } from "../harness";
import { boot, closeClients, ehloReply, plain, session } from "./support";

// A server started with no EMAIL_DOMAIN at all.
let server: TestServer;

beforeAll(async () => {
  server = await boot({ env: { EMAIL_DOMAIN: undefined } });
}, 180_000);

afterEach(closeClients);

afterAll(async () => {
  await server?.stop();
});

test("the session opens as usual", async () => {
  const smtp = await session(server);
  expect(smtp.transcript[1]!.raw).toBe(ehloReply());
});

test("every recipient is accepted, and every message then fails with a transient 450", async () => {
  // NOTE: the server falls back to the domain "mydomain" for its accounts,
  // but SMTP treats the missing setting as "not configured": no recipient
  // is checked, and DATA is refused with a code that invites a retry.
  const smtp = await session(server);
  expect((await smtp.send("MAIL FROM:<a@remote.example>")).raw).toBe("250 Accepted");
  for (const recipient of ["x@nobody.mail.test", "x@mydomain", "admin@mydomain", "x@nobody.mydomain", "x@elsewhere.example"]) {
    expect({ recipient, reply: (await smtp.rcptTo(recipient)).raw }).toEqual({ recipient, reply: "250 Accepted" });
  }
  expect((await smtp.send("DATA")).raw).toBe("354 End data with <CR><LF>.<CR><LF>");
  smtp.write(plain());
  // As with every refusal in DATA, the answer waits for the end of the message.
  expect(await smtp.staysQuiet(300)).toBe(true);
  smtp.write(".\r\n");
  expect((await smtp.read()).raw).toBe("450 Email service not configured");
  expect(await server.db.query("SELECT 1 FROM mails")).toEqual([]);
  expect((await smtp.send("NOOP")).raw).toBe("250 OK");
});
