import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import fs from "fs";
import path from "path";
import { createUser, type TestServer, type TestUser } from "../harness";
import { ImapClient, lit, openSession, utf8 } from "../harness/imap";
import { crlf, deliver, mailDomain, sendOverHttp, startImapServer, storedUidValidity } from "./support";

let server: TestServer;
let userCount = 0;

interface Box {
  user: TestUser;
  imap: ImapClient;
  domain: string;
}

/** A fresh account and an authenticated session with no mailbox selected. */
const account = async (): Promise<Box> => {
  const user = await createUser(server, `app${++userCount}`);
  const imap = await openSession(server, user.username, user.password);
  return { user, imap, domain: mailDomain(server, user.username) };
};

/** A small message with every usual header; `id` goes into Subject, Message-ID and the body. */
const message = (id: string, extraHeaders: string[] = []): string =>
  crlf(
    "From: Me <me@remote.example>",
    "To: You <you@remote.example>",
    `Subject: Appended ${id}`,
    "Date: Thu, 22 Jan 2026 10:00:00 +0000",
    `Message-ID: <${id}@append.example>`,
    ...extraHeaders,
    "",
    `Body of ${id}.`,
    ""
  );

const rowsOf = (user: TestUser, columns: string) =>
  server.db.query(`SELECT ${columns} FROM mails WHERE user_id = $1 ORDER BY sent, uid_domain`, [user.id]);

/** `seq/uid subject (flags)` for every message of `mailbox`, which is left selected. */
const listing = async (imap: ImapClient, mailbox: string): Promise<string[]> => {
  const selected = await imap.command(`SELECT ${mailbox}`);
  if (selected.status !== "OK") throw new Error(`SELECT ${mailbox}: ${selected.tagged}`);
  const result = await imap.command("FETCH 1:* (UID FLAGS BODY.PEEK[HEADER.FIELDS (SUBJECT)])");
  return result.untagged.map((line) => {
    const match = /^\* (\d+) FETCH \(UID (\d+) FLAGS \(([^)]*)\) .*Subject: ([^\r]*)\r\n/s.exec(line)!;
    return `${match[1]}/${match[2]} ${match[4]} (${match[3]})`;
  });
};

beforeAll(async () => {
  server = await startImapServer();
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

describe("APPEND: the command", () => {
  test("with a synchronizing literal: continuation, then APPENDUID with the account's UIDVALIDITY", async () => {
    const { imap, user } = await account();
    const result = await imap.command(["APPEND INBOX ", lit(message("one"))]);
    const validity = await storedUidValidity(server, user.username);
    expect(result.lines).toEqual(["+ go ahead", `A2 OK [APPENDUID ${validity} 1] APPEND completed`]);
    const second = await imap.command(["APPEND INBOX ", lit(message("two"))]);
    expect(second.lines).toEqual(["+ go ahead", `A3 OK [APPENDUID ${validity} 2] APPEND completed`]);
    imap.close();
  });

  test("with a non-synchronizing literal: no continuation", async () => {
    const { imap, user } = await account();
    const result = await imap.command(["APPEND INBOX ", lit(message("one"), false)]);
    const validity = await storedUidValidity(server, user.username);
    expect(result.lines).toEqual([`A2 OK [APPENDUID ${validity} 1] APPEND completed`]);
    imap.close();
  });

  test("works with no mailbox selected, and with another one selected", async () => {
    const { imap } = await account();
    expect((await imap.command(["APPEND INBOX ", lit(message("one"), false)])).status).toBe("OK");
    await imap.command("SELECT Trash");
    const result = await imap.command(["APPEND INBOX ", lit(message("two"), false)]);
    expect(result.status).toBe("OK");
    expect(result.untagged).toEqual([]);
    imap.close();
  });

  test("the mailbox name may be quoted, a literal, or INBOX in any case", async () => {
    const { imap } = await account();
    await imap.command('CREATE "My Box"');
    const quoted = await imap.command(['APPEND "My Box" ', lit(message("one"), false)]);
    expect(quoted.text).toMatch(/^\[APPENDUID \d+ 1\] APPEND completed$/);
    const literal = await imap.command(["APPEND ", lit("My Box"), " ", lit(message("two"))]);
    expect(literal.lines).toEqual(["+ go ahead", "+ go ahead", expect.stringMatching(/ OK \[APPENDUID \d+ 2\] APPEND completed$/)]);
    // INBOX numbers every received mail, the two in "My Box" included.
    const inbox = await imap.command(["APPEND inbox ", lit(message("three"), false)]);
    expect(inbox.text).toMatch(/^\[APPENDUID \d+ 3\] APPEND completed$/);
    imap.close();
  });

  test("pipelined after its literal, the next command is answered in order", async () => {
    const { imap } = await account();
    const body = message("one");
    imap.write(`P1 APPEND INBOX {${body.length}+}\r\n${body}\r\nP2 NOOP\r\nP3 STATUS INBOX (MESSAGES)\r\n`);
    const result = await imap.readResult("P3");
    expect(result.lines).toEqual([
      expect.stringMatching(/^P1 OK \[APPENDUID \d+ 1\] APPEND completed$/),
      "P2 OK NOOP completed",
      '* STATUS "INBOX" (MESSAGES 1)',
      "P3 OK STATUS completed",
    ]);
    imap.close();
  });

  test("a literal that counts its own closing CRLF is accepted", async () => {
    const { imap } = await account();
    const body = `${message("one")}\r\n`;
    imap.write(`P1 APPEND INBOX {${body.length}}\r\n`);
    expect(await imap.continuation()).toBe("+ go ahead");
    // No further CRLF follows the payload.
    imap.write(body);
    const result = await imap.readResult("P1");
    expect(result.tagged).toMatch(/^P1 OK \[APPENDUID \d+ 1\] APPEND completed$/);
    expect((await imap.command("NOOP")).lines).toEqual(["A2 OK NOOP completed"]);
    imap.close();
  });

  test("flags without parentheses are a syntax error, reported after the literal was taken", async () => {
    const { imap } = await account();
    const result = await imap.command(["APPEND INBOX \\Seen ", lit(message("one"))]);
    expect(result.lines).toEqual(["+ go ahead", "A2 BAD Expected literal"]);
    expect((await imap.command("STATUS INBOX (MESSAGES)")).untagged).toEqual(['* STATUS "INBOX" (MESSAGES 0)']);
    imap.close();
  });

  test("a message literal over 35 MiB is refused with TOOBIG before any octet is sent", async () => {
    const { imap } = await account();
    const limit = 35 * 1024 * 1024;
    const result = await imap.command(`APPEND INBOX {${limit + 1}}`);
    expect(result.lines).toEqual([`A2 NO [TOOBIG] Literal exceeds ${limit} octets`]);
    // Exactly the limit is granted.
    const tag = imap.send(`APPEND INBOX {${limit}}`);
    expect(await imap.continuation()).toBe("+ go ahead");
    expect(tag).toBe("A3");
    imap.close();
  });

  test("an oversized non-synchronizing message literal is refused the same way", async () => {
    const { imap } = await account();
    const limit = 35 * 1024 * 1024;
    const tag = imap.send(`APPEND INBOX {${limit + 1}+}`);
    const result = await imap.readResult(tag);
    expect(result.lines).toEqual([`${tag} NO [TOOBIG] Literal exceeds ${limit} octets`]);
    imap.close();
  });

  test("a mailbox name sent as a literal is held to the 8192-octet argument cap", async () => {
    const { imap } = await account();
    const result = await imap.command(`APPEND {8193}`);
    expect(result.lines).toEqual(["A2 NO [TOOBIG] Literal exceeds 8192 octets"]);
    imap.close();
  });

  test("a 1.8 MB message is stored whole", async () => {
    const { imap, user } = await account();
    const text = Array.from({ length: 40_000 }, (_, i) => `Line ${i} of a large appended message body.`).join("\r\n");
    const large = crlf("From: a@remote.example", "Subject: Large append", "Message-ID: <large@append.example>", "", text, "");
    expect(large.length).toBeGreaterThan(1_800_000);
    const result = await imap.command(["APPEND INBOX ", lit(large)]);
    expect(result.status).toBe("OK");
    const [row] = await server.db.query("SELECT octet_length(text) AS size FROM mails WHERE user_id = $1", [user.id]);
    // Stored with LF line endings: one octet less per line than was sent.
    expect(row!.size).toBe(text.length - 39_999 + 1);
    imap.close();
  });
});

describe("APPEND: what is stored", () => {
  test("the message is parsed into a row: addresses, subject, date, bodies; the Message-ID without brackets", async () => {
    const { imap, user } = await account();
    await imap.command(["APPEND INBOX ", lit(message("row"))]);
    const rows = await rowsOf(
      user,
      `message_id, subject, date, text, html, from_address, from_text, to_address, to_text, cc_address, envelope_to,
       envelope_from, attachments, sent, read, saved, deleted, draft, answered, expunged, is_spam, uid_domain`
    );
    expect(rows).toEqual([
      {
        message_id: "row@append.example",
        subject: "Appended row",
        date: new Date("2026-01-22T10:00:00.000Z"),
        text: "Body of row.\n",
        // As for a delivered message, the text doubles as the HTML body.
        html: "Body of row.\n",
        from_address: [{ name: "Me", address: "me@remote.example" }],
        from_text: '"Me" <me@remote.example>',
        to_address: [{ name: "You", address: "you@remote.example" }],
        to_text: '"You" <you@remote.example>',
        cc_address: null,
        envelope_to: [],
        envelope_from: null,
        attachments: [],
        sent: false,
        read: false,
        saved: false,
        deleted: false,
        draft: false,
        answered: false,
        expunged: false,
        is_spam: false,
        uid_domain: 1,
      },
    ]);
    imap.close();
  });

  test("threading headers and the other address headers are kept", async () => {
    const { imap, user } = await account();
    const reply = message("reply", [
      "In-Reply-To: <row@append.example>",
      "References: <root@append.example> <row@append.example>",
      "Cc: Carbon <cc@remote.example>",
      "Bcc: blind@remote.example",
      "Reply-To: replies@remote.example",
    ]);
    await imap.command(["APPEND INBOX ", lit(reply)]);
    const rows = await rowsOf(user, "in_reply_to, reference_ids, cc_text, bcc_text, reply_to_text");
    expect(rows).toEqual([
      {
        in_reply_to: "<row@append.example>",
        reference_ids: ["<root@append.example>", "<row@append.example>"],
        cc_text: '"Carbon" <cc@remote.example>',
        bcc_text: "blind@remote.example",
        reply_to_text: "replies@remote.example",
      },
    ]);
    imap.close();
  });

  test("each flag in the list sets its column; \\Flagged and \\Deleted also file the mail into Starred and Trash", async () => {
    const { imap, user } = await account();
    await imap.command(["APPEND INBOX (\\Seen \\Flagged \\Answered \\Deleted) ", lit(message("flags"))]);
    expect(await rowsOf(user, "read, saved, answered, deleted, draft")).toEqual([
      { read: true, saved: true, answered: true, deleted: true, draft: false },
    ]);
    const filed = await server.db.query(
      "SELECT mailbox, uid FROM mail_mailbox_uid WHERE user_id = $1 ORDER BY mailbox",
      [user.id]
    );
    expect(filed).toEqual([
      { mailbox: "INBOX", uid: "1" },
      { mailbox: "Starred", uid: "1" },
      { mailbox: "Trash", uid: "1" },
    ]);
    expect(await listing(imap, "INBOX")).toEqual(["1/1 Appended flags (\\Seen \\Flagged \\Deleted \\Answered)"]);
    imap.close();
  });

  test("an empty flag list, keywords, unknown flags and flags in another letter case set nothing", async () => {
    const { imap, user } = await account();
    await imap.command(["APPEND INBOX () ", lit(message("one"), false)]);
    await imap.command(["APPEND INBOX ($Forwarded \\Bogus \\seen NonJunk) ", lit(message("two"), false)]);
    expect(await rowsOf(user, "read, saved, answered, deleted, draft")).toEqual([
      { read: false, saved: false, answered: false, deleted: false, draft: false },
      { read: false, saved: false, answered: false, deleted: false, draft: false },
    ]);
    imap.close();
  });

  test("the date-time argument is ignored: INTERNALDATE is the message's Date header", async () => {
    // NOTE: RFC 3501 6.3.11 sets the internal date from the date-time argument.
    const { imap } = await account();
    await imap.command(['APPEND INBOX (\\Seen) "17-Jan-2025 10:00:00 +0000" ', lit(message("one"), false)]);
    await imap.command(['APPEND INBOX "17-Jan-2025 10:00:00 +0000" ', lit(message("two"), false)]);
    await imap.command("SELECT INBOX");
    expect((await imap.command("FETCH 1:2 (FLAGS INTERNALDATE)")).untagged).toEqual([
      '* 1 FETCH (FLAGS (\\Seen) INTERNALDATE "22-Jan-2026 10:00:00 +0000")',
      '* 2 FETCH (FLAGS () INTERNALDATE "22-Jan-2026 10:00:00 +0000")',
    ]);
    imap.close();
  });

  test("a message without Date or Message-ID gets the time of the APPEND and a generated id", async () => {
    const { imap, user } = await account();
    const before = Date.now();
    await imap.command(["APPEND INBOX ", lit("Subject: bare\r\n\r\nbody\r\n")]);
    const [row] = await rowsOf(user, "message_id, date, subject, text, from_address, to_address");
    expect(row!.message_id).toMatch(/^[0-9a-f]{8}$/);
    expect(new Date(row!.date).getTime()).toBeGreaterThanOrEqual(before - 1000);
    expect(new Date(row!.date).getTime()).toBeLessThanOrEqual(Date.now() + 1000);
    expect<Record<string, unknown>>({ ...row, message_id: "", date: "" }).toEqual({
      message_id: "",
      date: "",
      subject: "bare",
      text: "body\n",
      from_address: null,
      to_address: null,
    });
    imap.close();
  });

  test("a NUL character is left out of what is stored, and a Message-ID too long to store is dropped", async () => {
    // Either one failed the save: PostgreSQL text holds no NUL, and the
    // message_id column holds 512 characters.
    const { imap, user } = await account();
    const appended = await imap.command([
      "APPEND INBOX ",
      lit(crlf("Subject: =?utf-8?Q?nu=00l?=", `Message-ID: <${"x".repeat(600)}@append.example>`, "Content-Transfer-Encoding: quoted-printable", "", "a=00b", "")),
    ]);
    expect(appended.status).toBe("OK");
    const [row] = await rowsOf(user, "message_id, subject, text");
    // Stored as a message without a Message-ID is.
    expect(row!.message_id).toMatch(/^[0-9a-f]{8}$/);
    expect([row!.subject, row!.text]).toEqual(["nul", "ab\n"]);
    imap.close();
  });

  test("attachments are stored as files and served back", async () => {
    const { imap, user } = await account();
    const withAttachment = crlf(
      "From: a@remote.example",
      "Subject: With attachment",
      "Message-ID: <att@append.example>",
      "MIME-Version: 1.0",
      'Content-Type: multipart/mixed; boundary="b"',
      "",
      "--b",
      "Content-Type: text/plain",
      "",
      "text part",
      "--b",
      'Content-Type: application/octet-stream; name="f.bin"',
      'Content-Disposition: attachment; filename="f.bin"',
      "Content-Transfer-Encoding: base64",
      "",
      "AAECAwQ=",
      "--b--",
      ""
    );
    await imap.command(["APPEND INBOX ", lit(withAttachment)]);
    const [row] = await rowsOf(user, "text, attachments");
    expect(row!.text).toBe("text part");
    expect(row!.attachments).toEqual([
      { size: 5, content: { data: expect.any(String) }, filename: "f.bin", contentType: "application/octet-stream" },
    ]);
    const file = path.join(server.cwd, "attachments", row!.attachments[0].content.data);
    expect([...fs.readFileSync(file)]).toEqual([0, 1, 2, 3, 4]);
    await imap.command("SELECT INBOX");
    expect((await imap.command("FETCH 1 (BODY.PEEK[2])")).untagged).toEqual(["* 1 FETCH (BODY[2] {10}\r\nAAECAwQ=\r\n)"]);
    imap.close();
  });

  test("an HTML message gets a text body derived from it", async () => {
    const { imap, user } = await account();
    const html = crlf(
      "From: a@remote.example",
      "Subject: HTML",
      "Message-ID: <html@append.example>",
      "MIME-Version: 1.0",
      "Content-Type: text/html; charset=utf-8",
      "",
      "<p>Hello <b>html</b></p>",
      ""
    );
    await imap.command(["APPEND INBOX ", lit(html)]);
    expect(await rowsOf(user, "text, html")).toEqual([{ text: "Hello html", html: "<p>Hello <b>html</b></p>\n" }]);
    imap.close();
  });

  test("8-bit UTF-8 content is kept", async () => {
    const { imap, user } = await account();
    const raw = utf8(
      crlf(
        "From: Zoë <zoe@remote.example>",
        "Subject: Grüße ✉",
        "Message-ID: <utf8@append.example>",
        "Content-Type: text/plain; charset=utf-8",
        "Content-Transfer-Encoding: 8bit",
        "",
        "Emoji 😀 and 日本語.",
        ""
      )
    );
    await imap.command(["APPEND INBOX ", lit(raw)]);
    expect(await rowsOf(user, "subject, text, from_text")).toEqual([
      { subject: "Grüße ✉", text: "Emoji 😀 and 日本語.\n", from_text: '"Zoë" <zoe@remote.example>' },
    ]);
    imap.close();
  });
});

describe("APPEND: malformed and empty messages are stored as they parse", () => {
  const cases: [string, string | Buffer, { subject: string; text: string }][] = [
    ["an empty literal", "", { subject: "", text: "" }],
    ["text with no header block", "no headers at all, just text", { subject: "", text: "" }],
    ["a header block with no body", "Subject: only subject\r\n\r\n", { subject: "only subject", text: "" }],
    ["a body with no header block", "\r\nbody only\r\n", { subject: "", text: "body only\n" }],
  ];

  for (const [label, literal, expected] of cases) {
    test(label, async () => {
      const { imap, user } = await account();
      const result = await imap.command(["APPEND INBOX ", lit(literal)]);
      expect(result.text).toMatch(/^\[APPENDUID \d+ 1\] APPEND completed$/);
      const [row] = await rowsOf(user, "subject, text, message_id, from_address");
      expect({ subject: row!.subject, text: row!.text }).toEqual(expected);
      expect(row!.message_id).toMatch(/^[0-9a-f]{8}$/);
      expect(row!.from_address).toBeNull();
      imap.close();
    });
  }

  test("an empty literal is answered at once, without waiting for a closing CRLF", async () => {
    const { imap } = await account();
    imap.write("E1 APPEND INBOX {0}\r\n");
    expect(await imap.continuation()).toBe("+ go ahead");
    const result = await imap.readResult("E1");
    expect(result.tagged).toMatch(/^E1 OK \[APPENDUID \d+ 1\] APPEND completed$/);
    // The CRLF a client then sends is read as an empty line and ignored.
    imap.write("\r\n");
    expect((await imap.command("NOOP")).lines).toEqual(["A2 OK NOOP completed"]);
    imap.close();
  });

  test("binary octets are accepted", async () => {
    const { imap } = await account();
    const result = await imap.command(["APPEND INBOX ", lit(Buffer.from([0, 1, 2, 255, 254, 13, 10, 13, 10, 200]))]);
    expect(result.text).toMatch(/^\[APPENDUID \d+ 1\] APPEND completed$/);
    imap.close();
  });
});

describe("APPEND: the same Message-ID again", () => {
  test("to the same mailbox: the first message's UID is answered and nothing is added", async () => {
    const { imap, user } = await account();
    const first = await imap.command(["APPEND INBOX ", lit(message("dup"))]);
    const again = await imap.command(["APPEND INBOX (\\Seen) ", lit(message("dup").replace("Body of", "Another body of"))]);
    expect(first.text).toMatch(/^\[APPENDUID \d+ 1\] APPEND completed$/);
    expect(again.text).toBe(first.text!);
    // The stored message is the first one, unchanged.
    expect(await rowsOf(user, "text, read")).toEqual([{ text: "Body of dup.\n", read: false }]);
    // A different message still gets the next UID.
    const next = await imap.command(["APPEND INBOX ", lit(message("other"))]);
    expect(next.text).toMatch(/^\[APPENDUID \d+ 3\] APPEND completed$/);
    imap.close();
  });

  test("to another mailbox: the one stored message is filed there too", async () => {
    const { imap, user } = await account();
    await imap.command("CREATE folder");
    await imap.command(["APPEND INBOX ", lit(message("dup"))]);
    const second = await imap.command(["APPEND folder ", lit(message("dup"))]);
    expect(second.text).toMatch(/^\[APPENDUID \d+ 1\] APPEND completed$/);
    expect(await rowsOf(user, "message_id")).toEqual([{ message_id: "dup@append.example" }]);
    const filed = await server.db.query(
      "SELECT mailbox, uid FROM mail_mailbox_uid WHERE user_id = $1 ORDER BY mailbox",
      [user.id]
    );
    expect(filed).toEqual([
      { mailbox: "INBOX", uid: "1" },
      { mailbox: "folder", uid: "1" },
    ]);
    // One message with one set of flags, seen from both mailboxes.
    await imap.command("SELECT folder");
    await imap.command("STORE 1 +FLAGS.SILENT (\\Flagged)");
    expect(await listing(imap, "INBOX")).toEqual(["1/1 Appended dup (\\Flagged)"]);
    imap.close();
  });

  test("the id of a delivered message does not collide: its brackets are part of the stored id", async () => {
    const { imap, user, domain } = await account();
    await deliver(server, { to: `hello@${domain}`, messageId: "<same@append.example>", subject: "Delivered" });
    const result = await imap.command(["APPEND INBOX ", lit(message("same"))]);
    expect(result.text).toMatch(/^\[APPENDUID \d+ 2\] APPEND completed$/);
    expect(await rowsOf(user, "message_id")).toEqual([
      { message_id: "<same@append.example>" },
      { message_id: "same@append.example" },
    ]);
    imap.close();
  });
});

describe("APPEND: each kind of mailbox", () => {
  test("a user mailbox: UIDs of that mailbox; the message is also in INBOX", async () => {
    const { imap, user } = await account();
    await imap.command("CREATE folder");
    const result = await imap.command(["APPEND folder ", lit(message("one"))]);
    expect(result.text).toMatch(/^\[APPENDUID \d+ 1\] APPEND completed$/);
    expect(await listing(imap, "folder")).toEqual(["1/1 Appended one ()"]);
    expect(await listing(imap, "INBOX")).toEqual(["1/1 Appended one ()"]);
    expect(await rowsOf(user, "sent, uid_domain")).toEqual([{ sent: false, uid_domain: 1 }]);
    imap.close();
  });

  test("a per-address mailbox: filed there and in INBOX, the To header left as it was", async () => {
    const { imap, domain } = await account();
    await deliver(server, { to: `hello@${domain}`, subject: "Delivered" });
    const result = await imap.command(['APPEND "INBOX/accounts/hello" ', lit(message("one"))]);
    expect(result.text).toMatch(/^\[APPENDUID \d+ 2\] APPEND completed$/);
    expect(await listing(imap, '"INBOX/accounts/hello"')).toEqual(["1/1 Delivered ()", "2/2 Appended one ()"]);
    expect((await imap.command("FETCH 2 ENVELOPE")).untagged[0]).toContain('(("You" NIL "you" "remote.example"))');
    expect(await listing(imap, "INBOX")).toEqual(["1/1 Delivered ()", "2/2 Appended one ()"]);
    imap.close();
  });

  test("Drafts: \\Draft is set whether or not the client sent it, and the message is not in INBOX", async () => {
    const { imap, user } = await account();
    const plain = await imap.command(["APPEND Drafts ", lit(message("one"))]);
    const flagged = await imap.command(["APPEND drafts (\\Seen) ", lit(message("two"))]);
    expect(plain.text).toMatch(/^\[APPENDUID \d+ 1\] APPEND completed$/);
    expect(flagged.text).toMatch(/^\[APPENDUID \d+ 2\] APPEND completed$/);
    expect(await listing(imap, "Drafts")).toEqual(["1/1 Appended one (\\Draft)", "2/2 Appended two (\\Seen \\Draft)"]);
    expect(await listing(imap, "INBOX")).toEqual([]);
    expect(await rowsOf(user, "draft, sent")).toEqual([
      { draft: true, sent: false },
      { draft: true, sent: false },
    ]);
    imap.close();
  });

  test("INBOX with \\Draft: the message lands in Drafts instead", async () => {
    const { imap } = await account();
    const result = await imap.command(["APPEND INBOX (\\Draft) ", lit(message("one"))]);
    expect(result.text).toMatch(/^\[APPENDUID \d+ 1\] APPEND completed$/);
    expect(await listing(imap, "INBOX")).toEqual([]);
    expect(await listing(imap, "Drafts")).toEqual(["1/1 Appended one (\\Draft)"]);
    imap.close();
  });

  test("Trash sets \\Deleted, Starred sets \\Flagged, Junk marks the mail as spam", async () => {
    const { imap, user } = await account();
    await imap.command(["APPEND Trash ", lit(message("trash"))]);
    await imap.command(["APPEND Starred ", lit(message("star"))]);
    await imap.command(["APPEND Junk ", lit(message("junk"))]);
    expect(await rowsOf(user, "subject, deleted, saved, is_spam, uid_domain")).toEqual([
      { subject: "Appended trash", deleted: true, saved: false, is_spam: false, uid_domain: 1 },
      { subject: "Appended star", deleted: false, saved: true, is_spam: false, uid_domain: 2 },
      { subject: "Appended junk", deleted: false, saved: false, is_spam: true, uid_domain: 3 },
    ]);
    expect(await listing(imap, "Trash")).toEqual(["1/1 Appended trash (\\Deleted)"]);
    expect(await listing(imap, "Starred")).toEqual(["1/1 Appended star (\\Flagged)"]);
    // Junk numbers its mail in INBOX's UID space.
    expect(await listing(imap, "Junk")).toEqual(["1/3 Appended junk ()"]);
    // Trash and Starred mail is received mail and shows in INBOX; spam does not.
    expect(await listing(imap, "INBOX")).toEqual(["1/1 Appended trash (\\Deleted)", "2/2 Appended star (\\Flagged)"]);
    imap.close();
  });

  test("the sent mailboxes: stored as sent mail, with UIDs of the sent side, and not in INBOX", async () => {
    const { imap, user } = await account();
    await sendOverHttp(server, user, { sender: "me", to: "x@remote.example", subject: "Sent one", html: "<p>s</p>" });
    const unified = await imap.command(['APPEND "Sent Messages" (\\Seen) ', lit(message("two"))]);
    const perAddress = await imap.command(['APPEND "Sent Messages/accounts/me" ', lit(message("three"))]);
    expect(unified.text).toMatch(/^\[APPENDUID \d+ 2\] APPEND completed$/);
    expect(perAddress.text).toMatch(/^\[APPENDUID \d+ 2\] APPEND completed$/);
    expect(await listing(imap, '"Sent Messages"')).toEqual([
      "1/1 Sent one (\\Seen)",
      "2/2 Appended two (\\Seen)",
      "3/3 Appended three ()",
    ]);
    // Only the append that named the per-address mailbox is filed in it.
    expect(await listing(imap, '"Sent Messages/accounts/me"')).toEqual(["1/1 Sent one (\\Seen)", "2/2 Appended three ()"]);
    expect(await listing(imap, "INBOX")).toEqual([]);
    expect(await rowsOf(user, "subject, sent")).toEqual([
      { subject: "Sent one", sent: true },
      { subject: "Appended two", sent: true },
      { subject: "Appended three", sent: true },
    ]);
    imap.close();
  });

  test("a mailbox that does not exist: TRYCREATE, after the literal has been taken, and nothing is stored", async () => {
    const { imap, user } = await account();
    for (const name of ["no-such-box", '"INBOX/accounts/nobody"', '"Sent Messages"']) {
      const sync = await imap.command([`APPEND ${name} `, lit(message("one"))]);
      expect({ name, lines: sync.lines }).toEqual({
        name,
        lines: ["+ go ahead", `${sync.tag} NO [TRYCREATE] Mailbox does not exist`],
      });
      const nonSync = await imap.command([`APPEND ${name} `, lit(message("one"), false)]);
      expect(nonSync.lines).toEqual([`${nonSync.tag} NO [TRYCREATE] Mailbox does not exist`]);
    }
    expect(await rowsOf(user, "subject")).toEqual([]);
    expect((await imap.command('LIST "" "no-such-box"')).untagged).toEqual([]);
    imap.close();
  });

  test("the \\Noselect accounts parent accepts an APPEND", async () => {
    // NOTE: looks like a bug, as for COPY: the message is filed under a
    // mailbox that cannot be selected, and shows only in INBOX.
    const { imap, domain } = await account();
    await deliver(server, { to: `hello@${domain}`, subject: "Delivered" });
    const result = await imap.command(['APPEND "INBOX/accounts" ', lit(message("one"))]);
    expect(result.text).toMatch(/^\[APPENDUID \d+ 1\] APPEND completed$/);
    expect(await listing(imap, "INBOX")).toEqual(["1/1 Delivered ()", "2/2 Appended one ()"]);
    imap.close();
  });
});

describe("APPEND: into the selected mailbox", () => {
  test("no EXISTS is announced, though the session can fetch the new message at once", async () => {
    // NOTE: RFC 3501 6.3.11 says the server SHOULD send an untagged EXISTS
    // when the mailbox is selected. Here the session's own numbering grows
    // silently: message 2 exists for FETCH, but the client was never told.
    const { imap } = await account();
    await imap.command(["APPEND INBOX ", lit(message("one"))]);
    await imap.command("SELECT INBOX");
    const result = await imap.command(["APPEND INBOX ", lit(message("two"))]);
    expect(result.untagged).toEqual([]);
    expect((await imap.command("NOOP")).lines).toEqual(["A5 OK NOOP completed"]);
    expect((await imap.command("FETCH 1:* (UID)")).untagged).toEqual(["* 1 FETCH (UID 1)", "* 2 FETCH (UID 2)"]);
    imap.close();
  });

  test("an APPEND to another mailbox leaves the selected one's view alone", async () => {
    const { imap } = await account();
    await imap.command("CREATE folder");
    await imap.command(["APPEND INBOX ", lit(message("one"))]);
    await imap.command("SELECT folder");
    await imap.command(["APPEND folder ", lit(message("two"))]);
    await imap.command(["APPEND INBOX ", lit(message("three"))]);
    expect((await imap.command("FETCH 1:* (UID)")).untagged).toEqual(["* 1 FETCH (UID 1)"]);
    imap.close();
  });

  test("an examined mailbox accepts an APPEND too", async () => {
    const { imap } = await account();
    await imap.command("EXAMINE INBOX");
    const result = await imap.command(["APPEND INBOX ", lit(message("one"))]);
    expect(result.text).toMatch(/^\[APPENDUID \d+ 1\] APPEND completed$/);
    imap.close();
  });
});
