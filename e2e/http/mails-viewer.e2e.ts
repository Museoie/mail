/**
 * A domain shared read-only: the viewer role on a domain someone else owns.
 *
 * The owner shares one of their domains with another account. That account
 * reads the domain's mail, from the owner's rows and filtered to the domain,
 * and nothing it does changes those rows. It never sends as the domain, and
 * it loses the domain at its next request once the owner stops sharing.
 */
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { addr, giveDomains, readMail, startServer, type InsertedMail, type TestServer } from "../harness";
import { failed, headerOf, newMailbox, writeAttachment, type Mailbox } from "./mails-helpers";

const SHARED = "shared.example";
const ATTACHMENT_ID = "cdcdcdcd-cdcd-4dcd-8dcd-cdcdcdcdcdcd";
const NOT_FOUND = failed("No email is found.");

let server: TestServer;
let owner: Mailbox;
let viewer: Mailbox;
let shared: InsertedMail;
let forged: InsertedMail;

beforeAll(async () => {
  server = await startServer();
  owner = await newMailbox(server, "owner");
  viewer = await newMailbox(server, "viewer");
  // The operator vouches for the domain, as `bun run user add-domain` does.
  await giveDomains(server, owner.id, [SHARED]);
  writeAttachment(server, ATTACHMENT_ID, "the shared file");
  shared = await owner.mail({
    to: [addr(`info@${SHARED}`)],
    // A Bcc to the owner's own domain rides in the same envelope.
    envelope_to: [{ address: `info@${SHARED}` }, { address: owner.address("secret") }],
    subject: "Quarterly kiwi report",
    text: "For everyone who reads the shared domain.",
    html: "<p>For everyone who reads the shared domain.</p>",
    message_id: "<shared-root@remote.example>",
    attachments: [{ content: { data: ATTACHMENT_ID }, filename: "r.txt", contentType: "text/plain", size: 15 }],
    modseq: 7,
    updated: "2026-01-01T00:00:00Z",
    date: "2026-01-02T00:00:00Z",
  });
  // Delivered to the owner's own domain; its To header names the shared one.
  forged = await owner.mail({
    to: [addr(`info@${SHARED}`)],
    envelope_to: [{ address: owner.address() }],
    subject: "A private kiwi note",
    date: "2026-01-03T00:00:00Z",
  });
  const share = await owner.client.post("/api/domains/viewers", { name: SHARED, username: viewer.username });
  expect(share.body).toMatchObject({ status: "success", body: { name: SHARED, role: "owner", viewers: [viewer.username] } });
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

const untouched = async () => {
  const row = await readMail(server, shared.mail_id);
  expect({
    read: row!.read,
    saved: row!.saved,
    is_spam: row!.is_spam,
    expunged: row!.expunged,
    modseq: String(row!.modseq),
  }).toEqual({ read: false, saved: false, is_spam: false, expunged: false, modseq: "7" });
};

describe("sharing a domain", () => {
  test("the viewer is told which domain it views, and from whom", async () => {
    const { body } = await viewer.client.get("/api/domains");
    expect(body.body.domains).toContainEqual({
      name: SHARED,
      role: "viewer",
      owner: owner.username,
      managedByServer: false,
      verifiedAt: null,
    });
  });

  test("the domain's accounts are listed apart from the viewer's own", async () => {
    const { body } = await viewer.client.get("/api/mails/accounts");
    expect(body.body.received.map((a: { key: string }) => a.key)).not.toContain(`info@${SHARED}`);
    expect(body.body.shared).toHaveLength(1);
    expect(body.body.shared[0]).toMatchObject({ domain: SHARED, owner: owner.username });
    expect(body.body.shared[0].received.map((a: { key: string }) => a.key)).toEqual([`info@${SHARED}`]);
  });

  test("only the owner shares, and only an existing account", async () => {
    expect((await viewer.client.post("/api/domains/viewers", { name: SHARED, username: owner.username })).body).toEqual(
      failed("You can only share a domain you own.")
    );
    expect((await owner.client.post("/api/domains/viewers", { name: SHARED, username: "nobody-here" })).body).toEqual(
      failed("There is no account named nobody-here.")
    );
  });
});

describe("what a viewer reads", () => {
  test("the domain's mail, by its envelope: a header naming the domain is not enough", async () => {
    const { status, body } = await viewer.client.get(`/api/mails/headers/info@${SHARED}`);
    expect(status).toBe(200);
    const headers = body.body as { id: string }[];
    expect(headers.map((h) => h.id)).toEqual([shared.mail_id]);
    expect(headers.map((h) => h.id)).not.toContain(forged.mail_id);
  });

  test("only the envelope recipients on the shared domain: a Bcc to the owner's own stays hidden", async () => {
    const [header] = (await viewer.client.get(`/api/mails/headers/info@${SHARED}`)).body.body;
    expect(header).toEqual(
      headerOf(shared, {
        envelopeTo: { value: [{ address: `info@${SHARED}` }], text: "" },
        snippet: "For everyone who reads the shared domain.",
      })
    );
    // The owner sees both, since both are on their domains.
    const own = (await owner.client.get(`/api/mails/headers/info@${SHARED}`)).body.body.find(
      (h: { id: string }) => h.id === shared.mail_id
    );
    expect(own.envelopeTo.value).toEqual([{ address: `info@${SHARED}` }, { address: owner.address("secret") }]);
  });

  test("the body and its attachment, and reading leaves the mail unread", async () => {
    const { body } = await viewer.client.get(`/api/mails/body/${shared.mail_id}`);
    expect(body).toMatchObject({ status: "success", body: { id: shared.mail_id, html: "<p>For everyone who reads the shared domain.</p>" } });
    const attachment = await viewer.client.get(`/api/mails/attachment/${ATTACHMENT_ID}`);
    expect(attachment.status).toBe(200);
    expect(attachment.text).toBe("the shared file");
    await untouched();
  });

  test("the thread: the mail on its own", async () => {
    const { body } = await viewer.client.get(`/api/mails/thread/${shared.mail_id}`);
    expect(body.body.map((h: { id: string }) => h.id)).toEqual([shared.mail_id]);
  });

  test("nothing of the owner's other domains", async () => {
    expect((await viewer.client.get(`/api/mails/body/${forged.mail_id}`)).body).toEqual(NOT_FOUND);
    expect((await viewer.client.get(`/api/mails/thread/${forged.mail_id}`)).body).toEqual(NOT_FOUND);
    expect((await viewer.client.get(`/api/mails/headers/${owner.address()}`)).body).toEqual({ status: "success", body: [] });
  });

  test("an account with no role reads nothing of the domain", async () => {
    const stranger = await newMailbox(server, "stranger");
    expect((await stranger.client.get(`/api/mails/headers/info@${SHARED}`)).body).toEqual({ status: "success", body: [] });
    expect((await stranger.client.get(`/api/mails/body/${shared.mail_id}`)).body).toEqual(NOT_FOUND);
    expect((await stranger.client.get(`/api/mails/attachment/${ATTACHMENT_ID}`)).body.status).toBe("failed");
  });
});

describe("what a viewer cannot do", () => {
  test("mark, delete or mark as spam the domain's mail", async () => {
    for (const [method, path, payload] of [
      ["POST", "/api/mails/mark", { mail_id: shared.mail_id, read: true, save: true }],
      ["POST", "/api/mails/spam/mark", { mail_id: shared.mail_id, is_spam: true }],
      ["DELETE", `/api/mails/${shared.mail_id}`, undefined],
    ] as const) {
      const { status, body } = await viewer.client.request(method, path, payload === undefined ? {} : { body: JSON.stringify(payload), headers: { "Content-Type": "application/json" } });
      expect({ path, status, outcome: body.status }).toEqual({ path, status: 200, outcome: "failed" });
    }
    await untouched();
    expect(await server.db.query("SELECT 1 FROM spam_training WHERE user_id = $1", [owner.id])).toEqual([]);
  });

  test("send as the domain", async () => {
    server.mailgun.reset();
    const { body } = await viewer.client.post("/api/mails/send", {
      sender: "info",
      senderDomain: SHARED,
      to: "someone@remote.example",
      subject: "s",
      html: "<p>h</p>",
    });
    expect(body).toEqual(failed(`You can't send from ${SHARED}.`));
    expect(server.mailgun.requests).toEqual([]);
  });
});

describe("when the owner stops sharing", () => {
  test("the viewer reads nothing of the domain from its next request on", async () => {
    // A read before, so the session is known to have had the role.
    expect((await viewer.client.get(`/api/mails/headers/info@${SHARED}`)).body.body).toHaveLength(1);
    const { body } = await owner.client.delete(`/api/domains/viewers/${SHARED}/${viewer.username}`);
    expect(body).toMatchObject({ status: "success", body: { name: SHARED, viewers: [] } });

    expect((await viewer.client.get(`/api/mails/headers/info@${SHARED}`)).body).toEqual({ status: "success", body: [] });
    expect((await viewer.client.get(`/api/mails/body/${shared.mail_id}`)).body).toEqual(NOT_FOUND);
    expect((await viewer.client.get(`/api/mails/attachment/${ATTACHMENT_ID}`)).body.status).toBe("failed");
    expect((await viewer.client.get("/api/mails/accounts")).body.body.shared).toEqual([]);
    expect((await viewer.client.get("/api/domains")).body.body.domains.map((d: { name: string }) => d.name)).not.toContain(SHARED);
  });
});
