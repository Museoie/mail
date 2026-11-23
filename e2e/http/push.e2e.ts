import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import crypto from "crypto";
import fs from "fs";
import os from "os";
import path from "path";
import webPush from "web-push";
import {
  ADMIN_PASSWORD,
  adminUser,
  bootExpectingExit,
  createUser,
  deliverMail,
  generateCertificate,
  missingId,
  rawHttp,
  type ApiClient,
  type TestServer,
  type TestUser,
} from "../harness";
import { startServer } from "./core-helpers";
import {
  BAD_REQUEST,
  INVALID_BODY,
  NON_STRINGS,
  NOT_FOUND,
  NOT_LOGGED_IN,
  UUID,
  isAbout,
  rawRequest,
} from "./core-helpers";

const NO_SUBSCRIPTION = { status: "failed", message: "No subscription found" };
const SUCCESS = { status: "success" };

/** A fresh VAPID pair; nothing like it is ever written to the repository. */
const vapid = webPush.generateVAPIDKeys();

let server: TestServer;
let alice: TestUser;
let bob: TestUser;
let aliceClient: ApiClient;
let bobClient: ApiClient;
/** Holds the viewer role on mail.test, which admin owns. */
let viewer: TestUser;
let viewerClient: ApiClient;

beforeAll(async () => {
  server = await startServer();
  alice = await createUser(server, "alice");
  bob = await createUser(server, "bob");
  viewer = await createUser(server, "vera");
  await server.db.query(
    `INSERT INTO domain_roles (domain_id, user_id, role)
     SELECT domain_id, $1, 'viewer' FROM domains WHERE name = 'mail.test'`,
    [viewer.id]
  );
  aliceClient = server.client();
  bobClient = server.client();
  viewerClient = server.client();
  await aliceClient.login(alice.username, alice.password);
  await bobClient.login(bob.username, bob.password);
  await viewerClient.login(viewer.username, viewer.password);
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

const subscription = (suffix = "x") => ({
  endpoint: `https://push.example.com/send/${suffix}`,
  keys: { p256dh: `p256dh-${suffix}`, auth: `auth-${suffix}` },
});

const subscribe = async (client: ApiClient, suffix: string): Promise<string> => {
  const { body } = await client.post("/api/push/subscribe", { subscription: subscription(suffix) });
  expect(body.status).toBe("success");
  return body.body as string;
};

const rowOf = async (id: string) => (await server.db.query("SELECT * FROM push_subscriptions WHERE push_subscription_id = $1", [id]))[0];
const rowCount = async () =>
  (await server.db.query("SELECT COUNT(*)::int AS count FROM push_subscriptions"))[0]!.count as number;

describe("GET /api/push/public-key", () => {
  const publicKeyText = async (env: Record<string, string | undefined>) => {
    const configured = await startServer({ env });
    try {
      const { status, text } = await configured.client().get("/api/push/public-key");
      expect(status).toBe(200);
      return text;
    } finally {
      await configured.stop();
    }
  };

  test("has no body field when no key is configured", async () => {
    const { status, text, setCookies } = await server.client().get("/api/push/public-key");
    expect(status).toBe(200);
    expect(text).toBe('{"status":"success"}');
    expect(setCookies).toEqual([]);
  });

  test("returns the public key when the pair is configured", async () => {
    expect(await publicKeyText({ PUSH_VAPID_PUBLIC_KEY: vapid.publicKey, PUSH_VAPID_PRIVATE_KEY: vapid.privateKey })).toBe(
      `{"status":"success","body":"${vapid.publicKey}"}`
    );
  }, 90_000);

  test("returns the public key even when the private key is missing or empty", async () => {
    expect(await publicKeyText({ PUSH_VAPID_PUBLIC_KEY: vapid.publicKey })).toBe(
      `{"status":"success","body":"${vapid.publicKey}"}`
    );
    expect(await publicKeyText({ PUSH_VAPID_PUBLIC_KEY: vapid.publicKey, PUSH_VAPID_PRIVATE_KEY: "" })).toBe(
      `{"status":"success","body":"${vapid.publicKey}"}`
    );
  }, 90_000);

  test("a lone public key is not validated at all", async () => {
    expect(await publicKeyText({ PUSH_VAPID_PUBLIC_KEY: "not a key!" })).toBe('{"status":"success","body":"not a key!"}');
  }, 90_000);

  test("has no body field when only the private key is configured", async () => {
    expect(await publicKeyText({ PUSH_VAPID_PRIVATE_KEY: vapid.privateKey })).toBe('{"status":"success"}');
  }, 90_000);

  // NOTE: an empty PUSH_VAPID_PUBLIC_KEY (as `.env.example` ships it) is
  // returned as an empty string rather than left out.
  test("returns an empty string for an empty public key", async () => {
    expect(await publicKeyText({ PUSH_VAPID_PUBLIC_KEY: "" })).toBe('{"status":"success","body":""}');
    expect(await publicKeyText({ PUSH_VAPID_PUBLIC_KEY: "", PUSH_VAPID_PRIVATE_KEY: vapid.privateKey })).toBe(
      '{"status":"success","body":""}'
    );
  }, 90_000);

  test("is public, and answers HEAD", async () => {
    const head = await server.client().head("/api/push/public-key");
    expect(head.status).toBe(200);
    expect(head.text).toBe("");
    expect(head.headers.get("content-length")).toBe("20");
  });

  test("answers an owner and a viewer the same", async () => {
    for (const client of [aliceClient, viewerClient]) {
      expect((await client.get("/api/push/public-key")).body).toEqual(SUCCESS);
    }
  });
});

describe("VAPID keys at boot", () => {
  const exitCodeWith = async (publicKey: string, privateKey: string) =>
    // A boot that is going to refuse the keys does so within a second or two.
    (await bootExpectingExit({ env: { PUSH_VAPID_PUBLIC_KEY: publicKey, PUSH_VAPID_PRIVATE_KEY: privateKey } }, 10_000)).exitCode;

  /** Boots with the pair and returns the public key the server then hands out. */
  const servedKeyWith = async (publicKey: string, privateKey: string) => {
    const booted = await startServer({ env: { PUSH_VAPID_PUBLIC_KEY: publicKey, PUSH_VAPID_PRIVATE_KEY: privateKey } });
    try {
      return (await booted.client().get("/api/push/public-key")).body.body as string;
    } finally {
      await booted.stop();
    }
  };

  test.each([
    ["a public key that is not URL-safe base64", () => [vapid.publicKey.replace(/.$/, "+"), vapid.privateKey]],
    ["a public key with base64 padding", () => [`${vapid.publicKey}=`, vapid.privateKey]],
    ["a public key in standard base64", () => [Buffer.from(vapid.publicKey, "base64url").toString("base64"), vapid.privateKey]],
    ["a public key that is too short", () => [vapid.publicKey.slice(0, -4), vapid.privateKey]],
    ["a public key that is too long", () => [`${vapid.publicKey}AAAA`, vapid.privateKey]],
    ["a public key with a space in it", () => [`${vapid.publicKey.slice(0, 10)} ${vapid.publicKey.slice(10)}`, vapid.privateKey]],
    ["a private key that is not URL-safe base64", () => [vapid.publicKey, vapid.privateKey.replace(/.$/, "/")]],
    ["a private key with base64 padding", () => [vapid.publicKey, `${vapid.privateKey}=`]],
    ["a private key that is too short", () => [vapid.publicKey, vapid.privateKey.slice(0, -2)]],
    ["a private key that is too long", () => [vapid.publicKey, `${vapid.privateKey}AA`]],
    ["the two keys swapped", () => [vapid.privateKey, vapid.publicKey]],
    ["words instead of keys", () => ["public", "private"]],
  ] as [string, () => [string, string]][])("the server exits with 1 on %s", async (_label, keys) => {
    const [publicKey, privateKey] = keys();
    expect(await exitCodeWith(publicKey, privateKey)).toBe(1);
  });

  test("the server boots with a well-formed pair", async () => {
    expect(await servedKeyWith(vapid.publicKey, vapid.privateKey)).toBe(vapid.publicKey);
  }, 90_000);

  test("the keys only have to decode to the right lengths, not to a real key pair", async () => {
    const publicKey = Buffer.alloc(65, 7).toString("base64url");
    const privateKey = Buffer.alloc(32, 9).toString("base64url");
    expect(await servedKeyWith(publicKey, privateKey)).toBe(publicKey);
  }, 90_000);

  test("a key whose last character carries bits that decode to nothing is accepted", async () => {
    // 43 characters hold 258 bits and 87 hold 522; the last two bits are in no byte.
    const privateKey = `${Buffer.alloc(32, 9).toString("base64url").slice(0, -1)}R`;
    const publicKey = `${Buffer.alloc(65, 7).toString("base64url").slice(0, -1)}R`;
    expect(Buffer.from(privateKey, "base64url")).toHaveLength(32);
    expect(Buffer.from(publicKey, "base64url")).toHaveLength(65);
    expect(await servedKeyWith(vapid.publicKey, privateKey)).toBe(vapid.publicKey);
    expect(await servedKeyWith(publicKey, vapid.privateKey)).toBe(publicKey);
  }, 90_000);
});

describe("GET /api/push/refresh/:id", () => {
  /** A subscription of `client`'s, last marked in use three days ago (the sweep takes those older than seven). */
  const agedSubscription = async (client: ApiClient, suffix: string) => {
    const id = await subscribe(client, suffix);
    await server.db.query("UPDATE push_subscriptions SET updated = NOW() - INTERVAL '3 days' WHERE push_subscription_id = $1", [id]);
    return { id, before: (await rowOf(id))! };
  };

  test("marks the caller's own subscription as in use: `updated` moves to now, and nothing else", async () => {
    const { id, before } = await agedSubscription(aliceClient, "refresh-existing");
    const { status, text } = await aliceClient.get(`/api/push/refresh/${id}`);
    expect(status).toBe(200);
    expect(text).toBe('{"status":"success"}');
    const after = (await rowOf(id))!;
    expect(isAbout(after.updated, 0)).toBe(true);
    expect(after).toEqual({ ...before, updated: after.updated });
  });

  test("can be repeated, and takes the id in any spelling PostgreSQL reads as the same UUID", async () => {
    const { id } = await agedSubscription(aliceClient, "refresh-again");
    for (const spelled of [id, id, id.toUpperCase(), `{${id}}`, id.replace(/-/g, "")]) {
      const { body } = await aliceClient.get(`/api/push/refresh/${encodeURIComponent(spelled)}`);
      expect({ spelled, body }).toEqual({ spelled, body: SUCCESS });
    }
    expect(isAbout((await rowOf(id))!.updated, 0)).toBe(true);
  });

  test("leaves someone else's subscription alone, and so does a caller without a session", async () => {
    const { id, before } = await agedSubscription(aliceClient, "refresh-foreign");
    for (const client of [bobClient, server.client()]) {
      const { status, body } = await client.get(`/api/push/refresh/${id}`);
      expect(status).toBe(200);
      expect(body).toEqual(NO_SUBSCRIPTION);
    }
    expect(await rowOf(id)).toEqual(before);
  });

  test("a viewer owns no subscription of the domain's owner", async () => {
    const admin = await adminUser(server);
    const adminClient = server.client();
    await adminClient.login("admin", ADMIN_PASSWORD);
    const { id, before } = await agedSubscription(adminClient, "refresh-admins");
    expect(before.user_id).toBe(admin.id);

    expect((await viewerClient.get(`/api/push/refresh/${id}`)).body).toEqual(NO_SUBSCRIPTION);
    expect(await rowOf(id)).toEqual(before);
    expect((await adminClient.get(`/api/push/refresh/${id}`)).body).toEqual(SUCCESS);
  });

  test("answers the same for an unknown id and for one that is not a UUID", async () => {
    for (const client of [server.client(), aliceClient]) {
      for (const id of [missingId(), "not-a-uuid", "1", "%20", "a%2Fb", "null", "'; DROP TABLE users; --"]) {
        const { status, text } = await client.get(`/api/push/refresh/${encodeURIComponent(id)}`);
        expect({ id, status, text }).toEqual({ id, status: 200, text: '{"status":"failed","message":"No subscription found"}' });
      }
    }
  });

  test("is public: no session is needed, and none is created", async () => {
    const { status, body, setCookies } = await server.client().get(`/api/push/refresh/${missingId()}`);
    expect(status).toBe(200);
    expect(body).toEqual(NO_SUBSCRIPTION);
    expect(setCookies).toEqual([]);
  });

  test("answers HEAD, a trailing slash and any letter case of the id", async () => {
    const id = missingId();
    expect((await server.client().head(`/api/push/refresh/${id}`)).status).toBe(200);
    expect((await server.client().get(`/api/push/refresh/${id}/`)).body).toEqual(NO_SUBSCRIPTION);
    expect((await server.client().get(`/api/push/refresh/${id.toUpperCase()}`)).body).toEqual(NO_SUBSCRIPTION);
  });

  test("answers 400 to an id that cannot be decoded", async () => {
    const response = await rawHttp(server.ports.http, rawRequest("GET", "/api/push/refresh/%E0%A4%A"));
    expect(response.status).toBe(400);
    expect(JSON.parse(response.body)).toEqual(BAD_REQUEST);
    const viaFetch = await server.client().get("/api/push/refresh/%ZZ");
    expect(viaFetch.status).toBe(400);
    expect(viaFetch.body).toEqual(BAD_REQUEST);
  });

  test("is not answered for other methods", async () => {
    for (const method of ["POST", "DELETE", "PUT"]) {
      const { status, body } = await aliceClient.request(method, `/api/push/refresh/${missingId()}`);
      expect({ method, status, body }).toEqual({ method, status: 404, body: NOT_FOUND });
    }
  });
});

describe("which push paths need a session", () => {
  test("everything but /public-key and /refresh… is 401 without one", async () => {
    const anonymous = server.client();
    const gated: [string, string][] = [
      ["POST", "/api/push/subscribe"],
      ["GET", "/api/push/subscribe"],
      ["DELETE", `/api/push/subscribe/${missingId()}`],
      ["DELETE", "/api/push/subscribe"],
      ["GET", "/api/push"],
      ["GET", "/api/push/"],
      ["GET", "/api/push/anything"],
      ["POST", "/api/push/public-key/extra"],
      ["PUT", "/api/push/subscribe"],
    ];
    for (const [method, path] of gated) {
      const { status, body, setCookies } = await anonymous.request(method, path);
      expect({ method, path, status, body }).toEqual({ method, path, status: 401, body: NOT_LOGGED_IN });
      expect(setCookies).toEqual([]);
    }
  });

  test("the two public paths are matched literally, so another spelling of them needs a session", async () => {
    const anonymous = server.client();
    for (const path of [
      "/api/push/public-key/",
      "/api/push/PUBLIC-KEY",
      "/api/push/Public-Key",
      `/api/push/REFRESH/${missingId()}`,
      `/api/push/Refresh/${missingId()}`,
    ]) {
      const { status, body } = await anonymous.get(path);
      expect({ path, status, body }).toEqual({ path, status: 401, body: NOT_LOGGED_IN });
    }
    // With a session the same spellings reach the routes.
    expect((await aliceClient.get("/api/push/public-key/")).body).toEqual(SUCCESS);
    expect((await aliceClient.get("/api/push/PUBLIC-KEY")).body).toEqual(SUCCESS);
    expect((await aliceClient.get(`/api/push/REFRESH/${missingId()}`)).body).toEqual(NO_SUBSCRIPTION);
    // The whole API in upper case still mounts the push router with a lower-case rest.
    expect((await anonymous.get("/API/PUSH/public-key")).body).toEqual(SUCCESS);
    expect((await anonymous.get(`/API/PUSH/refresh/${missingId()}`)).body).toEqual(NO_SUBSCRIPTION);
  });

  test("anything that merely starts with /refresh is public, and then not found", async () => {
    const anonymous = server.client();
    for (const path of ["/api/push/refresh", "/api/push/refresh/", "/api/push/refreshed", "/api/push/refresh/a/b"]) {
      const { status, body } = await anonymous.get(path);
      expect({ path, status, body }).toEqual({ path, status: 404, body: NOT_FOUND });
    }
    expect((await anonymous.post("/api/push/refresh-all", {})).status).toBe(404);
  });

  test("with a session, unknown push paths are 404", async () => {
    for (const [method, path] of [
      ["GET", "/api/push"],
      ["GET", "/api/push/anything"],
      ["GET", "/api/push/subscribe"],
      ["DELETE", "/api/push/subscribe"],
      ["DELETE", "/api/push/subscribe/a/b"],
      ["PUT", "/api/push/subscribe"],
    ] as const) {
      const { status, body } = await aliceClient.request(method, path);
      expect({ method, path, status, body }).toEqual({ method, path, status: 404, body: NOT_FOUND });
    }
  });
});

describe("POST /api/push/subscribe", () => {
  const post = (body: unknown, client: ApiClient = aliceClient) => client.post("/api/push/subscribe", body);
  const failure = async (subscriptionValue: unknown) => {
    const before = await rowCount();
    const { status, body } = await post({ subscription: subscriptionValue });
    expect(status).toBe(200);
    expect(body.status).toBe("failed");
    expect(await rowCount()).toBe(before);
    return body.message as string;
  };

  test("stores the subscription and returns its id", async () => {
    const before = await rowCount();
    const { status, body, text } = await post({ subscription: subscription("stored") });
    expect(status).toBe(200);
    expect(body.status).toBe("success");
    expect(body.body).toMatch(UUID);
    expect(text).toBe(`{"status":"success","body":"${body.body}"}`);
    expect(await rowCount()).toBe(before + 1);

    const row = (await rowOf(body.body))!;
    expect(row).toMatchObject({
      push_subscription_id: body.body,
      user_id: alice.id,
      endpoint: "https://push.example.com/send/stored",
      keys_p256dh: "p256dh-stored",
      keys_auth: "auth-stored",
    });
    expect(Object.keys(row).sort()).toEqual([
      "endpoint",
      "keys_auth",
      "keys_p256dh",
      "last_notified",
      "push_subscription_id",
      "updated",
      "user_id",
    ]);
    expect(Math.abs(row.last_notified.getTime() - Date.now())).toBeLessThan(15_000);
    expect(Math.abs(row.updated.getTime() - Date.now())).toBeLessThan(15_000);
  });

  test("keeps only the endpoint and the two keys, exactly as sent", async () => {
    const sent = {
      endpoint: "  HTTPS://Push.Example.COM:443/Send/Ünï?q=1&r=%20#frag  ",
      expirationTime: null,
      keys: { p256dh: " BPa/+dh== ", auth: "\tau th\n", extra: "dropped" },
      options: { userVisibleOnly: true },
    };
    const { body } = await post({ subscription: sent, other: "ignored" });
    expect(body.status).toBe("success");
    const row = (await rowOf(body.body))!;
    expect(row.endpoint).toBe(sent.endpoint);
    expect(row.keys_p256dh).toBe(sent.keys.p256dh);
    expect(row.keys_auth).toBe(sent.keys.auth);
  });

  test("stores the same subscription twice as two rows", async () => {
    const first = await subscribe(aliceClient, "twice");
    const second = await subscribe(aliceClient, "twice");
    expect(second).not.toBe(first);
    expect(await rowOf(first)).toBeDefined();
    expect(await rowOf(second)).toBeDefined();
  });

  test("refuses a body that is an array", async () => {
    for (const body of [[], [{ subscription: subscription() }]]) {
      const result = await post(body);
      expect(result.status).toBe(200);
      expect(result.body).toEqual(INVALID_BODY);
    }
  });

  test.each([...NON_STRINGS, ["a string", "https://push.example.com/x"], ["missing", undefined]].filter(
    ([label]) => label !== "an object"
  ) as [string, unknown][])("refuses a subscription that is %s", async (_label, value) => {
    expect(await failure(value)).toBe("subscription must be an object");
  });

  test("refuses a missing body the same way", async () => {
    const { body } = await aliceClient.request("POST", "/api/push/subscribe");
    expect(body).toEqual({ status: "failed", message: "subscription must be an object" });
  });

  test.each([...NON_STRINGS, ["an empty string", ""], ["missing", undefined]])(
    "refuses an endpoint that is %s",
    async (_label, endpoint) => {
      expect(await failure({ ...subscription(), endpoint })).toBe(
        "subscription.endpoint must be a string of 1 to 2048 characters"
      );
    }
  );

  test("accepts an endpoint of 2048 characters and refuses one of 2049", async () => {
    const base = "https://push.example.com/";
    const fits = base + "a".repeat(2048 - base.length);
    expect(fits).toHaveLength(2048);
    const { body } = await post({ subscription: { ...subscription(), endpoint: fits } });
    expect(body.status).toBe("success");
    expect((await rowOf(body.body))!.endpoint).toBe(fits);
    expect(await failure({ ...subscription(), endpoint: `${fits}a` })).toBe(
      "subscription.endpoint must be a string of 1 to 2048 characters"
    );
  });

  test("counts the endpoint's length in UTF-16 units", async () => {
    const base = "https://push.example.com/";
    // Each emoji is two units: 1011 of them make 2022, plus the 25 of the base and one more letter.
    const fits = `${base}${"😀".repeat(1011)}a`;
    expect(fits).toHaveLength(2048);
    expect((await post({ subscription: { ...subscription(), endpoint: fits } })).body.status).toBe("success");
    const over = `${base}${"😀".repeat(1012)}`;
    expect(over).toHaveLength(2049);
    expect(await failure({ ...subscription(), endpoint: over })).toBe(
      "subscription.endpoint must be a string of 1 to 2048 characters"
    );
  });

  test.each([
    ["not a url"],
    ["push.example.com/send"],
    ["//push.example.com/send"],
    ["/send"],
    ["https://"],
    ["https://push.example.com:99999/send"],
    ["https://push.example.com:port/send"],
    ["https://push example.com/send"],
    ["https://[::1/send"],
    ["https://exa<mple.com/send"],
    ["https://1.2.3.4.5/send"],
    ["https://push.example.256/send"],
    ["https://0x100000000/send"],
  ])("refuses an endpoint that is not a URL: %s", async (endpoint) => {
    expect(await failure({ ...subscription(), endpoint })).toBe("subscription.endpoint must be a valid URL");
  });

  test.each([
    ["http://push.example.com/send"],
    ["HTTP://push.example.com/send"],
    ["ftp://push.example.com/send"],
    ["ws://push.example.com/send"],
    ["wss://push.example.com/send"],
    ["file:///etc/passwd"],
    ["data:text/plain,hello"],
    ["javascript:alert(1)"],
    ["mailto:admin@example.com"],
    ["blob:https://push.example.com/uuid"],
    ["https:push.example.com"].map((url) => url.replace("https:", "httpx:")),
  ])("refuses an endpoint that is not https: %s", async (endpoint) => {
    expect(await failure({ ...subscription(), endpoint })).toBe("subscription.endpoint must use https");
  });

  test.each([
    ["https://user:password@push.example.com/send"],
    ["https://user@push.example.com/send"],
    ["https://:password@push.example.com/send"],
    ["https://user:@push.example.com/send"],
    ["https://us%40er@push.example.com/send"],
  ])("refuses an endpoint that embeds credentials: %s", async (endpoint) => {
    expect(await failure({ ...subscription(), endpoint })).toBe("subscription.endpoint must not embed credentials");
  });

  test.each([
    ["https://127.0.0.1/send"],
    ["https://127.0.0.1:6379/send"],
    ["https://10.0.0.5/send"],
    ["https://8.8.8.8/send"],
    ["https://169.254.169.254/latest/meta-data"],
    ["https://0.0.0.0/send"],
    ["https://[::1]/send"],
    ["https://[::1]:8443/send"],
    ["https://[fd00::1]/send"],
    ["https://[2001:db8::1]/send"],
    ["https://[::ffff:127.0.0.1]/send"],
    ["https://2130706433/send"],
    ["https://0x7f000001/send"],
    ["https://0x7f.1/send"],
    ["https://127.1/send"],
    ["https://017700000001/send"],
    ["https://127.0.0.1./send"],
    ["https://１２７.０.０.１/send"],
  ])("refuses an endpoint addressed by IP: %s", async (endpoint) => {
    expect(await failure({ ...subscription(), endpoint })).toBe(
      "subscription.endpoint must address a push service by hostname, not by IP"
    );
  });

  test.each([
    ["https://localhost/send"],
    ["https://LOCALHOST:8443/send"],
    ["https://intranet/send"],
    ["https://push/send"],
    ["https://localhost./send"],
    ["https://localhost../send"],
    ["https://xn--nxasmq6b/send"],
  ])("refuses an endpoint without a dot in its hostname: %s", async (endpoint) => {
    expect(await failure({ ...subscription(), endpoint })).toBe(
      "subscription.endpoint must use a fully qualified hostname"
    );
  });

  test.each([
    ["https://printer.local/send"],
    ["https://PRINTER.LOCAL/send"],
    ["https://printer.local./send"],
    ["https://app.localhost/send"],
    ["https://db.internal/send"],
    ["https://metadata.google.internal/computeMetadata"],
    ["https://nas.home.arpa/send"],
    ["https://a.b.c.internal:8443/send"],
  ])("refuses an endpoint on an internal name: %s", async (endpoint) => {
    expect(await failure({ ...subscription(), endpoint })).toBe(
      "subscription.endpoint must not address an internal hostname"
    );
  });

  test.each([
    ["https://push.example.com"],
    ["https://push.example.com./send"],
    ["HTTPS://PUSH.EXAMPLE.COM/send"],
    ["https://push.example.com:8443/send?x=1#y"],
    ["https://@push.example.com/send"],
    ["https://fcm.googleapis.com/fcm/send/abc:def"],
    ["https://updates.push.services.mozilla.com/wpush/v2/gAAAA"],
    ["https://例え.jp/send"],
    ["https://xn--r8jz45g.jp/send"],
    ["https://local.example.com/send"],
    ["https://internal.example.com/send"],
    ["https://notlocal/../x.y/send".replace("notlocal/../x.y", "a.localx")],
    ["https://localhost.example/send"],
    ["https://1.2.3.4.example/send"],
    ["https:\\\\push.example.com\\send"],
    ["https:push.example.com/send"],
    ["   https://push.example.com/send\n"],
  ])("accepts %s", async (endpoint) => {
    const { body } = await post({ subscription: { ...subscription(), endpoint } });
    expect({ endpoint, status: body.status, message: body.message }).toEqual({
      endpoint,
      status: "success",
      message: undefined,
    });
    expect((await rowOf(body.body))!.endpoint).toBe(endpoint);
  });

  test.each(
    [...NON_STRINGS, ["a string", "keys"], ["missing", undefined]].filter(
      ([label]) => label !== "an object"
    ) as [string, unknown][]
  )(
    "refuses keys that are %s",
    async (_label, keys) => {
      expect(await failure({ ...subscription(), keys })).toBe("subscription.keys must be an object");
    }
  );

  test.each([...NON_STRINGS, ["an empty string", ""], ["missing", undefined]])(
    "refuses a p256dh key that is %s",
    async (_label, p256dh) => {
      expect(await failure({ ...subscription(), keys: { p256dh, auth: "auth" } })).toBe(
        "subscription.keys.p256dh must be a string of 1 to 256 characters"
      );
    }
  );

  test.each([...NON_STRINGS, ["an empty string", ""], ["missing", undefined]])(
    "refuses an auth key that is %s",
    async (_label, auth) => {
      expect(await failure({ ...subscription(), keys: { p256dh: "p256dh", auth } })).toBe(
        "subscription.keys.auth must be a string of 1 to 256 characters"
      );
    }
  );

  test("accepts keys of 256 characters and refuses 257, counted in UTF-16 units", async () => {
    const ok = await post({ subscription: { ...subscription(), keys: { p256dh: "p".repeat(256), auth: "a".repeat(256) } } });
    expect(ok.body.status).toBe("success");
    const row = (await rowOf(ok.body.body))!;
    expect(row.keys_p256dh).toHaveLength(256);
    expect(row.keys_auth).toHaveLength(256);

    expect(await failure({ ...subscription(), keys: { p256dh: "p".repeat(257), auth: "a" } })).toBe(
      "subscription.keys.p256dh must be a string of 1 to 256 characters"
    );
    expect(await failure({ ...subscription(), keys: { p256dh: "p", auth: "a".repeat(257) } })).toBe(
      "subscription.keys.auth must be a string of 1 to 256 characters"
    );

    const emoji = await post({ subscription: { ...subscription(), keys: { p256dh: "🔑".repeat(128), auth: "🔑".repeat(128) } } });
    expect(emoji.body.status).toBe("success");
    expect(await failure({ ...subscription(), keys: { p256dh: `${"🔑".repeat(128)}x`, auth: "a" } })).toBe(
      "subscription.keys.p256dh must be a string of 1 to 256 characters"
    );
    expect(await failure({ ...subscription(), keys: { p256dh: "p", auth: `${"🔑".repeat(128)}x` } })).toBe(
      "subscription.keys.auth must be a string of 1 to 256 characters"
    );
  });

  test("checks the endpoint before the keys, and p256dh before auth", async () => {
    expect(await failure({ endpoint: "nope", keys: null })).toBe("subscription.endpoint must be a valid URL");
    expect(await failure({ endpoint: "https://push.example.com/x", keys: { p256dh: 1, auth: 2 } })).toBe(
      "subscription.keys.p256dh must be a string of 1 to 256 characters"
    );
  });

  test("subscribes a viewer for itself, never for the domain's owner", async () => {
    const id = await subscribe(viewerClient, "viewer");
    expect((await rowOf(id))!.user_id).toBe(viewer.id);
    expect((await viewerClient.delete(`/api/push/subscribe/${id}`)).body).toEqual(SUCCESS);
  });

  test("answers with a trailing slash and in any letter case", async () => {
    for (const path of ["/api/push/subscribe/", "/api/push/SUBSCRIBE", "/API/Push/Subscribe/"]) {
      const { body } = await aliceClient.post(path, { subscription: subscription("case") });
      expect({ path, status: body.status }).toEqual({ path, status: "success" });
    }
  });
});

describe("DELETE /api/push/subscribe/:id", () => {
  test("removes the caller's own subscription", async () => {
    const id = await subscribe(aliceClient, "own");
    const other = await subscribe(aliceClient, "own-other");
    const { status, text } = await aliceClient.delete(`/api/push/subscribe/${id}`);
    expect(status).toBe(200);
    expect(text).toBe('{"status":"success"}');
    expect(await rowOf(id)).toBeUndefined();
    expect(await rowOf(other)).toBeDefined();
    // It is gone, so a second delete finds nothing.
    expect((await aliceClient.delete(`/api/push/subscribe/${id}`)).body).toEqual(NO_SUBSCRIPTION);
  });

  test("does not remove someone else's", async () => {
    const id = await subscribe(aliceClient, "alices");
    const { status, body } = await bobClient.delete(`/api/push/subscribe/${id}`);
    expect(status).toBe(200);
    expect(body).toEqual(NO_SUBSCRIPTION);
    expect(await rowOf(id)).toBeDefined();
  });

  test("finds nothing for an unknown id", async () => {
    const { status, body } = await aliceClient.delete(`/api/push/subscribe/${missingId()}`);
    expect(status).toBe(200);
    expect(body).toEqual(NO_SUBSCRIPTION);
  });

  test("finds nothing for an id that is not a UUID", async () => {
    const before = await rowCount();
    for (const id of ["not-a-uuid", "1", "%20", "a%2Fb", "null", "'; DELETE FROM push_subscriptions; --", "x".repeat(300)]) {
      const { status, body } = await aliceClient.delete(`/api/push/subscribe/${encodeURIComponent(id)}`);
      expect({ id, status, body }).toEqual({ id, status: 200, body: NO_SUBSCRIPTION });
    }
    expect(await rowCount()).toBe(before);
  });

  test("accepts the id in upper case, with braces or without hyphens, as PostgreSQL does", async () => {
    for (const spell of [
      (id: string) => id.toUpperCase(),
      (id: string) => `{${id}}`,
      (id: string) => id.replace(/-/g, ""),
    ]) {
      const id = await subscribe(aliceClient, "spelled");
      const { body } = await aliceClient.delete(`/api/push/subscribe/${encodeURIComponent(spell(id))}`);
      expect({ sent: spell(id), body }).toEqual({ sent: spell(id), body: SUCCESS });
      expect(await rowOf(id)).toBeUndefined();
    }
  });

  test("answers 400 to an id that cannot be decoded", async () => {
    const { status, body } = await aliceClient.delete("/api/push/subscribe/%ZZ");
    expect(status).toBe(400);
    expect(body).toEqual(BAD_REQUEST);
    // Without a session the gate answers first.
    expect((await server.client().delete("/api/push/subscribe/%ZZ")).status).toBe(401);
  });

  test("a viewer cannot remove the domain owner's subscription", async () => {
    const admin = await adminUser(server);
    const adminClient = server.client();
    await adminClient.login("admin", ADMIN_PASSWORD);
    const id = await subscribe(adminClient, "admins");
    expect((await rowOf(id))!.user_id).toBe(admin.id);

    const { status, body } = await viewerClient.delete(`/api/push/subscribe/${id}`);
    expect(status).toBe(200);
    expect(body).toEqual(NO_SUBSCRIPTION);
    expect(await rowOf(id)).toBeDefined();
    expect((await adminClient.delete(`/api/push/subscribe/${id}`)).body).toEqual(SUCCESS);
  });

  test("answers with a trailing slash and in any letter case", async () => {
    const one = await subscribe(aliceClient, "slash");
    expect((await aliceClient.delete(`/api/push/subscribe/${one}/`)).body).toEqual(SUCCESS);
    const two = await subscribe(aliceClient, "case");
    expect((await aliceClient.delete(`/api/push/SUBSCRIBE/${two}`)).body).toEqual(SUCCESS);
  });

  test("a user's subscriptions go with the user row", async () => {
    const carol = await createUser(server, "carol");
    const client = server.client();
    await client.login(carol.username, carol.password);
    const id = await subscribe(client, "carols");
    await server.db.query("DELETE FROM users WHERE user_id = $1", [carol.id]);
    expect(await rowOf(id)).toBeUndefined();
  });
});

describe("sending a push", () => {
  test("a redirect from the push service is not followed", async () => {
    const inside: string[] = [];
    const internal = Bun.serve({
      port: 0,
      hostname: "127.0.0.1",
      fetch: (request) => {
        inside.push(`${request.method} ${new URL(request.url).pathname}`);
        return new Response("ok");
      },
    });
    const pushed: string[] = [];
    const redirect = async (request: Request) => {
      pushed.push(new URL(request.url).protocol);
      await request.arrayBuffer();
      return new Response(null, { status: 307, headers: { Location: `http://127.0.0.1:${internal.port}/inside` } });
    };
    const dir = await fs.promises.mkdtemp(path.join(os.tmpdir(), "mail-e2e-push-"));
    const certificate = await generateCertificate(dir);
    const tls = { cert: Bun.file(certificate.certPath), key: Bun.file(certificate.keyPath) };
    const secureService = Bun.serve({ port: 0, hostname: "127.0.0.1", tls, fetch: redirect });
    const plainService = Bun.serve({ port: 0, hostname: "127.0.0.1", fetch: redirect });
    const pushing = await startServer({
      env: {
        PUSH_VAPID_PUBLIC_KEY: vapid.publicKey,
        PUSH_VAPID_PRIVATE_KEY: vapid.privateKey,
        NODE_TLS_REJECT_UNAUTHORIZED: "0",
      },
    });
    try {
      const dave = await createUser(pushing, "dave");
      // Written to the table directly: the subscribe route takes only a
      // public https name. One endpoint each server's push client can reach.
      for (const endpoint of [`https://127.0.0.1:${secureService.port}/push`, `http://127.0.0.1:${plainService.port}/push`]) {
        const ecdh = crypto.createECDH("prime256v1");
        ecdh.generateKeys();
        await pushing.db.query(
          "INSERT INTO push_subscriptions (user_id, endpoint, keys_p256dh, keys_auth) VALUES ($1, $2, $3, $4)",
          [dave.id, endpoint, ecdh.getPublicKey().toString("base64url"), crypto.randomBytes(16).toString("base64url")]
        );
      }
      const recipient = `hello@${dave.username}.${pushing.domain}`;
      const delivered = await deliverMail(pushing, {
        from: "someone@remote.example",
        to: [recipient],
        data: `From: someone@remote.example\r\nTo: ${recipient}\r\nSubject: hi\r\n\r\nhello\r\n`,
      });
      expect(delivered.accepted).toBe(true);
      for (let i = 0; i < 200 && pushed.length === 0; i++) await Bun.sleep(50);
      expect(pushed.length).toBeGreaterThan(0);
      await Bun.sleep(1000);
      expect(inside).toEqual([]);
    } finally {
      await pushing.stop();
      secureService.stop(true);
      plainService.stop(true);
      internal.stop(true);
      await fs.promises.rm(dir, { recursive: true, force: true });
    }
  }, 90_000);
});
