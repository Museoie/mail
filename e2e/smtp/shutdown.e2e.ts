import { afterAll, afterEach, beforeAll, expect, test } from "bun:test";
import fs from "fs";
import net from "net";
import path from "path";
import { waitFor, type TestServer } from "../harness";
import { boot, closeClients, mailByMessageId, messageId, plain, session } from "./support";

// What open SMTP connections see when the server is told to stop (SIGTERM).
let server: TestServer;

beforeAll(async () => {
  server = await boot();
}, 180_000);

afterEach(closeClients);

afterAll(async () => {
  await server?.stop();
});

const SHUTTING_DOWN = "421 Server shutting down";

const connectError = (port: number): Promise<string> =>
  new Promise((resolve) => {
    const socket = net.createConnection({ port, host: "127.0.0.1" });
    socket.on("connect", () => {
      socket.destroy();
      resolve("connected");
    });
    socket.on("error", (error: NodeJS.ErrnoException) => resolve(error.code ?? "error"));
  });

test("on SIGTERM: commands get 421, a message in flight is still taken, idle connections are closed after five seconds, exit 0", async () => {
  const id = messageId("inflight");
  const idle = await session(server);
  const sending = await session(server);
  await sending.send("MAIL FROM:<bounce@remote.example>");
  await sending.send("RCPT TO:<hello@mail.test>");
  expect((await sending.send("DATA")).raw).toBe("354 End data with <CR><LF>.<CR><LF>");
  await sending.writeFlushed(plain({ messageId: id, subject: "Sent across a shutdown", body: "first half" }));

  const asking = await session(server);

  const started = Date.now();
  const exited = server.kill("SIGTERM");

  // The listener stops accepting at once.
  await waitFor(async () => (await connectError(server.ports.smtp)) === "ECONNREFUSED", {
    what: "the SMTP listener to close",
    timeoutMs: 10_000,
  });

  // From then on a command on an open connection is refused and the connection ended.
  expect((await asking.send("NOOP")).raw).toBe(SHUTTING_DOWN);
  expect(await asking.waitForClose(5000)).toBe(true);

  // A transaction already in DATA runs to its end and is stored.
  sending.write("second half\r\n.\r\n");
  expect((await sending.read()).raw).toBe("250 OK: message queued");
  expect((await mailByMessageId(server, id)).text).toBe("first half\nsecond half\n");
  // Its next command is refused like any other.
  expect((await sending.send("MAIL FROM:<bounce@remote.example>")).raw).toBe(SHUTTING_DOWN);
  expect(await sending.waitForClose(5000)).toBe(true);

  // A connection that says nothing is told 421 when the five-second grace runs out.
  expect((await idle.read(20_000)).raw).toBe(SHUTTING_DOWN);
  expect(await idle.waitForClose(5000)).toBe(true);
  expect(Date.now() - started).toBeGreaterThanOrEqual(4500);

  expect(await exited).toBe(0);
}, 40_000);

test("at boot, working files a previous process left behind are removed and stored attachments kept", async () => {
  const attachments = path.join(server.cwd, "attachments");
  const leftover = path.join(attachments, ".incoming", "11111111-2222-4333-8444-555555555555");
  fs.mkdirSync(leftover, { recursive: true });
  fs.writeFileSync(path.join(leftover, "0"), "half-written attachment");
  fs.writeFileSync(path.join(attachments, "kept-attachment"), "stored earlier");

  await server.restart();

  expect(fs.existsSync(path.join(attachments, ".incoming"))).toBe(false);
  expect(fs.readFileSync(path.join(attachments, "kept-attachment"), "utf8")).toBe("stored earlier");
  // And the restarted server takes mail again.
  const id = messageId("restart");
  const smtp = await session(server);
  const result = await smtp.transaction({ from: "bounce@remote.example", to: ["hello@mail.test"], data: plain({ messageId: id }) });
  expect(result.final.raw).toBe("250 OK: message queued");
}, 60_000);
