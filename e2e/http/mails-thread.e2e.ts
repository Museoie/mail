import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { addr, missingId, readMail, startServer, type InsertedMail, type TestServer } from "../harness";
import {
  BODY_ETAG,
  JSON_TYPE,
  adminMailbox,
  failed,
  headerOf,
  newMailbox,
  type SeedRow,
} from "./mails-helpers";

let server: TestServer;

beforeAll(async () => {
  server = await startServer();
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

const NOT_FOUND = failed("No email is found.");

/** A mailbox whose seeded mails get ascending dates in insertion order. */
const conversation = async () => {
  const box = await newMailbox(server);
  let second = 0;
  const mail = (subject: string, row: SeedRow = {}) =>
    box.mail({ subject, date: new Date(Date.UTC(2026, 2, 1, 0, 0, second++)), ...row });
  const thread = async (seed: InsertedMail) => {
    const { status, body } = await box.client.get(`/api/mails/thread/${seed.mail_id}`);
    expect(status).toBe(200);
    expect(body.status).toBe("success");
    return (body.body as { subject: string; spam?: boolean }[]).map((h) => (h.spam ? `${h.subject} [spam]` : h.subject));
  };
  return { box, mail, thread };
};

describe("GET /api/mails/thread/:id — the response", () => {
  test("a mail with no conversation is a list of itself, byte for byte like a list row", async () => {
    const box = await newMailbox(server);
    const me = box.address();
    const mail = await box.mail({ subject: "Alone", text: "Body", date: "2026-01-02T03:04:05.678Z", message_id: "<t@x>" });
    const { status, text, headers } = await box.client.get(`/api/mails/thread/${mail.mail_id}`);
    expect(status).toBe(200);
    expect(text).toBe(
      `{"status":"success","body":[{"id":"${mail.mail_id}","subject":"Alone","date":"2026-01-02T03:04:05.678Z",` +
        `"from":{"value":[{"name":"Remote Sender","address":"sender@remote.example"}],"text":"Remote Sender <sender@remote.example>"},` +
        `"to":{"value":[{"address":"${me}"}],"text":"${me}"},"envelopeTo":{"value":[{"address":"${me}"}],"text":""},` +
        `"read":false,"saved":false,"sent":false,"insight":null,"snippet":"Body","messageId":"<t@x>"}]}`
    );
    expect(headers.get("content-type")).toBe(JSON_TYPE);
    expect(headers.get("etag")).toMatch(BODY_ETAG);
    expect(headers.get("content-length")).toBe(String(Buffer.byteLength(text)));
    expect(text).toBe((await box.client.get(`/api/mails/headers/${me}`)).text);
  });

  test("rows are list rows, oldest first, with every reference kept (not cut to three)", async () => {
    const box = await newMailbox(server);
    const root = await box.mail({ subject: "Root", message_id: "<root@x>", date: "2026-01-01T00:00:00Z", read: true, saved: true });
    const reply = await box.mail({
      subject: "Re: Root",
      message_id: "<reply@x>",
      in_reply_to: "<root@x>",
      reference_ids: ["<a@x>", "<b@x>", "<c@x>", "<d@x>", "<root@x>"],
      cc: [addr("c@remote.example", "C")],
      date: "2026-01-02T00:00:00Z",
      insight: { summary: ["s"] },
    });
    const { body } = await box.client.get(`/api/mails/thread/${reply.mail_id}`);
    expect(body.body).toEqual([
      headerOf(root),
      headerOf(reply, { references: ["<a@x>", "<b@x>", "<c@x>", "<d@x>", "<root@x>"] }),
    ]);
    expect(Object.keys(body.body[1])).toEqual([
      "id", "subject", "date", "from", "to", "cc", "envelopeTo", "read", "saved", "sent", "insight", "snippet",
      "messageId", "inReplyTo", "references",
    ]);
  });

  test("opening a thread does not mark anything read", async () => {
    const box = await newMailbox(server);
    const mail = await box.mail();
    await box.client.get(`/api/mails/thread/${mail.mail_id}`);
    expect((await readMail(server, mail.mail_id))!.read).toBe(false);
  });
});

describe("GET /api/mails/thread/:id — which mail may be opened", () => {
  test("refuses an id that is not a UUID, a missing id and another user's mail", async () => {
    const box = await newMailbox(server);
    const other = await newMailbox(server);
    const theirs = await other.mail();
    for (const id of ["abc", "0", missingId(), theirs.mail_id, `${theirs.mail_id}0`, "%20", "x".repeat(3000)]) {
      const { status, body } = await box.client.get(`/api/mails/thread/${id}`);
      expect({ id: id.slice(0, 40), status, body }).toEqual({ id: id.slice(0, 40), status: 200, body: NOT_FOUND });
    }
    const admin = await adminMailbox(server);
    expect((await admin.client.get(`/api/mails/thread/${theirs.mail_id}`)).body).toEqual(NOT_FOUND);
  });

  test("the refusal is exact", async () => {
    const box = await newMailbox(server);
    const { status, text } = await box.client.get("/api/mails/thread/abc");
    expect(status).toBe(200);
    expect(text).toBe('{"status":"failed","message":"No email is found."}');
  });

  test("accepts the id in upper case", async () => {
    const box = await newMailbox(server);
    const mail = await box.mail();
    const { body } = await box.client.get(`/api/mails/thread/${mail.mail_id.toUpperCase()}`);
    expect(body.body.map((h: { id: string }) => h.id)).toEqual([mail.mail_id]);
  });

  test("an expunged mail cannot be opened, nor its body read", async () => {
    const box = await newMailbox(server);
    const mail = await box.mail({ expunged: true });
    expect((await box.client.get(`/api/mails/thread/${mail.mail_id}`)).body).toEqual(NOT_FOUND);
    expect((await box.client.get(`/api/mails/body/${mail.mail_id}`)).body).toEqual(NOT_FOUND);
  });
});

describe("GET /api/mails/thread/:id — threading by headers", () => {
  test("follows In-Reply-To and References in both directions, whatever the subjects", async () => {
    const { mail, thread } = await conversation();
    const root = await mail("Root", { message_id: "<root@x>" });
    const r1 = await mail("Re: Root", { message_id: "<r1@x>", in_reply_to: "<root@x>", reference_ids: ["<root@x>"] });
    const r2 = await mail("A new subject entirely", { message_id: "<r2@x>", in_reply_to: "<r1@x>" });
    const r3 = await mail("Only references", { message_id: "<r3@x>", reference_ids: ["<r2@x>"] });
    await mail("Unrelated", { message_id: "<u@x>" });
    await mail("Re: Root", { message_id: "<lookalike@x>", in_reply_to: "<elsewhere@x>" });
    const expected = ["Root", "Re: Root", "A new subject entirely", "Only references"];
    for (const seed of [root, r1, r2, r3]) expect(await thread(seed)).toEqual(expected);
  });

  test("two replies to one mail are in one thread with each other", async () => {
    const { mail, thread } = await conversation();
    await mail("Root", { message_id: "<root@x>" });
    const a = await mail("Reply A", { message_id: "<a@x>", in_reply_to: "<root@x>" });
    await mail("Reply B", { message_id: "<b@x>", in_reply_to: "<root@x>" });
    expect(await thread(a)).toEqual(["Root", "Reply A", "Reply B"]);
  });

  test("mails sharing a referenced id are threaded even when the referenced mail is not stored", async () => {
    const { mail, thread } = await conversation();
    const a = await mail("First", { message_id: "<a@x>", reference_ids: ["<absent@x>"] });
    await mail("Second", { message_id: "<b@x>", in_reply_to: "<absent@x>" });
    expect(await thread(a)).toEqual(["First", "Second"]);
  });

  test("matches ids with or without angle brackets on either side", async () => {
    const { mail, thread } = await conversation();
    const root = await mail("Bare root", { message_id: "root@x" });
    await mail("Bracketed reply to bare", { message_id: "<r1@x>", in_reply_to: "<root@x>" });
    await mail("Bare reply to bracketed", { message_id: "r2@x", in_reply_to: "r1@x" });
    await mail("Bare reference", { message_id: "<r3@x>", reference_ids: ["r2@x"] });
    expect(await thread(root)).toEqual([
      "Bare root",
      "Bracketed reply to bare",
      "Bare reply to bracketed",
      "Bare reference",
    ]);
  });

  test("includes sent mail, read mail and mail of any account of the user", async () => {
    const { box, mail, thread } = await conversation();
    const root = await mail("Root", { message_id: "<root@x>", read: true });
    await mail("My answer", {
      message_id: "<mine@x>",
      in_reply_to: "<root@x>",
      from: [addr(box.address())],
      to: [addr("sender@remote.example")],
      envelope_to: [],
      sent: true,
      read: true,
    });
    await mail("Their answer to another account", { message_id: "<theirs@x>", in_reply_to: "<mine@x>", to: [addr(box.address("other"))] });
    expect(await thread(root)).toEqual(["Root", "My answer", "Their answer to another account"]);
  });

  test("leaves out drafts, expunged mail and other users' mail", async () => {
    const { mail, thread } = await conversation();
    const other = await newMailbox(server);
    const root = await mail("Root", { message_id: "<root@x>" });
    await mail("Draft reply", { message_id: "<draft@x>", in_reply_to: "<root@x>", draft: true });
    await mail("Expunged reply", { message_id: "<gone@x>", in_reply_to: "<root@x>", expunged: true });
    await other.mail({ subject: "Other user's reply", message_id: "<other@x>", in_reply_to: "<root@x>" });
    await mail("Kept reply", { message_id: "<kept@x>", in_reply_to: "<root@x>" });
    expect(await thread(root)).toEqual(["Root", "Kept reply"]);
  });

  test("a draft opened by id is shown inside its conversation", async () => {
    const { mail, thread } = await conversation();
    await mail("Root", { message_id: "<root@x>" });
    const draft = await mail("Draft reply", { message_id: "<draft@x>", in_reply_to: "<root@x>", draft: true });
    expect(await thread(draft)).toEqual(["Root", "Draft reply"]);
  });

  test("spam stays out unless the opened mail is spam; then spam rows are labelled", async () => {
    const { mail, thread } = await conversation();
    const root = await mail("Root", { message_id: "<root@x>" });
    const spam = await mail("Spam reply", { message_id: "<spam@x>", in_reply_to: "<root@x>", is_spam: true });
    await mail("More spam", { message_id: "<spam2@x>", in_reply_to: "<spam@x>", is_spam: true });
    await mail("Ham reply", { message_id: "<ham@x>", in_reply_to: "<root@x>" });
    expect(await thread(root)).toEqual(["Root", "Ham reply"]);
    expect(await thread(spam)).toEqual(["Root", "Spam reply [spam]", "More spam [spam]", "Ham reply"]);
  });

  test("a spam row carries spam: true as its last key; other rows have no spam key", async () => {
    const box = await newMailbox(server);
    const root = await box.mail({ subject: "Root", message_id: "<root@x>", date: "2026-01-01T00:00:00Z" });
    const spam = await box.mail({ subject: "Spam", message_id: "<s@x>", in_reply_to: "<root@x>", is_spam: true, date: "2026-01-02T00:00:00Z" });
    const { body } = await box.client.get(`/api/mails/thread/${spam.mail_id}`);
    expect(body.body).toEqual([headerOf(root), headerOf(spam, { spam: true })]);
    expect(Object.keys(body.body[1]).at(-1)).toBe("spam");
    expect("spam" in body.body[0]).toBe(false);
  });

  // NOTE: a copy made by IMAP COPY/MOVE is recognized by its Message-ID
  // ending in ".copy@server". The filter is a plain suffix match on the stored
  // text, so a copy id stored with a closing bracket ("<…copy@server>") does
  // not end with the suffix and is shown next to its original.
  test("leaves out IMAP copies by Message-ID suffix, but not when the id ends with a bracket", async () => {
    const { mail, thread } = await conversation();
    const root = await mail("Root", { message_id: "<root@x>" });
    await mail("Copy, bare id", { message_id: "abc.copy@server", in_reply_to: "<root@x>" });
    await mail("Copy, open bracket only", { message_id: "<abc2.copy@server", in_reply_to: "<root@x>" });
    await mail("Copy, bracketed id", { message_id: "<abc3.copy@server>", in_reply_to: "<root@x>" });
    expect(await thread(root)).toEqual(["Root", "Copy, bracketed id"]);
  });

  test("a copy opened by id is shown with its conversation", async () => {
    const { mail, thread } = await conversation();
    await mail("Root", { message_id: "<root@x>" });
    const copy = await mail("The copy", { message_id: "abc.copy@server", in_reply_to: "<root@x>" });
    expect(await thread(copy)).toEqual(["Root", "The copy"]);
  });

  test("follows a chain four links out from the opened mail, and no further", async () => {
    const { mail, thread } = await conversation();
    const chain: InsertedMail[] = [];
    for (let i = 0; i < 9; i++) {
      chain.push(await mail(`link ${i}`, { message_id: `<l${i}@x>`, in_reply_to: i ? `<l${i - 1}@x>` : null }));
    }
    // From the end: the mail it answers is one link; three more rounds follow.
    expect(await thread(chain[8]!)).toEqual(["link 4", "link 5", "link 6", "link 7", "link 8"]);
    // From the start: its direct reply is found at once, then one more per round.
    expect(await thread(chain[0]!)).toEqual(["link 0", "link 1", "link 2", "link 3", "link 4"]);
    // From the middle it reaches both ways.
    expect(await thread(chain[4]!)).toEqual([
      "link 0", "link 1", "link 2", "link 3", "link 4", "link 5", "link 6", "link 7", "link 8",
    ]);
  });

  test("a full References header reaches the whole conversation in one step", async () => {
    const { mail, thread } = await conversation();
    const all: InsertedMail[] = [];
    for (let i = 0; i < 9; i++) {
      all.push(
        await mail(`ref ${i}`, {
          message_id: `<f${i}@x>`,
          in_reply_to: i ? `<f${i - 1}@x>` : null,
          reference_ids: i ? Array.from({ length: i }, (_, k) => `<f${k}@x>`) : null,
        })
      );
    }
    const expected = all.map((m) => m.subject!);
    expect(await thread(all[8]!)).toEqual(expected);
    expect(await thread(all[0]!)).toEqual(expected);
  });

  test("orders by date, oldest first; among equal dates the opened mail comes first", async () => {
    const box = await newMailbox(server);
    const t1 = await box.mail({ subject: "same time A", message_id: "<t1@x>", date: "2026-06-01T00:00:00Z" });
    const t2 = await box.mail({ subject: "same time B", message_id: "<t2@x>", in_reply_to: "<t1@x>", date: "2026-06-01T00:00:00Z" });
    await box.mail({ subject: "earlier reply", message_id: "<t3@x>", in_reply_to: "<t1@x>", date: "2026-05-01T00:00:00+09:00" });
    await box.mail({ subject: "later", message_id: "<t4@x>", in_reply_to: "<t1@x>", date: "2026-06-01T00:00:00.001Z" });
    const subjectsOf = async (seed: InsertedMail) =>
      (await box.client.get(`/api/mails/thread/${seed.mail_id}`)).body.body.map((h: { subject: string }) => h.subject);
    expect(await subjectsOf(t1)).toEqual(["earlier reply", "same time A", "same time B", "later"]);
    expect(await subjectsOf(t2)).toEqual(["earlier reply", "same time B", "same time A", "later"]);
  });
});

describe("GET /api/mails/thread/:id — very large conversations", () => {
  const bulk = (userId: string, me: string, count: number, columns: string, values: string) =>
    server.db.query(
      `INSERT INTO mails (user_id, message_id, subject, date, to_address, to_text, envelope_to, from_address, from_text, text${columns})
       SELECT $1, '<bulk-' || g || '@x>', 'Re: Huge', timestamptz '2026-01-01T00:00:00Z' + g * interval '1 second',
              $2::jsonb, $3, $2::jsonb, '[{"address":"pm@corp.example"}]'::jsonb, 'pm@corp.example', 'reply ' || g${values}
       FROM generate_series(1, ${count}) g`,
      [userId, [{ address: me }], me]
    );

  test("by headers: the opened mail and the 300 newest mails linked to it", async () => {
    const box = await newMailbox(server);
    const root = await box.mail({ subject: "Huge", message_id: "<huge-root@x>", date: "2025-12-31T00:00:00Z" });
    await bulk(box.id, box.address(), 320, ", in_reply_to", ", '<huge-root@x>'");
    const { body } = await box.client.get(`/api/mails/thread/${root.mail_id}`);
    expect(body.body).toHaveLength(301);
    expect(body.body[0].id).toBe(root.mail_id);
    expect(body.body[1].snippet).toBe("reply 21");
    expect(body.body[300].snippet).toBe("reply 320");
  }, 60_000);

  test("by subject: the opened mail and the 100 newest header-less mails of that subject", async () => {
    const box = await newMailbox(server);
    const root = await box.mail({ subject: "Huge", from: [{ address: "pm@corp.example" }], date: "2025-12-31T00:00:00Z" });
    await bulk(box.id, box.address(), 120, "", "");
    const { body } = await box.client.get(`/api/mails/thread/${root.mail_id}`);
    expect(body.body).toHaveLength(101);
    expect(body.body[0].id).toBe(root.mail_id);
    expect(body.body[1].snippet).toBe("reply 21");
    expect(body.body[100].snippet).toBe("reply 120");
  }, 60_000);
});

describe("GET /api/mails/thread/:id — threading header-less mail by subject", () => {
  const pm = [addr("pm@corp.example", "PM")];

  test("joins mails with the same subject once reply and forward prefixes are stripped", async () => {
    const { mail, thread } = await conversation();
    const first = await mail("Project kickoff", { from: pm });
    await mail("Re: Project kickoff", { from: pm });
    await mail("RE:   project   KICKOFF  ", { from: pm });
    await mail("Fwd: Project kickoff", { from: pm });
    await mail("FW: Project kickoff", { from: pm });
    await mail("Re: Re: Fwd: Project kickoff", { from: pm });
    await mail("Re[2]: Project kickoff", { from: pm });
    await mail("Re： Project kickoff", { from: pm });
    await mail("AW: Project kickoff", { from: pm });
    await mail("  re : Project kickoff", { from: pm });
    for (const prefix of ["WG", "SV", "VS", "Antw", "TR", "RV"]) await mail(`${prefix}: Project kickoff`, { from: pm });
    await mail("Re: Project kickoff!", { from: pm });
    await mail("Re: Project kick off", { from: pm });
    await mail("Reply: Project kickoff", { from: pm });
    await mail("Project kickoff (Re:)", { from: pm });
    expect(await thread(first)).toEqual([
      "Project kickoff",
      "Re: Project kickoff",
      "RE:   project   KICKOFF  ",
      "Fwd: Project kickoff",
      "FW: Project kickoff",
      "Re: Re: Fwd: Project kickoff",
      "Re[2]: Project kickoff",
      "Re： Project kickoff",
      "AW: Project kickoff",
      "  re : Project kickoff",
      "WG: Project kickoff",
      "SV: Project kickoff",
      "VS: Project kickoff",
      "Antw: Project kickoff",
      "TR: Project kickoff",
      "RV: Project kickoff",
    ]);
  });

  test("needs a reply or forward marker on at least one of two mails", async () => {
    const { mail, thread } = await conversation();
    const a = await mail("Weekly report", { from: pm });
    await mail("Weekly report", { from: pm });
    expect(await thread(a)).toEqual(["Weekly report"]);
    // A marked mail links the two unmarked ones through itself.
    await mail("Re: Weekly report", { from: pm });
    expect(await thread(a)).toEqual(["Weekly report", "Weekly report", "Re: Weekly report"]);
  });

  test("needs a shared participant on From, To or Cc; Bcc does not count", async () => {
    const { mail, thread } = await conversation();
    const first = await mail("Invoice", { from: pm });
    await mail("Re: Invoice", { from: [addr("stranger@else.example")] });
    await mail("Re: Invoice", { from: [addr("x@else.example")], bcc: pm });
    expect(await thread(first)).toEqual(["Invoice"]);
    await mail("Re: Invoice (cc)", { subject: "Re: Invoice", from: [addr("dev@corp.example")], cc: pm });
    await mail("Re: Invoice", { from: [addr("y@else.example")], to: [addr("PM@CORP.example")], envelope_to: [] });
    expect(await thread(first)).toHaveLength(3);
  });

  test("the user's own addresses do not count as shared participants", async () => {
    const { box, mail, thread } = await conversation();
    const own = await mail("Notes", { from: [addr(box.address("me"))], to: [addr(box.address())] });
    await mail("Re: Notes", { from: [addr(box.address("me"))], to: [addr(box.address())] });
    expect(await thread(own)).toEqual(["Notes"]);
    // For a user, an address on the bare domain (admin's) is somebody else.
    const viaAdmin = await mail("Minutes", { from: [addr("x@mail.test")] });
    await mail("Re: Minutes", { from: [addr("x@mail.test")] });
    expect(await thread(viaAdmin)).toEqual(["Minutes", "Re: Minutes"]);
  });

  test("for admin the addresses of both its domains are own; users' addresses are participants", async () => {
    const admin = await adminMailbox(server);
    const pair = async (subject: string, from: string) => {
      const first = await admin.mail({ subject, from: [addr(from)], date: "2026-01-01T00:00:00Z" });
      await admin.mail({ subject: `Re: ${subject}`, from: [addr(from)], date: "2026-01-02T00:00:00Z" });
      const { body } = await admin.client.get(`/api/mails/thread/${first.mail_id}`);
      return body.body.map((h: { subject: string }) => h.subject);
    };
    expect(await pair("Own domain", "y@mail.test")).toEqual(["Own domain"]);
    expect(await pair("Admin subdomain", "y@admin.mail.test")).toEqual(["Admin subdomain"]);
    expect(await pair("User subdomain", "y@someone.mail.test")).toEqual(["User subdomain", "Re: User subdomain"]);
  });

  test("a subject shorter than three characters after stripping never matches", async () => {
    const { mail, thread } = await conversation();
    const two = await mail("Hi", { from: pm });
    await mail("Re: Hi", { from: pm });
    expect(await thread(two)).toEqual(["Hi"]);
    const three = await mail("Hey", { from: pm });
    await mail("Re: Hey", { from: pm });
    expect(await thread(three)).toEqual(["Hey", "Re: Hey"]);
    const empty = await mail("", { from: pm });
    await mail("Re: ", { from: pm });
    expect(await thread(empty)).toEqual([""]);
  });

  test("mail that carries thread headers is never matched by subject", async () => {
    const { mail, thread } = await conversation();
    const plain = await mail("Budget", { from: pm });
    const withHeader = await mail("Re: Budget", { from: pm, in_reply_to: "<elsewhere@x>" });
    const withRefs = await mail("Re: Budget", { from: pm, reference_ids: ["<elsewhere2@x>"] });
    expect(await thread(plain)).toEqual(["Budget"]);
    expect(await thread(withHeader)).toEqual(["Re: Budget"]);
    expect(await thread(withRefs)).toEqual(["Re: Budget"]);
  });

  test("leaves out spam, drafts and expunged mail; a spam seed pulls in spam", async () => {
    const { mail, thread } = await conversation();
    const first = await mail("Offer", { from: pm });
    const spam = await mail("Re: Offer", { from: pm, is_spam: true });
    await mail("Re: Offer", { from: pm, draft: true });
    await mail("Re: Offer", { from: pm, expunged: true });
    expect(await thread(first)).toEqual(["Offer"]);
    expect(await thread(spam)).toEqual(["Offer", "Re: Offer [spam]"]);
  });

  test("a header thread that holds a header-less mail also pulls that subject's header-less mails", async () => {
    const { mail, thread } = await conversation();
    const root = await mail("Budget", { message_id: "<b1@x>", from: pm });
    const reply = await mail("Re: Budget", { message_id: "<b2@x>", in_reply_to: "<b1@x>", from: pm });
    const loose = await mail("Re: Budget", { message_id: "<b3@x>", from: pm });
    for (const seed of [root, reply, loose]) expect(await thread(seed)).toEqual(["Budget", "Re: Budget", "Re: Budget"]);
  });

  test("the subject lookup uses the opened mail's subject only", async () => {
    const { mail, thread } = await conversation();
    // The opened reply was retitled; the header-less root it answers has another subject.
    await mail("Original topic", { message_id: "<o1@x>", from: pm });
    const retitled = await mail("New title", { message_id: "<o2@x>", in_reply_to: "<o1@x>", from: pm });
    await mail("Re: Original topic", { message_id: "<o3@x>", from: pm });
    expect(await thread(retitled)).toEqual(["Original topic", "New title"]);
  });

  // NOTE: "header-less" means two different things. The row filter of the
  // subject lookup asks for `reference_ids IS NULL`, while the matching rule
  // treats an empty list like no header. A mail stored with `[]` therefore
  // finds its subject siblings when it is the one opened, but is never found
  // when a sibling is opened: the same conversation, two different answers.
  test("a mail stored with an empty References list finds its siblings but is not found from them", async () => {
    const { mail, thread } = await conversation();
    const plain = await mail("Roadmap", { from: pm });
    const emptyRefs = await mail("Re: Roadmap", { from: pm, reference_ids: [] });
    expect(await thread(emptyRefs)).toEqual(["Roadmap", "Re: Roadmap"]);
    expect(await thread(plain)).toEqual(["Roadmap"]);
  });
});
