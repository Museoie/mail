import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createUser, type ApiClient, type TestServer, type TestUser } from "../harness";
import { ImapClient, lit, openSession } from "../harness/imap";
import { deliver, mailDomain, raws, startImapServer, type DeliveredMail } from "./support";

let server: TestServer;
let userCount = 0;

interface Box {
  user: TestUser;
  imap: ImapClient;
  http: ApiClient;
  mails: DeliveredMail[];
  domain: string;
}

/** A fresh account with `count` mails for `hello@`, an IMAP session on INBOX and a signed-in HTTP client. */
const account = async (count: number): Promise<Box> => {
  const user = await createUser(server, `web${++userCount}`);
  const domain = mailDomain(server, user.username);
  const mails: DeliveredMail[] = [];
  for (let i = 1; i <= count; i++) mails.push(await deliver(server, { to: `hello@${domain}`, subject: `Mail ${i}` }));
  const imap = await openSession(server, user.username, user.password, "INBOX");
  const http = server.client();
  await http.login(user.username, user.password);
  return { user, imap, http, mails, domain };
};

const mark = async (http: ApiClient, body: Record<string, unknown>) => {
  const result = await http.post("/api/mails/mark", body);
  expect(result.body).toEqual({ status: "success" });
};

/** Everything up to and including `* <n> EXISTS` and the `* 0 RECENT` after it. */
const untilExists = async (imap: ImapClient): Promise<string[]> => {
  const seen = await imap.waitFor(/^\* \d+ EXISTS$/);
  const recent = await imap.next();
  return raws([...seen, recent]);
};

beforeAll(async () => {
  server = await startImapServer();
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

describe("flags changed over HTTP, seen over IMAP", () => {
  test("marking a mail read pushes its flags to an idling session", async () => {
    const { imap, http, mails } = await account(2);
    const { tag } = await imap.idle();
    await mark(http, { mail_id: mails[1]!.mail_id, read: true });
    expect(raws(await imap.waitFor(/FETCH/))).toEqual(["* 2 FETCH (FLAGS (\\Seen))"]);
    expect((await imap.done(tag)).lines).toEqual([`${tag} OK IDLE terminated`]);
    imap.close();
  });

  test("saving and unsaving a mail pushes \\Flagged on and off, and files it in and out of Starred", async () => {
    const { imap, http, mails } = await account(1);
    const { tag } = await imap.idle();
    await mark(http, { mail_id: mails[0]!.mail_id, save: true });
    expect(raws(await imap.waitFor(/FETCH/))).toEqual(["* 1 FETCH (FLAGS (\\Flagged))"]);
    await mark(http, { mail_id: mails[0]!.mail_id, save: false });
    expect(raws(await imap.waitFor(/FETCH/))).toEqual(["* 1 FETCH (FLAGS ())"]);
    await mark(http, { mail_id: mails[0]!.mail_id, save: true });
    expect(raws(await imap.waitFor(/FETCH/))).toEqual(["* 1 FETCH (FLAGS (\\Flagged))"]);
    await imap.done(tag);
    // Filed twice, so the second filing has the second Starred UID.
    await imap.command("SELECT Starred");
    expect((await imap.command("FETCH 1:* (UID FLAGS)")).untagged).toEqual(["* 1 FETCH (UID 2 FLAGS (\\Flagged))"]);
    imap.close();
  });

  test("read and save in one request push the flags twice, the second with both", async () => {
    const { imap, http, mails } = await account(1);
    const { tag } = await imap.idle();
    await mark(http, { mail_id: mails[0]!.mail_id, read: true, save: true });
    const pushed = [...(await imap.waitFor(/Flagged/))];
    expect(raws(pushed).at(-1)).toBe("* 1 FETCH (FLAGS (\\Seen \\Flagged))");
    await imap.done(tag);
    imap.close();
  });

  test("'read: false' is accepted and does nothing: a mail cannot be marked unread over HTTP", async () => {
    const { imap, http, mails } = await account(1);
    await mark(http, { mail_id: mails[0]!.mail_id, read: true });
    await mark(http, { mail_id: mails[0]!.mail_id, read: false });
    expect((await imap.command("FETCH 1 (FLAGS)")).untagged).toEqual(["* 1 FETCH (FLAGS (\\Seen))"]);
    imap.close();
  });

  test("reading a mail's body over HTTP marks it read and pushes the flags, each time it is read", async () => {
    const { imap, http, mails } = await account(1);
    const { tag } = await imap.idle();
    const first = await http.get(`/api/mails/body/${mails[0]!.mail_id}`);
    expect(first.body.status).toBe("success");
    expect(raws(await imap.waitFor(/FETCH/))).toEqual(["* 1 FETCH (FLAGS (\\Seen))"]);
    await http.get(`/api/mails/body/${mails[0]!.mail_id}`);
    expect(raws(await imap.waitFor(/FETCH/))).toEqual(["* 1 FETCH (FLAGS (\\Seen))"]);
    await imap.done(tag);
    const [row] = await server.db.query("SELECT read FROM mails WHERE mail_id = $1", [mails[0]!.mail_id]);
    expect(row).toEqual({ read: true });
    imap.close();
  });

  test("a session that is not idling is not sent anything; it sees the flags when it fetches them", async () => {
    const { imap, http, mails } = await account(1);
    await mark(http, { mail_id: mails[0]!.mail_id, read: true, save: true });
    expect((await imap.command("NOOP")).lines).toEqual(["A3 OK NOOP completed"]);
    expect((await imap.command("FETCH 1 (FLAGS)")).lines).toEqual([
      "* 1 FETCH (FLAGS (\\Seen \\Flagged))",
      "A4 OK FETCH completed",
    ]);
    imap.close();
  });

  test("the push uses the idling session's own sequence numbers and skips mail outside its mailbox", async () => {
    const { imap, http, mails, domain, user } = await account(2);
    const work = await deliver(server, { to: `work@${domain}`, subject: "Work" });
    const other = await openSession(server, user.username, user.password, "INBOX/accounts/work");
    await imap.command("SELECT INBOX");
    const inboxIdle = await imap.idle();
    const workIdle = await other.idle();

    // A mail only INBOX shows, then the one both show.
    await mark(http, { mail_id: mails[1]!.mail_id, read: true });
    await mark(http, { mail_id: work.mail_id, read: true });
    expect(raws(await imap.waitFor(/^\* 3 FETCH/))).toEqual(["* 2 FETCH (FLAGS (\\Seen))", "* 3 FETCH (FLAGS (\\Seen))"]);
    // In the per-address mailbox the work mail is message 1, and the other mail was never mentioned.
    expect(raws(await other.waitFor(/FETCH/))).toEqual(["* 1 FETCH (FLAGS (\\Seen))"]);
    await imap.done(inboxIdle.tag);
    await other.done(workIdle.tag);
    imap.close();
    other.close();
  });

  test("with CONDSTORE enabled the pushed flags still carry no MODSEQ, though the mail's did change", async () => {
    // NOTE: RFC 7162 3.1.3 wants MODSEQ on unsolicited FETCH responses once
    // CONDSTORE is enabled.
    const { imap, http, mails } = await account(1);
    await imap.command("ENABLE CONDSTORE");
    const { tag } = await imap.idle();
    await mark(http, { mail_id: mails[0]!.mail_id, read: true });
    expect(raws(await imap.waitFor(/FETCH/))).toEqual(["* 1 FETCH (FLAGS (\\Seen))"]);
    await imap.done(tag);
    expect((await imap.command("FETCH 1 (FLAGS)")).untagged).toEqual(["* 1 FETCH (FLAGS (\\Seen) MODSEQ (2))"]);
    imap.close();
  });
});

describe("mail removed over HTTP, seen over IMAP", () => {
  test("a deleted mail is gone from the table; an idling session hears of it only with the next delivery", async () => {
    const { imap, http, mails, domain, user } = await account(3);
    const { tag } = await imap.idle();
    const deleted = await http.delete(`/api/mails/${mails[1]!.mail_id}`);
    expect(deleted.body).toEqual({ status: "success" });
    expect(
      await server.db.query("SELECT uid_domain FROM mails WHERE user_id = $1 ORDER BY uid_domain", [user.id])
    ).toEqual([{ uid_domain: 1 }, { uid_domain: 3 }]);

    await deliver(server, { to: `hello@${domain}`, subject: "New" });
    expect(await untilExists(imap)).toEqual(["* 2 EXPUNGE", "* 3 EXISTS", "* 0 RECENT"]);
    await imap.done(tag);
    expect((await imap.command("FETCH 1:* (UID)")).untagged).toEqual([
      "* 1 FETCH (UID 1)",
      "* 2 FETCH (UID 3)",
      "* 3 FETCH (UID 4)",
    ]);
    imap.close();
  });

  test("a session that is not idling keeps the deleted mail's number until it selects again", async () => {
    const { imap, http, mails } = await account(2);
    await http.delete(`/api/mails/${mails[0]!.mail_id}`);
    expect((await imap.command("NOOP")).lines).toEqual(["A3 OK NOOP completed"]);
    expect((await imap.command("FETCH 1:* (UID)")).untagged).toEqual(["* 2 FETCH (UID 2)"]);
    expect((await imap.command("STATUS INBOX (MESSAGES UIDNEXT)")).untagged).toEqual([
      '* STATUS "INBOX" (MESSAGES 1 UIDNEXT 3)',
    ]);
    expect((await imap.command("SELECT INBOX")).untagged[0]).toBe("* 1 EXISTS");
    expect((await imap.command("FETCH 1:* (UID)")).untagged).toEqual(["* 1 FETCH (UID 2)"]);
    imap.close();
  });

  test("a mail marked as spam leaves INBOX for Junk under the same UID, and comes back when unmarked", async () => {
    const { imap, http, mails, domain } = await account(2);
    const spam = await http.post("/api/mails/spam/mark", { mail_id: mails[0]!.mail_id, is_spam: true });
    expect(spam.body).toEqual({ status: "success" });
    expect((await imap.command("STATUS INBOX (MESSAGES)")).untagged).toEqual(['* STATUS "INBOX" (MESSAGES 1)']);
    expect((await imap.command("STATUS Junk (MESSAGES)")).untagged).toEqual(['* STATUS "Junk" (MESSAGES 1)']);
    expect((await imap.command('STATUS "INBOX/accounts/hello" (MESSAGES)')).untagged).toEqual([
      '* STATUS "INBOX/accounts/hello" (MESSAGES 1)',
    ]);

    // An idling session is told with the next delivery.
    const { tag } = await imap.idle();
    await deliver(server, { to: `hello@${domain}`, subject: "New" });
    expect(await untilExists(imap)).toEqual(["* 1 EXPUNGE", "* 2 EXISTS", "* 0 RECENT"]);
    await imap.done(tag);

    await imap.command("SELECT Junk");
    expect((await imap.command("FETCH 1:* (UID FLAGS)")).untagged).toEqual(["* 1 FETCH (UID 1 FLAGS ())"]);

    await http.post("/api/mails/spam/mark", { mail_id: mails[0]!.mail_id, is_spam: false });
    expect((await imap.command("SELECT INBOX")).untagged[0]).toBe("* 3 EXISTS");
    expect((await imap.command("FETCH 1 (UID)")).untagged).toEqual(["* 1 FETCH (UID 1)"]);
    imap.close();
  });
});

describe("changes made over IMAP, seen over HTTP", () => {
  const counts = async (http: ApiClient) => {
    const { body } = await http.get("/api/mails/accounts");
    const [received] = body.body.received;
    return received && {
      doc_count: received.doc_count,
      unread_doc_count: received.unread_doc_count,
      saved_doc_count: received.saved_doc_count,
    };
  };

  test("\\Seen and \\Flagged set over IMAP change the unread and saved counts", async () => {
    const { imap, http } = await account(3);
    expect(await counts(http)).toEqual({ doc_count: 3, unread_doc_count: 3, saved_doc_count: 0 });
    await imap.command("STORE 1:2 +FLAGS.SILENT (\\Seen)");
    await imap.command("STORE 3 +FLAGS.SILENT (\\Flagged)");
    expect(await counts(http)).toEqual({ doc_count: 3, unread_doc_count: 1, saved_doc_count: 1 });
    imap.close();
  });

  test("a message expunged over IMAP is no longer counted over HTTP, nor served by id", async () => {
    const { imap, http, mails } = await account(2);
    await imap.command("STORE 1 +FLAGS.SILENT (\\Deleted)");
    await imap.command("EXPUNGE");
    expect(await counts(http)).toEqual({ doc_count: 1, unread_doc_count: 1, saved_doc_count: 0 });
    // The row is kept, marked expunged, and every by-id route reads the mark.
    const id = mails[0]!.mail_id;
    expect((await http.get(`/api/mails/body/${id}`)).body).toEqual({ status: "failed", message: "No email is found." });
    expect((await http.post("/api/mails/mark", { mail_id: id, read: true })).body).toEqual({
      status: "failed",
      message: "Invalid request. You may not manipulate other users' email",
    });
    expect((await http.post("/api/mails/spam/mark", { mail_id: id, is_spam: true })).body).toEqual({
      status: "failed",
      message: "Mail not found or you don't have permission",
    });
    expect(await counts(http)).toEqual({ doc_count: 1, unread_doc_count: 1, saved_doc_count: 0 });
    imap.close();
  });

  test("a message moved into Trash over IMAP is still there over HTTP after an EXPUNGE in INBOX", async () => {
    const { imap, http, domain } = await account(2);
    await imap.command("MOVE 1 Trash");
    await imap.command("EXPUNGE");
    expect(await counts(http)).toEqual({ doc_count: 2, unread_doc_count: 2, saved_doc_count: 0 });
    const { body } = await http.get(`/api/mails/headers/hello@${domain}`);
    expect(body.body.map((mail: { subject: string }) => mail.subject).sort()).toEqual(["Mail 1", "Mail 2"]);
    imap.close();
  });

  test("a body fetched over IMAP marks the mail read for HTTP too", async () => {
    const { imap, http } = await account(1);
    await imap.command("FETCH 1 (BODY[])");
    expect(await counts(http)).toEqual({ doc_count: 1, unread_doc_count: 0, saved_doc_count: 0 });
    imap.close();
  });

  test("a message appended over IMAP can be read over HTTP", async () => {
    const { imap, http, user } = await account(0);
    await imap.command([
      "APPEND INBOX ",
      lit("From: a@remote.example\r\nSubject: Appended\r\nMessage-ID: <web@append.example>\r\n\r\nAppended body.\r\n"),
    ]);
    const [row] = await server.db.query("SELECT mail_id FROM mails WHERE user_id = $1", [user.id]);
    const { body } = await http.get(`/api/mails/body/${row!.mail_id}`);
    expect(body.status).toBe("success");
    expect(body.body.html).toContain("Appended body.");
    imap.close();
  });
});

describe("a password reset over HTTP", () => {
  test("ends every IMAP connection signed in with the old password, saying BYE", async () => {
    const { imap, user } = await account(1);
    const idling = await openSession(server, user.username, user.password, "INBOX");
    await idling.idle();
    // A reset token, as the password-reset mail would have carried it.
    await server.db.query(
      "UPDATE users SET token = 'reset-token-e2e', expiry = NOW() + INTERVAL '1 hour' WHERE user_id = $1",
      [user.id]
    );
    const reset = await server.client().post("/api/users/set-info", {
      email: user.email,
      username: user.username,
      password: "a-new-password-e2e",
      token: "reset-token-e2e",
    });
    expect(reset.body.status).toBe("success");

    expect(raws(await imap.waitForClose())).toEqual(["* BYE Credential changed, please re-authenticate"]);
    expect(raws(await idling.waitForClose())).toEqual(["* BYE Credential changed, please re-authenticate"]);

    // The old password no longer signs in; the new one does.
    const again = await ImapClient.connect(server);
    expect((await again.command(`LOGIN ${user.username} ${user.password}`)).text).toBe(
      "[AUTHENTICATIONFAILED] Invalid credentials."
    );
    expect((await again.command(`LOGIN ${user.username} a-new-password-e2e`)).status).toBe("OK");
    again.close();
  });

  test("leaves other accounts' connections alone", async () => {
    const mine = await account(0);
    const theirs = await account(0);
    await server.db.query(
      "UPDATE users SET token = 'reset-token-e2e', expiry = NOW() + INTERVAL '1 hour' WHERE user_id = $1",
      [mine.user.id]
    );
    await server.client().post("/api/users/set-info", {
      email: mine.user.email,
      username: mine.user.username,
      password: "a-new-password-e2e",
      token: "reset-token-e2e",
    });
    await mine.imap.waitForClose();
    expect((await theirs.imap.command("NOOP")).lines).toEqual(["A3 OK NOOP completed"]);
    theirs.imap.close();
  });
});
