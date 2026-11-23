import { afterAll, afterEach, beforeAll, describe, expect, test } from "bun:test";
import net from "net";
import { ADMIN_PASSWORD, createUser, type TestServer, type TestUser } from "../harness";
import { proxyLine } from "../harness/smtp";
import { boot, closeClients, open, type Listener } from "./support";

// The limiter counts failed sign-ins per client address: ten in fifteen
// minutes, each one answered half a second late. Behind the PROXY protocol the
// address is the one the proxy names, which is what lets each test here start
// from an empty count.
let server: TestServer;
let alice: TestUser;

beforeAll(async () => {
  server = await boot({ tls: true, env: { PROXY_PROTOCOL: "true" } });
  alice = await createUser(server, "alice");
  await createUser(server, "bob");
}, 180_000);

afterEach(closeClients);

afterAll(async () => {
  await server?.stop();
});

let nextAddress = 0;
const freshAddress = () => `10.99.0.${++nextAddress}`;

const connect = async (address: string, listener: Listener = "smtps") => {
  const client = await open(server, listener, { proxy: proxyLine(address) });
  await client.ehlo();
  return client;
};

const SUCCESS = "235 Authentication successful";
const INVALID = "535 Error: Authentication credentials invalid";
const LOCKED = "535 Too many failed authentication attempts";

describe("failed sign-ins from one address", () => {
  test("the eleventh attempt is refused without being checked, on every listener, whoever it names", async () => {
    const address = freshAddress();
    // Ten failures across two connections and three usernames, one of them unknown.
    const first = await connect(address);
    for (let i = 0; i < 5; i++) {
      expect((await first.authPlain(i % 2 ? "alice" : "nobody-here", "wrong")).raw).toBe(INVALID);
    }
    const second = await connect(address, "submission");
    for (let i = 0; i < 5; i++) {
      expect((await second.authLogin(i % 2 ? "bob" : "alice", "wrong")).raw).toBe(INVALID);
    }

    // The right password no longer helps, and the refusal comes at once.
    const started = Date.now();
    expect((await second.authPlain("alice", alice.password)).raw).toBe(LOCKED);
    expect((await second.authPlain("admin", ADMIN_PASSWORD)).raw).toBe(LOCKED);
    expect((await second.authPlain("never-tried-before", "x")).raw).toBe(LOCKED);
    expect((await second.authPlain("alice", "wrong")).raw).toBe(LOCKED);
    expect((await second.authPlain("bob", "wrong")).raw).toBe(LOCKED);
    // Five refusals with the half-second wait would take two and a half seconds.
    expect(Date.now() - started).toBeLessThan(2000);
    // AUTH LOGIN is turned away only once it has collected both answers.
    expect((await second.send("AUTH LOGIN")).raw).toBe("334 VXNlcm5hbWU6");
    expect((await second.send(Buffer.from("alice").toString("base64"))).raw).toBe("334 UGFzc3dvcmQ6");
    expect((await second.send(Buffer.from(alice.password).toString("base64"))).raw).toBe(LOCKED);

    // The connection is not closed, and everything but AUTH still works.
    expect((await second.send("NOOP")).raw).toBe("250 OK");

    // A new connection from the same address is still locked out, on each listener.
    for (const listener of ["smtps", "submission"] as const) {
      const again = await connect(address, listener);
      expect({ listener, reply: (await again.authPlain("alice", alice.password)).raw }).toEqual({ listener, reply: LOCKED });
    }
    const upgraded = await connect(address, "smtp");
    await upgraded.startTls();
    await upgraded.ehlo();
    expect((await upgraded.authPlain("alice", alice.password)).raw).toBe(LOCKED);

    // Another address is unaffected.
    const elsewhere = await connect(freshAddress());
    expect((await elsewhere.authPlain("alice", alice.password)).raw).toBe(SUCCESS);
  }, 30_000);

  test("nine failures still allow a sign-in, and that success does not clear the address's count", async () => {
    const address = freshAddress();
    const smtp = await connect(address);
    for (let i = 0; i < 9; i++) expect((await smtp.authPlain("alice", "wrong")).raw).toBe(INVALID);
    expect((await smtp.authPlain("alice", alice.password)).raw).toBe(SUCCESS);

    // One more failure, on another account, is the address's tenth.
    const next = await connect(address);
    expect((await next.authPlain("bob", "wrong")).raw).toBe(INVALID);
    expect((await next.authPlain("alice", alice.password)).raw).toBe(LOCKED);
  }, 30_000);

  test("exchanges that never reach a password check are not counted", async () => {
    const smtp = await connect(freshAddress());
    for (let i = 0; i < 4; i++) {
      expect((await smtp.send("AUTH PLAIN !!!!")).raw).toBe("500 Error: invalid userdata");
      await smtp.send("AUTH PLAIN");
      expect((await smtp.send("*")).raw).toBe("501 Authentication aborted");
      expect((await smtp.send("AUTH CRAM-MD5")).raw).toBe("504 Error: Unrecognized authentication type");
      expect((await smtp.send("AUTH LOGIN =")).raw).toBe("500 Error: missing username");
    }
    expect((await smtp.authPlain("alice", alice.password)).raw).toBe(SUCCESS);
  });
});

describe("the count is shared with IMAP", () => {
  /** Tries one IMAP LOGIN from `address` and returns the tagged reply line, or undefined if the server hung up. */
  const imapLogin = (address: string, username: string, password: string): Promise<string | undefined> =>
    new Promise((resolve, reject) => {
      const socket = net.createConnection({ port: server.ports.imap, host: "127.0.0.1" });
      let buffer = "";
      let sent = false;
      const timer = setTimeout(() => {
        socket.destroy();
        reject(new Error(`Timed out in the IMAP exchange: ${JSON.stringify(buffer)}`));
      }, 10_000);
      const finish = (line: string | undefined) => {
        clearTimeout(timer);
        socket.destroy();
        resolve(line);
      };
      socket.on("connect", () => socket.write(`${proxyLine(address, { destinationPort: 143 })}\r\n`));
      socket.on("data", (chunk) => {
        buffer += chunk.toString("latin1");
        if (!sent && buffer.includes("\r\n")) {
          sent = true;
          socket.write(`a1 LOGIN "${username}" "${password}"\r\n`);
        }
        const tagged = /^a1 [^\r\n]*/m.exec(buffer);
        if (tagged) finish(tagged[0]);
      });
      socket.on("close", () => finish(undefined));
      socket.on("error", () => finish(undefined));
    });

  test("ten failed IMAP logins lock SMTP AUTH out for that address", async () => {
    const address = freshAddress();
    for (let i = 0; i < 10; i++) {
      const reply = await imapLogin(address, "alice", "wrong");
      expect(reply).toMatch(/^a1 NO \[AUTHENTICATIONFAILED\]/);
    }
    const smtp = await connect(address);
    expect((await smtp.authPlain("alice", alice.password)).raw).toBe(LOCKED);
  }, 30_000);
});
