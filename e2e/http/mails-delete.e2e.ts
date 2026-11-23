import fs from "fs";
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { addr, missingId, rawHttp, readMail, startServer, type TestServer } from "../harness";
import {
  JSON_TYPE,
  adminMailbox,
  attachmentPath,
  countMails,
  failed,
  newMailbox,
  subjects,
  writeAttachment,
  type Mailbox,
} from "./mails-helpers";

let server: TestServer;
let box: Mailbox;

beforeAll(async () => {
  server = await startServer();
  box = await newMailbox(server);
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

const SUCCESS = { status: "success" };
const NOT_YOURS = failed("Invalid request. You may not manipulate other users' email");

describe("DELETE /api/mails/:id", () => {
  test("removes the mail's row outright, with an exact answer", async () => {
    const mail = await box.mail();
    const { status, text, headers } = await box.client.delete(`/api/mails/${mail.mail_id}`);
    expect(status).toBe(200);
    expect(text).toBe('{"status":"success"}');
    expect(headers.get("content-type")).toBe(JSON_TYPE);
    expect(await readMail(server, mail.mail_id)).toBeUndefined();
  });

  test("the mail is gone from every read route", async () => {
    const fresh = await newMailbox(server);
    const gone = await fresh.mail({ subject: "Deleteme pineapple", message_id: "<gone@x>", saved: true });
    const kept = await fresh.mail({ subject: "Re: Deleteme pineapple", in_reply_to: "<gone@x>" });
    expect((await fresh.client.delete(`/api/mails/${gone.mail_id}`)).body).toEqual(SUCCESS);
    const me = fresh.address();
    expect(subjects((await fresh.client.get(`/api/mails/headers/${me}`)).body.body)).toEqual([kept.subject!]);
    expect((await fresh.client.get(`/api/mails/headers/${me}?saved=1`)).body.body).toEqual([]);
    expect(subjects((await fresh.client.get("/api/mails/unified")).body.body)).toEqual([kept.subject!]);
    expect(subjects((await fresh.client.get("/api/mails/search/pineapple")).body.body)).toEqual([kept.subject!]);
    expect(subjects((await fresh.client.get(`/api/mails/thread/${kept.mail_id}`)).body.body)).toEqual([kept.subject!]);
    expect((await fresh.client.get(`/api/mails/body/${gone.mail_id}`)).body).toEqual(failed("No email is found."));
    expect((await fresh.client.get(`/api/mails/thread/${gone.mail_id}`)).body).toEqual(failed("No email is found."));
    expect((await fresh.client.get("/api/mails/accounts")).body.body.received).toEqual([
      { key: me, doc_count: 1, unread_doc_count: 1, saved_doc_count: 0, updated: new Date(kept.date as Date).toISOString() },
    ]);
  });

  test("its mailbox entries go with it", async () => {
    const mail = await box.mail();
    await box.client.post("/api/mails/mark", { mail_id: mail.mail_id, save: true });
    const entries = () => server.db.query("SELECT mailbox FROM mail_mailbox_uid WHERE mail_id = $1", [mail.mail_id]);
    expect(await entries()).toEqual([{ mailbox: "Starred" }]);
    await box.client.delete(`/api/mails/${mail.mail_id}`);
    expect(await entries()).toEqual([]);
  });

  // NOTE: the row goes but the attachment files it named stay on disk, with
  // nothing left that refers to them.
  test("leaves the mail's attachment files on disk, no longer downloadable", async () => {
    const id = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
    writeAttachment(server, id, "kept on disk");
    const mail = await box.mail({ attachments: [{ content: { data: id }, filename: "a.txt", contentType: "text/plain", size: 12 }] });
    expect((await box.client.get(`/api/mails/attachment/${id}`)).text).toBe("kept on disk");
    await box.client.delete(`/api/mails/${mail.mail_id}`);
    expect(fs.existsSync(attachmentPath(server, id))).toBe(true);
    expect((await box.client.get(`/api/mails/attachment/${id}`)).body).toEqual(failed("Not found"));
  });

  test("deleting twice: the second attempt is refused like a mail that never existed", async () => {
    const mail = await box.mail();
    expect((await box.client.delete(`/api/mails/${mail.mail_id}`)).body).toEqual(SUCCESS);
    const again = await box.client.delete(`/api/mails/${mail.mail_id}`);
    expect(again.status).toBe(200);
    expect(again.text).toBe('{"status":"failed","message":"Invalid request. You may not manipulate other users\' email"}');
  });

  test("deletes any of the user's live rows: sent, spam, draft, saved, deleted", async () => {
    for (const row of [{ sent: true }, { is_spam: true }, { draft: true }, { saved: true, read: true }, { deleted: true }]) {
      const mail = await box.mail(row);
      expect({ row, body: (await box.client.delete(`/api/mails/${mail.mail_id}`)).body }).toEqual({ row, body: SUCCESS });
      expect(await readMail(server, mail.mail_id)).toBeUndefined();
    }
  });

  test("an expunged mail is refused like a missing one, and its row stays for the delta to report", async () => {
    const mail = await box.mail({ expunged: true });
    const { status, text } = await box.client.delete(`/api/mails/${mail.mail_id}`);
    expect(status).toBe(200);
    expect(text).toBe('{"status":"failed","message":"Invalid request. You may not manipulate other users\' email"}');
    expect(await readMail(server, mail.mail_id)).toBeDefined();
  });

  test("deletes only the one mail", async () => {
    const fresh = await newMailbox(server);
    const keep = await fresh.mail({ message_id: "<same-thread@x>" });
    const drop = await fresh.mail({ in_reply_to: "<same-thread@x>" });
    await fresh.client.delete(`/api/mails/${drop.mail_id}`);
    expect(await countMails(server, fresh.id)).toBe(1);
    expect(await readMail(server, keep.mail_id)).toBeDefined();
  });

  test("accepts the id in upper case, with a trailing slash and with a body it ignores", async () => {
    const upper = await box.mail();
    expect((await box.client.delete(`/api/mails/${upper.mail_id.toUpperCase()}`)).body).toEqual(SUCCESS);
    const slash = await box.mail();
    expect((await box.client.delete(`/api/mails/${slash.mail_id}/`)).body).toEqual(SUCCESS);
    const withBody = await box.mail();
    const other = await box.mail();
    expect((await box.client.request("DELETE", `/api/mails/${withBody.mail_id}`, { json: { mail_id: other.mail_id } })).body).toEqual(SUCCESS);
    expect(await readMail(server, withBody.mail_id)).toBeUndefined();
    expect(await readMail(server, other.mail_id)).toBeDefined();
  });

  test("ignores the query string", async () => {
    const mail = await box.mail();
    const other = await box.mail();
    expect((await box.client.delete(`/api/mails/${mail.mail_id}?id=${other.mail_id}&all=1`)).body).toEqual(SUCCESS);
    expect(await readMail(server, other.mail_id)).toBeDefined();
  });

  test("refuses an id that is not a UUID, a missing id and another user's mail with one message", async () => {
    const other = await newMailbox(server);
    const admin = await adminMailbox(server);
    const theirs = await other.mail();
    for (const id of ["abc", "123", "all", "*", "%20", "a%2Fb", missingId(), theirs.mail_id, `${theirs.mail_id}0`, `{${theirs.mail_id}}`, theirs.mail_id.replace(/-/g, ""), "x".repeat(5000)]) {
      const { status, body } = await box.client.delete(`/api/mails/${id}`);
      expect({ id: id.slice(0, 40), status, body }).toEqual({ id: id.slice(0, 40), status: 200, body: NOT_YOURS });
    }
    expect((await admin.client.delete(`/api/mails/${theirs.mail_id}`)).body).toEqual(NOT_YOURS);
    expect(await readMail(server, theirs.mail_id)).toBeDefined();
  });

  test("a mail stored for one user but naming another's address belongs to the one it is stored for", async () => {
    const other = await newMailbox(server);
    const mail = await other.mail({ to: [addr(box.address())] });
    expect((await box.client.delete(`/api/mails/${mail.mail_id}`)).body).toEqual(NOT_YOURS);
    expect((await other.client.delete(`/api/mails/${mail.mail_id}`)).body).toEqual(SUCCESS);
  });

  test("an undecodable id is a 400", async () => {
    const response = await rawHttp(
      server.ports.http,
      `DELETE /api/mails/%E0%A4%A HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nCookie: ${box.client.cookieHeader()}\r\n\r\n`
    );
    expect(response.status).toBe(400);
    expect(response.body).toBe('{"status":"error","message":"Bad Request"}');
  });

  test("POST and GET on a mail id are not answered", async () => {
    const mail = await box.mail();
    expect((await box.client.post(`/api/mails/${mail.mail_id}`, {})).status).toBe(404);
    expect((await box.client.get(`/api/mails/${mail.mail_id}`)).status).toBe(404);
    expect(await readMail(server, mail.mail_id)).toBeDefined();
  });
});
