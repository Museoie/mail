/**
 * `POST /api/auth/events`: the signed events the provider posts, what each
 * does to sessions and accounts, and how a body that is not a verified event
 * is refused.
 */
import { afterAll, beforeAll, beforeEach, describe, expect, test } from "bun:test";
import zlib from "zlib";
import {
  createUser,
  rawHttp,
  startServer,
  type ApiClient,
  type ApiResult,
  type TestServer,
} from "../harness";
import { person, signIn, startIssuer, type FakeIssuer, type Identity } from "../harness/issuer";

const ACCEPTED = { status: "success" };
const INVALID = { status: "failed", message: "Invalid event." };
const TOO_LARGE = { status: "failed", message: "request entity too large" };
const UNAVAILABLE = { status: "error", message: "Try again later." };
const LIMIT = 64 * 1024;

const nowS = () => Math.floor(Date.now() / 1000);

const postEvent = (
  server: TestServer,
  body: BodyInit,
  headers: Record<string, string> = { "Content-Type": "application/secevent+jwt" },
  path = "/api/auth/events"
) => server.client().request("POST", path, { body, headers });

const answerOf = ({ status, body }: ApiResult) => ({ status, body });

describe("POST /api/auth/events", () => {
  let issuer: FakeIssuer;
  let server: TestServer;

  beforeAll(async () => {
    issuer = await startIssuer();
    server = await startServer({ env: issuer.env({ AUTH_LOCAL_LOGIN: "true" }) });
    // The first verified event fetches the provider's keys; from here on they are known.
    const warmUp = await postEvent(server, issuer.eventToken({ "something.new": {} }));
    expect(answerOf(warmUp)).toEqual({ status: 202, body: ACCEPTED });
  }, 90_000);
  afterAll(async () => {
    await server?.stop();
    await issuer?.stop();
  });

  beforeEach(async () => {
    await server.db.query("DELETE FROM sessions");
    await server.db.query("DELETE FROM users WHERE username <> 'admin'");
    issuer.resetReplies();
    issuer.clearCalls();
  });

  const signedIn = async (who: Identity): Promise<ApiClient> => {
    const client = server.client();
    const { done } = await signIn(client, issuer, { who });
    expect(done.headers.get("location")).toBe("/");
    return client;
  };
  const whoAmI = async (client: ApiClient) => (await client.get("/api/users/login")).body.body.user?.username;
  const sessionSids = async () =>
    (
      await server.db.query<{ session_username: string; auth_sid: string | null }>(
        "SELECT session_username, auth_sid FROM sessions"
      )
    )
      .map((row) => `${row.session_username}:${row.auth_sid}`)
      .sort();
  const versionOf = async (username: string) =>
    (await server.db.query<{ password_version: number }>("SELECT password_version FROM users WHERE username = $1", [username]))[0]
      ?.password_version;

  /** Posts one event and asserts the provider would take it as delivered. */
  const deliver = async (events: unknown, claims: Record<string, unknown> = {}) => {
    const result = await postEvent(server, issuer.eventToken(events, claims));
    expect(answerOf(result)).toEqual({ status: 202, body: ACCEPTED });
    return result;
  };

  /**
   * Bob, who signed up here with a password and was then linked by his email:
   * one password session and two single sign-on sessions from two provider
   * sessions. Alice, with one single sign-on session, is the bystander.
   */
  const scene = async () => {
    const bob = await createUser(server, "bob", { email: "bob@people.example" });
    const password = server.client();
    await password.login(bob.username, bob.password);
    const laptop = await signedIn(person("bob", { sid: "ses_bob_laptop" }));
    const phone = await signedIn(person("bob", { sid: "ses_bob_phone" }));
    const alice = await signedIn(person("alice"));
    expect(await sessionSids()).toEqual(["alice:ses_alice", "bob:null", "bob:ses_bob_laptop", "bob:ses_bob_phone"]);
    return { bob, password, laptop, phone, alice };
  };

  describe("answers", () => {
    test("202 for a verified event, with no session and no cookie", async () => {
      const result = await postEvent(server, issuer.eventToken({ "session.ended": { sid: "ses_nobody" } }));
      expect(result.status).toBe(202);
      expect(result.body).toEqual(ACCEPTED);
      expect(result.headers.get("content-type")).toBe("application/json; charset=utf-8");
      expect(result.setCookies).toEqual([]);
      expect(await server.db.query("SELECT 1 FROM sessions")).toEqual([]);
      // The keys were fetched once, at the first event; nothing is asked per event.
      expect(issuer.calls).toEqual([]);
    });

    test("whatever content type the body claims, or none", async () => {
      const token = () => issuer.eventToken({ "session.ended": { sid: "ses_nobody" } });
      for (const contentType of [
        "application/secevent+jwt",
        "application/json",
        "application/json; charset=utf-16",
        "text/plain",
        "application/x-www-form-urlencoded",
        "multipart/form-data; boundary=x",
      ]) {
        const result = await postEvent(server, token(), { "Content-Type": contentType });
        expect({ contentType, ...answerOf(result) }).toEqual({ contentType, status: 202, body: ACCEPTED });
      }
      const raw = token();
      const bare = await rawHttp(
        server.ports.http,
        `POST /api/auth/events HTTP/1.1\r\nHost: localhost\r\nContent-Length: ${raw.length}\r\nConnection: close\r\n\r\n${raw}`
      );
      expect({ status: bare.status, body: bare.body }).toEqual({ status: 202, body: JSON.stringify(ACCEPTED) });
    });

    test("in any letter case and with a trailing slash, and only to POST", async () => {
      for (const path of ["/api/auth/events/", "/API/AUTH/EVENTS", "/Api/Auth/Events/"]) {
        const result = await postEvent(
          server,
          issuer.eventToken({ "session.ended": { sid: "ses_nobody" } }),
          { "Content-Type": "application/json" },
          path
        );
        expect({ path, ...answerOf(result) }).toEqual({ path, status: 202, body: ACCEPTED });
      }
      for (const method of ["GET", "DELETE", "PUT"]) {
        const result = await server.client().request(method, "/api/auth/events");
        expect({ method, ...answerOf(result) }).toEqual({
          method,
          status: 404,
          body: { status: "failed", message: "Not found" },
        });
      }
    });

    test("a token with white space around it", async () => {
      const token = issuer.eventToken({ "session.ended": { sid: "ses_nobody" } });
      const result = await postEvent(server, `\r\n  ${token}\n\t `);
      expect(answerOf(result)).toEqual({ status: 202, body: ACCEPTED });
    });

    test("an event with no expiry, an expiry long past, or issued long ago: only its signature dates it", async () => {
      await deliver({ "session.ended": { sid: "ses_nobody" } }, { exp: undefined });
      await deliver({ "session.ended": { sid: "ses_nobody" } }, { exp: 1 });
      await deliver({ "session.ended": { sid: "ses_nobody" } }, { iat: nowS() - 365 * 24 * 3600 });
      await deliver({ "session.ended": { sid: "ses_nobody" } }, { iat: nowS() + 15 });
      await deliver({ "session.ended": { sid: "ses_nobody" } }, { aud: ["another-service", "mail"] });
    });
  });

  describe("session.ended", () => {
    test("deletes the sessions started from that provider session, and no others", async () => {
      const { password, laptop, phone, alice } = await scene();
      await deliver({ "session.ended": { sid: "ses_bob_laptop" } });

      expect(await sessionSids()).toEqual(["alice:ses_alice", "bob:null", "bob:ses_bob_phone"]);
      expect(await whoAmI(laptop)).toBeUndefined();
      expect(await whoAmI(phone)).toBe("bob");
      expect(await whoAmI(password)).toBe("bob");
      expect(await whoAmI(alice)).toBe("alice");
      // Nothing is revoked: the account's credential generation stays.
      expect(await versionOf("bob")).toBe(1);
    });

    test("deletes every session of that provider session", async () => {
      const first = await signedIn(person("alice"));
      const second = await signedIn(person("alice"));
      const other = await signedIn(person("alice", { sid: "ses_alice_other" }));
      await deliver({ "session.ended": { sid: "ses_alice" } });
      expect(await sessionSids()).toEqual(["alice:ses_alice_other"]);
      expect(await whoAmI(first)).toBeUndefined();
      expect(await whoAmI(second)).toBeUndefined();
      expect(await whoAmI(other)).toBe("alice");
    });

    test("is acknowledged when no session has that provider session", async () => {
      await scene();
      await deliver({ "session.ended": { sid: "ses_nobody" } });
      await deliver({ "session.ended": { sid: "" } });
      await deliver({ "session.ended": { sid: "ses_bob_laptop", sub: "usr_alice", extra: [1] } });
      expect(await sessionSids()).toEqual(["alice:ses_alice", "bob:null", "bob:ses_bob_phone"]);
    });
  });

  describe("revoking a linked account", () => {
    const revoking: [string, (sub: string) => unknown][] = [
      ["access.changed with the role removed", (sub) => ({ "access.changed": { sub, role: null } })],
      ["user.disabled", (sub) => ({ "user.disabled": { sub } })],
      ["user.deleted", (sub) => ({ "user.deleted": { sub } })],
      ["credential.revoked", (sub) => ({ "credential.revoked": { sub, id: "apw_1" } })],
    ];

    for (const [label, events] of revoking) {
      test(`${label} moves the credential generation on and deletes every session of the account`, async () => {
        const { bob, password, laptop, phone, alice } = await scene();
        await deliver(events("usr_bob"));

        expect(await versionOf("bob")).toBe(2);
        expect(await sessionSids()).toEqual(["alice:ses_alice"]);
        for (const client of [password, laptop, phone]) expect(await whoAmI(client)).toBeUndefined();
        expect(await whoAmI(alice)).toBe("alice");
        expect(await versionOf("alice")).toBe(1);

        // The account itself is untouched: still linked, still there.
        const [row] = await server.db.query("SELECT auth_sub, is_deleted, email FROM users WHERE username = 'bob'");
        expect(row).toEqual({ auth_sub: "usr_bob", is_deleted: false, email: bob.email });
        // A new sign-in starts on the new generation.
        const back = await signedIn(person("bob"));
        expect((await back.get("/api/users/login")).body.body.user.passwordVersion).toBe(2);
      });
    }

    test("a session written back with the old generation is refused when it is next read", async () => {
      const { laptop } = await scene();
      const [row] = await server.db.query("SELECT * FROM sessions WHERE auth_sid = 'ses_bob_laptop'");
      await deliver({ "user.disabled": { sub: "usr_bob" } });
      // What a request in flight during the event would leave behind.
      const columns = Object.keys(row!);
      await server.db.query(
        `INSERT INTO sessions (${columns.join(", ")}) VALUES (${columns.map((_, i) => `$${i + 1}`).join(", ")})`,
        columns.map((column) => row![column])
      );
      expect(await whoAmI(laptop)).toBeUndefined();
      expect(await sessionSids()).toEqual(["alice:ses_alice"]);
    });

    test("is acknowledged, and changes nothing, for a subject no account is linked to", async () => {
      await scene();
      for (const [, events] of revoking) await deliver(events("usr_nobody"));
      await deliver({ "user.disabled": { sub: "" } });
      expect(await sessionSids()).toEqual(["alice:ses_alice", "bob:null", "bob:ses_bob_laptop", "bob:ses_bob_phone"]);
      expect(await versionOf("bob")).toBe(1);
      expect(await versionOf("alice")).toBe(1);
    });

    test("delivered again, it is applied again", async () => {
      await scene();
      const token = issuer.eventToken({ "user.disabled": { sub: "usr_bob" } }, { jti: "evt_same" });
      for (const expectedVersion of [2, 3, 4]) {
        expect(answerOf(await postEvent(server, token))).toEqual({ status: 202, body: ACCEPTED });
        expect(await versionOf("bob")).toBe(expectedVersion);
      }
      expect(await sessionSids()).toEqual(["alice:ses_alice"]);
    });
  });

  describe("events that change nothing", () => {
    test("access.changed to another role: any role grants the whole app", async () => {
      await scene();
      await deliver({ "access.changed": { sub: "usr_bob", role: "admin" } });
      await deliver({ "access.changed": { sub: "usr_bob", role: "" } });
      expect(await sessionSids()).toEqual(["alice:ses_alice", "bob:null", "bob:ses_bob_laptop", "bob:ses_bob_phone"]);
      expect(await versionOf("bob")).toBe(1);
    });

    test("invite.accepted, and types this server does not know", async () => {
      await scene();
      await deliver({ "invite.accepted": { invite: "inv_1", sub: "usr_bob" } });
      await deliver({ "something.new": { sub: "usr_bob", sid: "ses_bob_laptop" } });
      await deliver({ "user.disabled.v2": { sub: "usr_bob" } });
      await deliver({ "USER.DISABLED": { sub: "usr_bob" } });
      // An unknown type needs none of the fields a known one does.
      await deliver({ "https://schemas.example/event": {} });
      expect(await sessionSids()).toEqual(["alice:ses_alice", "bob:null", "bob:ses_bob_laptop", "bob:ses_bob_phone"]);
      expect(await versionOf("bob")).toBe(1);
    });
  });

  // NOTE: an event type that happens to be the name of a member every
  // JavaScript object has ("constructor", "toString", "__proto__", …) is not
  // acknowledged as an unknown type. The server looks the type up in a plain
  // object, finds that member instead of a list of required fields, and the
  // route fails: 500, and the provider keeps redelivering.
  describe("an event type named like a built-in object member", () => {
    test("is answered 500 rather than acknowledged", async () => {
      await scene();
      for (const type of ["constructor", "toString", "valueOf", "hasOwnProperty", "__proto__"]) {
        const events = JSON.parse(`{"${type}":{"sub":"usr_bob","sid":"ses_bob_laptop"}}`);
        const result = await postEvent(server, issuer.eventToken(events));
        expect({ type, ...answerOf(result) }).toEqual({
          type,
          status: 500,
          body: { status: "error", message: "Internal server error" },
        });
      }
      expect(await sessionSids()).toHaveLength(4);
      expect(await versionOf("bob")).toBe(1);
      // With a payload that is not an object it is refused like any other malformed event.
      const malformed = await postEvent(server, issuer.eventToken({ constructor: "usr_bob" }));
      expect(answerOf(malformed)).toEqual({ status: 400, body: INVALID });
    });
  });

  describe("refuses with 400", () => {
    const expectInvalid = async (body: string, label: string) => {
      const result = await postEvent(server, body);
      expect({ label, ...answerOf(result) }).toEqual({ label, status: 400, body: INVALID });
    };

    test("a body that is not a token", async () => {
      for (const body of ["", "   ", "not-a-token", "a.b", "a.b.c.d", "{}", '{"events":{}}', "a b.c d.e f"]) {
        await expectInvalid(body, JSON.stringify(body));
      }
      const twoTokens = `${issuer.eventToken({ "session.ended": { sid: "s" } })}\n${issuer.eventToken({ "session.ended": { sid: "s" } })}`;
      await expectInvalid(twoTokens, "two tokens");
    });

    test("a token that is not a verified event for this app", async () => {
      await scene();
      const event = { "user.disabled": { sub: "usr_bob" } };
      const forgeries: [string, string][] = [
        ["wrong audience", issuer.eventToken(event, { aud: "another-service" })],
        ["no audience", issuer.eventToken(event, { aud: undefined })],
        ["wrong issuer", issuer.eventToken(event, { iss: "https://someone-else.e2e.test" })],
        ["bad signature", issuer.eventToken(event, {}, { tamper: true })],
        ["unpublished key", issuer.eventToken(event, {}, { kid: "unpublished" })],
        ["no kid", issuer.eventToken(event, {}, { header: { kid: undefined } })],
        ["typ JWT", issuer.eventToken(event, {}, { header: { typ: "JWT" } })],
        ["no typ", issuer.eventToken(event, {}, { header: { typ: undefined } })],
        ["alg none", issuer.eventToken(event, {}, { header: { alg: "none" } })],
        ["alg HS256", issuer.eventToken(event, {}, { header: { alg: "HS256" } })],
        ["critical header", issuer.eventToken(event, {}, { header: { crit: ["events"] } })],
        ["issued in the future", issuer.eventToken(event, { iat: nowS() + 60 })],
        ["no iat", issuer.eventToken(event, { iat: undefined })],
        ["an access token", issuer.accessToken(person("bob"))],
        ["an ID token", issuer.idToken(person("bob"))],
      ];
      for (const [label, token] of forgeries) await expectInvalid(token, label);
      expect(await versionOf("bob")).toBe(1);
      expect(await sessionSids()).toHaveLength(4);
    });

    test("an event token that is not one well-formed event", async () => {
      await scene();
      const malformed: [string, string][] = [
        ["no jti", issuer.eventToken({ "user.disabled": { sub: "usr_bob" } }, { jti: undefined })],
        ["empty jti", issuer.eventToken({ "user.disabled": { sub: "usr_bob" } }, { jti: "" })],
        ["numeric jti", issuer.eventToken({ "user.disabled": { sub: "usr_bob" } }, { jti: 7 })],
        ["no events", issuer.eventToken(undefined)],
        ["events null", issuer.eventToken(null)],
        ["events a list", issuer.eventToken([{ "user.disabled": { sub: "usr_bob" } }])],
        ["events a string", issuer.eventToken("user.disabled")],
        ["no event", issuer.eventToken({})],
        [
          "two events",
          issuer.eventToken({ "user.disabled": { sub: "usr_bob" }, "session.ended": { sid: "ses_bob_phone" } }),
        ],
        ["payload null", issuer.eventToken({ "user.disabled": null })],
        ["payload a string", issuer.eventToken({ "user.disabled": "usr_bob" })],
        ["payload a list", issuer.eventToken({ "user.disabled": ["usr_bob"] })],
        ["unknown type with a payload that is not an object", issuer.eventToken({ "something.new": true })],
        ["session.ended without sid", issuer.eventToken({ "session.ended": { sub: "usr_bob" } })],
        ["session.ended with a numeric sid", issuer.eventToken({ "session.ended": { sid: 7 } })],
        ["access.changed without sub", issuer.eventToken({ "access.changed": { role: null } })],
        ["access.changed without role", issuer.eventToken({ "access.changed": { sub: "usr_bob" } })],
        ["access.changed with a numeric role", issuer.eventToken({ "access.changed": { sub: "usr_bob", role: 0 } })],
        ["user.disabled without sub", issuer.eventToken({ "user.disabled": {} })],
        ["user.deleted with a null sub", issuer.eventToken({ "user.deleted": { sub: null } })],
        ["credential.revoked without id", issuer.eventToken({ "credential.revoked": { sub: "usr_bob" } })],
        ["credential.revoked without sub", issuer.eventToken({ "credential.revoked": { id: "apw_1" } })],
        ["invite.accepted without invite", issuer.eventToken({ "invite.accepted": { sub: "usr_bob" } })],
        ["invite.accepted without sub", issuer.eventToken({ "invite.accepted": { invite: "inv_1" } })],
      ];
      for (const [label, token] of malformed) await expectInvalid(token, label);
      expect(await versionOf("bob")).toBe(1);
      expect(await sessionSids()).toHaveLength(4);
    });
  });

  describe("the request body", () => {
    test("is refused with 413 past 64 KiB, by its declared length or as it arrives", async () => {
      const declared = await postEvent(server, "e".repeat(LIMIT + 1));
      expect(answerOf(declared)).toEqual({ status: 413, body: TOO_LARGE });

      // Exactly at the limit it is read, and then is not an event.
      const atLimit = await postEvent(server, "e".repeat(LIMIT));
      expect(answerOf(atLimit)).toEqual({ status: 400, body: INVALID });

      // Without a declared length the bytes are counted as they come.
      const chunk = "e".repeat(8 * 1024);
      const chunks = Array.from({ length: 9 }, () => `${chunk.length.toString(16)}\r\n${chunk}\r\n`).join("");
      const streamed = await rawHttp(
        server.ports.http,
        `POST /api/auth/events HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n${chunks}0\r\n\r\n`
      );
      expect({ status: streamed.status, body: JSON.parse(streamed.body) }).toEqual({ status: 413, body: TOO_LARGE });
    });

    test("may be gzip- or deflate-encoded", async () => {
      await scene();
      const token = issuer.eventToken({ "session.ended": { sid: "ses_bob_laptop" } });
      for (const [encoding, body] of [
        ["gzip", zlib.gzipSync(token)],
        ["GZIP", zlib.gzipSync(token)],
        ["deflate", zlib.deflateSync(token)],
        ["identity", Buffer.from(token)],
        ["Identity", Buffer.from(token)],
      ] as const) {
        const result = await postEvent(server, body, {
          "Content-Type": "application/secevent+jwt",
          "Content-Encoding": encoding,
        });
        expect({ encoding, ...answerOf(result) }).toEqual({ encoding, status: 202, body: ACCEPTED });
      }
      expect(await sessionSids()).toEqual(["alice:ses_alice", "bob:null", "bob:ses_bob_phone"]);
    });

    test("is refused with 415 in an encoding the server does not read", async () => {
      const token = issuer.eventToken({ "session.ended": { sid: "ses_nobody" } });
      for (const encoding of ["br", "zstd", "compress", "gzip, deflate", "x-gzip"]) {
        const result = await postEvent(server, token, {
          "Content-Type": "application/secevent+jwt",
          "Content-Encoding": encoding,
        });
        expect({ encoding, ...answerOf(result) }).toEqual({
          encoding,
          status: 415,
          body: { status: "failed", message: `unsupported content encoding "${encoding}"` },
        });
      }
    });

    test("is refused with 400 when it does not decode", async () => {
      const result = await postEvent(server, "this is not gzip", {
        "Content-Type": "application/secevent+jwt",
        "Content-Encoding": "gzip",
      });
      expect(answerOf(result)).toEqual({ status: 400, body: { status: "failed", message: "invalid request body" } });
    });

    test("is refused with 413 when it decodes to more than 64 KiB", async () => {
      const bomb = zlib.gzipSync("e".repeat(LIMIT + 1));
      expect(bomb.byteLength).toBeLessThan(1024);
      const result = await postEvent(server, bomb, {
        "Content-Type": "application/secevent+jwt",
        "Content-Encoding": "gzip",
      });
      expect(answerOf(result)).toEqual({ status: 413, body: TOO_LARGE });
    });

    test("with a declared length of zero is an empty body: not an event", async () => {
      const result = await server.client().request("POST", "/api/auth/events");
      expect(answerOf(result)).toEqual({ status: 400, body: INVALID });
    });
  });

  describe("while the provider is down", () => {
    test("events are still verified with the keys already fetched", async () => {
      await scene();
      await issuer.down();
      try {
        await deliver({ "session.ended": { sid: "ses_bob_laptop" } });
        // A key that was never seen cannot be fetched now: the token is refused, not deferred.
        const unknownKey = await postEvent(
          server,
          issuer.eventToken({ "session.ended": { sid: "ses_bob_phone" } }, {}, { kid: "k-late" })
        );
        expect(answerOf(unknownKey)).toEqual({ status: 400, body: INVALID });
      } finally {
        issuer.up();
      }
      expect(await sessionSids()).toEqual(["alice:ses_alice", "bob:null", "bob:ses_bob_phone"]);
    });
  });
});

describe("POST /api/auth/events before the provider's keys were ever fetched", () => {
  let issuer: FakeIssuer;
  let server: TestServer;

  beforeAll(async () => {
    issuer = await startIssuer();
    server = await startServer({ env: issuer.env() });
  }, 90_000);
  afterAll(async () => {
    await server?.stop();
    await issuer?.stop();
  });

  test("answers 503 while the provider cannot be reached, so the provider delivers again", async () => {
    await issuer.down();
    try {
      const result = await postEvent(server, issuer.eventToken({ "session.ended": { sid: "ses_nobody" } }));
      expect(answerOf(result)).toEqual({ status: 503, body: UNAVAILABLE });
      expect(result.headers.get("content-type")).toBe("application/json; charset=utf-8");
      expect(result.setCookies).toEqual([]);
    } finally {
      issuer.up();
    }
  });

  test("a body that is not a token is refused with 400 without needing the provider", async () => {
    await issuer.down();
    try {
      for (const body of ["", "not-a-token", issuer.eventToken({ "x.y": {} }, {}, { header: { alg: "none" } })]) {
        const result = await postEvent(server, body);
        expect(answerOf(result)).toEqual({ status: 400, body: INVALID });
      }
    } finally {
      issuer.up();
    }
  });

  // NOTE: the provider is reachable again here, but the server asks for the
  // key set at most once every thirty seconds, failed attempts included. So an
  // event delivered right after a failed fetch is answered 503 without the
  // provider being asked, and is only accepted on a later retry.
  test("stays 503 right after the failed key fetch, without asking the provider again", async () => {
    issuer.clearCalls();
    const result = await postEvent(server, issuer.eventToken({ "session.ended": { sid: "ses_nobody" } }));
    expect(answerOf(result)).toEqual({ status: 503, body: UNAVAILABLE });
    expect(issuer.calls).toEqual([]);
  });
});

describe("POST /api/auth/events when the key set cannot be fetched", () => {
  test("answers 503 for a key endpoint that fails", async () => {
    const issuer = await startIssuer();
    issuer.replyAlways("/.well-known/jwks.json", () => new Response("Bad Gateway", { status: 502 }));
    const server = await startServer({ env: issuer.env() });
    try {
      const result = await postEvent(server, issuer.eventToken({ "session.ended": { sid: "ses_nobody" } }));
      expect(answerOf(result)).toEqual({ status: 503, body: UNAVAILABLE });
      expect(issuer.calls.map((call) => call.path)).toEqual([
        "/.well-known/openid-configuration",
        "/.well-known/jwks.json",
      ]);
    } finally {
      await server.stop();
      await issuer.stop();
    }
  }, 90_000);

  test("answers 503 for a key endpoint that answers 404 or something that is not a key set", async () => {
    for (const answer of [() => new Response("Not Found", { status: 404 }), () => new Response("keys", { status: 200 })]) {
      const issuer = await startIssuer();
      issuer.replyAlways("/.well-known/jwks.json", answer);
      const server = await startServer({ env: issuer.env() });
      try {
        const result = await postEvent(server, issuer.eventToken({ "session.ended": { sid: "ses_nobody" } }));
        expect(answerOf(result)).toEqual({ status: 503, body: UNAVAILABLE });
      } finally {
        await server.stop();
        await issuer.stop();
      }
    }
  }, 120_000);

  // NOTE: a discovery document the client refuses outright (here: one naming
  // another issuer) is not an outage, so the event is answered 500, with the
  // generic error body, rather than 503.
  test("answers 500 when the discovery document is refused", async () => {
    const issuer = await startIssuer();
    issuer.discoveryOverrides.issuer = "https://someone-else.e2e.test";
    const server = await startServer({ env: issuer.env() });
    try {
      const result = await postEvent(server, issuer.eventToken({ "session.ended": { sid: "ses_nobody" } }));
      expect(answerOf(result)).toEqual({ status: 500, body: { status: "error", message: "Internal server error" } });
    } finally {
      await server.stop();
      await issuer.stop();
    }
  }, 90_000);
});
