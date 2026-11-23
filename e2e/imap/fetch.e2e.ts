import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createUser, type TestServer, type TestUser } from "../harness";
import { ImapClient, openSession, utf8 } from "../harness/imap";
import { FIXTURE_NAMES, type FixtureName } from "./messages";
import { served } from "./served";
import {
  deliver,
  deliverFixture,
  mailDomain,
  sendOverHttp,
  startImapServer,
  type DeliveredMail,
} from "./support";

let server: TestServer;
/** Holds every fixture in INBOX, in `FIXTURE_NAMES` order; nothing here changes its flags. */
let fetcher: TestUser;
let domain: string;
let imap: ImapClient;
const rows = {} as Record<FixtureName, DeliveredMail>;
const seqOf = (name: FixtureName) => FIXTURE_NAMES.indexOf(name) + 1;

const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/** INTERNALDATE as a server in UTC writes it: the day space-padded, the rest zero-padded. */
const internalDate = (date: Date): string => {
  const two = (n: number) => String(n).padStart(2, "0");
  return (
    `${String(date.getUTCDate()).padStart(2, " ")}-${MONTHS[date.getUTCMonth()]}-${date.getUTCFullYear()} ` +
    `${two(date.getUTCHours())}:${two(date.getUTCMinutes())}:${two(date.getUTCSeconds())} +0000`
  );
};

beforeAll(async () => {
  server = await startImapServer();
  // A seven-letter name: the fixture sizes in `served.ts` count the address.
  fetcher = await createUser(server, "fetcher");
  domain = mailDomain(server, fetcher.username);
  for (const name of FIXTURE_NAMES) rows[name] = await deliverFixture(server, name, fetcher.username);
  imap = await openSession(server, fetcher.username, fetcher.password, "INBOX");
}, 120_000);

afterAll(async () => {
  imap?.close();
  await server?.stop();
});

describe("FETCH: what is served for each kind of message", () => {
  for (const name of FIXTURE_NAMES) {
    test(`${name}: UID, FLAGS, INTERNALDATE, RFC822.SIZE, ENVELOPE, BODYSTRUCTURE and BODY`, async () => {
      const expected = served(name, domain);
      const seq = seqOf(name);
      const stored = new Date(rows[name].date);
      const date = stored.toUTCString();
      const result = await imap.command(
        `FETCH ${seq} (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE BODYSTRUCTURE BODY)`
      );
      expect(result.lines).toEqual([
        `* ${seq} FETCH (UID ${seq} FLAGS () ` +
          `INTERNALDATE "${expected.internalDate ?? internalDate(stored)}" ` +
          `RFC822.SIZE ${expected.size} ` +
          `ENVELOPE ${expected.envelope.replace("DATE", date)} ` +
          `BODYSTRUCTURE ${expected.bodyStructure} ` +
          `BODY ${expected.body})`,
        `${result.tag} OK FETCH completed`,
      ]);
    });
  }

  test("a plain text message, written out in full", async () => {
    // The stored message has its text as the HTML body too, so even a plain
    // text delivery is served as multipart/alternative.
    const result = await imap.command("FETCH 1 (INTERNALDATE RFC822.SIZE ENVELOPE BODYSTRUCTURE)");
    expect(result.untagged).toEqual([
      '* 1 FETCH (INTERNALDATE " 5-Jan-2026 09:15:30 +0000" RFC822.SIZE 826 ' +
        'ENVELOPE ("Mon, 05 Jan 2026 09:15:30 GMT" "Plain text message" ' +
        '(("Carol Sender" NIL "carol" "remote.example")) ' +
        '(("Carol Sender" NIL "carol" "remote.example")) ' +
        '(("Replies" NIL "replies" "remote.example")) ' +
        '(("Hello There" NIL "hello" "fetcher.mail.test")) ' +
        '(("Dave" NIL "dave" "remote.example")) NIL NIL "<plain@fixtures.example>") ' +
        'BODYSTRUCTURE (("TEXT" "PLAIN" ("CHARSET" "UTF-8") NIL NIL "BASE64" 64 1 NIL NIL NIL NIL)' +
        '("TEXT" "HTML" ("CHARSET" "UTF-8") NIL NIL "BASE64" 64 1 NIL NIL NIL NIL) "alternative" NIL NIL NIL NIL))',
    ]);
  });

  test("a message with text attachments, written out in full", async () => {
    // Text parts are named in upper case, attachment types as stored; a text/*
    // attachment carries a line count, any other does not.
    const result = await imap.command(`FETCH ${seqOf("attachmentQp")} (BODYSTRUCTURE BODY)`);
    expect(result.untagged).toEqual([
      `* ${seqOf("attachmentQp")} FETCH (BODYSTRUCTURE (` +
        '(("TEXT" "PLAIN" ("CHARSET" "UTF-8") NIL NIL "BASE64" 60 1 NIL NIL NIL NIL)' +
        '("TEXT" "HTML" ("CHARSET" "UTF-8") NIL NIL "BASE64" 60 1 NIL NIL NIL NIL) "alternative" NIL NIL NIL NIL)' +
        '("text" "csv" ("NAME" "people.csv") NIL NIL "BASE64" 164 1 NIL ("ATTACHMENT" ("FILENAME" "people.csv")) NIL NIL)' +
        '("text" "plain" ("NAME" "notes.txt") NIL NIL "BASE64" 36 1 NIL ("ATTACHMENT" ("FILENAME" "notes.txt")) NIL NIL)' +
        ' "mixed" NIL NIL NIL NIL) ' +
        "BODY (" +
        '(("TEXT" "PLAIN" ("CHARSET" "UTF-8") NIL NIL "BASE64" 60 1)' +
        '("TEXT" "HTML" ("CHARSET" "UTF-8") NIL NIL "BASE64" 60 1) "alternative")' +
        '("text" "csv" ("NAME" "people.csv") NIL NIL "BASE64" 164 1)' +
        '("text" "plain" ("NAME" "notes.txt") NIL NIL "BASE64" 36 1)' +
        ' "mixed"))',
    ]);
  });

  test("RFC 2047 headers are served decoded, as raw UTF-8 in quoted strings", async () => {
    const result = await imap.command(`FETCH ${seqOf("encodedHeaders")} ENVELOPE`);
    expect(result.untagged).toEqual([
      `* ${seqOf("encodedHeaders")} FETCH (ENVELOPE ("Mon, 12 Jan 2026 16:00:00 GMT" ` +
        `"${utf8("Grüße aus Köln ✉ and more")}" ` +
        `(("${utf8("Jörg Müller")}" NIL "joerg" "remote.example")) ` +
        `(("${utf8("Jörg Müller")}" NIL "joerg" "remote.example")) NIL ` +
        `(("${utf8("Zoë 日本")}" NIL "hello" "fetcher.mail.test")) NIL NIL NIL "<encodedHeaders@fixtures.example>"))`,
    ]);
  });

  test("the envelope date is the Date header in GMT; INTERNALDATE is the same instant, not the arrival time", async () => {
    // Sent with "+0100" and "-0500" zones.
    const result = await imap.command("FETCH 2:3 (INTERNALDATE ENVELOPE)");
    expect(result.untagged.map((line) => /INTERNALDATE "([^"]+)" ENVELOPE \("([^"]+)"/.exec(line)!.slice(1))).toEqual([
      [" 6-Jan-2026 09:00:00 +0000", "Tue, 06 Jan 2026 09:00:00 GMT"],
      [" 7-Jan-2026 16:30:00 +0000", "Wed, 07 Jan 2026 16:30:00 GMT"],
    ]);
  });

  test("a message without a Date header is dated by its arrival", async () => {
    const seq = seqOf("missingHeaders");
    const stored = new Date(rows.missingHeaders.date);
    expect(Math.abs(stored.getTime() - Date.now())).toBeLessThan(10 * 60_000);
    const result = await imap.command(`FETCH ${seq} (INTERNALDATE)`);
    expect(result.untagged).toEqual([`* ${seq} FETCH (INTERNALDATE "${internalDate(stored)}")`]);
  });

  test("RFC822.SIZE is stored on the row once it has been served", async () => {
    const [row] = await server.db.query("SELECT rfc822_size FROM mails WHERE mail_id = $1", [rows.alternative.mail_id]);
    expect(Number(row!.rfc822_size)).toBe(689);
  });
});

describe("FETCH: macros", () => {
  const flags = "FLAGS ()";
  const date = 'INTERNALDATE " 7-Jan-2026 16:30:00 +0000"';
  const size = "RFC822.SIZE 689";

  test("FAST is FLAGS INTERNALDATE RFC822.SIZE", async () => {
    const result = await imap.command("FETCH 3 FAST");
    expect(result.lines).toEqual([`* 3 FETCH (${flags} ${date} ${size})`, `${result.tag} OK FETCH completed`]);
  });

  test("ALL adds ENVELOPE", async () => {
    const result = await imap.command("FETCH 3 ALL");
    expect(result.untagged).toEqual([
      `* 3 FETCH (${flags} ${date} ${size} ENVELOPE ${served("alternative", domain).envelope})`,
    ]);
  });

  test("FULL adds ENVELOPE and BODY (the structure, not the content)", async () => {
    const expected = served("alternative", domain);
    const result = await imap.command("FETCH 3 FULL");
    expect(result.untagged).toEqual([
      `* 3 FETCH (${flags} ${date} ${size} ENVELOPE ${expected.envelope} BODY ${expected.body})`,
    ]);
  });

  test("a macro name is case-insensitive, and is not a macro inside a list", async () => {
    expect((await imap.command("FETCH 3 fast")).untagged).toEqual([`* 3 FETCH (${flags} ${date} ${size})`]);
    const inList = await imap.command("FETCH 3 (FAST)");
    expect(inList.lines).toEqual([`${inList.tag} BAD Invalid data items in FETCH`]);
  });
});

describe("FETCH: data item syntax", () => {
  const fetch = async (args: string) => (await imap.command(`FETCH ${args}`)).lines.map((line) =>
    line.replace(/^A\d+ /, "")
  );
  const done = "OK FETCH completed";

  test("a single item needs no parentheses; anything after it is ignored", async () => {
    expect(await fetch("2 UID")).toEqual(["* 2 FETCH (UID 2)", done]);
    expect(await fetch("2 UID FLAGS")).toEqual(["* 2 FETCH (UID 2)", done]);
  });

  test("item names are case-insensitive and answered in upper case, in the order asked", async () => {
    expect(await fetch("2 (uid flags)")).toEqual(["* 2 FETCH (UID 2 FLAGS ())", done]);
    expect(await fetch("2 (Flags Uid Rfc822.Size)")).toEqual(["* 2 FETCH (FLAGS () UID 2 RFC822.SIZE 744)", done]);
  });

  test("a repeated item is answered each time", async () => {
    expect(await fetch("2 (UID UID)")).toEqual(["* 2 FETCH (UID 2 UID 2)", done]);
    expect(await fetch("2 (FLAGS FLAGS)")).toEqual(["* 2 FETCH (FLAGS () FLAGS ())", done]);
  });

  test("an empty list is answered with an empty list; a missing closing parenthesis is tolerated", async () => {
    expect(await fetch("2 ()")).toEqual(["* 2 FETCH ()", done]);
    expect(await fetch("2 (UID FLAGS")).toEqual(["* 2 FETCH (UID 2 FLAGS ())", done]);
  });

  test("an unknown item, or one unknown item in a list, is BAD", async () => {
    const bad = "BAD Invalid data items in FETCH";
    expect(await fetch("2 BOGUS")).toEqual([bad]);
    expect(await fetch("2 (UID BOGUS)")).toEqual([bad]);
    expect(await fetch("2 (RFC822.BOGUS)")).toEqual([bad]);
    expect(await fetch("2 (BODY[BOGUS])")).toEqual([bad]);
    expect(await fetch("2 (BODY[MIME])")).toEqual([bad]);
    expect(await fetch("2 (BODYSTRUCTURE[1])")).toEqual([bad]);
  });

  test("MODSEQ may be asked for explicitly, without enabling CONDSTORE", async () => {
    const modseq = Number(rows.html.modseq);
    expect(await fetch("2 (UID MODSEQ)")).toEqual([`* 2 FETCH (UID 2 MODSEQ (${modseq}))`, done]);
    // It is always written last, wherever it was asked.
    expect(await fetch("2 (MODSEQ UID)")).toEqual([`* 2 FETCH (UID 2 MODSEQ (${modseq}))`, done]);
  });
});

describe("FETCH: sequence sets", () => {
  const total = FIXTURE_NAMES.length;
  /** The sequence numbers answered, in the order they came. */
  const fetched = async (set: string, command = "FETCH") => {
    const result = await imap.command(`${command} ${set} (UID)`);
    return {
      status: result.tagged!.replace(/^A\d+ /, ""),
      seqs: result.untagged.map((line) => Number(/^\* (\d+) FETCH \(UID (\d+)\)$/.exec(line)![1])),
    };
  };
  const ok = "OK FETCH completed";
  const all = Array.from({ length: total }, (_, i) => i + 1);

  test("1:* is every message, in order", async () => {
    expect(await fetched("1:*")).toEqual({ status: ok, seqs: all });
  });

  test("* is the last message, alone or as either end of a range", async () => {
    expect(await fetched("*")).toEqual({ status: ok, seqs: [total] });
    expect(await fetched(`${total}:*`)).toEqual({ status: ok, seqs: [total] });
    expect(await fetched(`*:${total - 1}`)).toEqual({ status: ok, seqs: [total - 1, total] });
    expect(await fetched("*:*")).toEqual({ status: ok, seqs: [total] });
  });

  test("a range may be written backwards", async () => {
    expect(await fetched("3:1")).toEqual({ status: ok, seqs: [1, 2, 3] });
    expect(await fetched("*:1")).toEqual({ status: ok, seqs: all });
  });

  test("a range reaching past the end is cut at the end; one starting past it matches nothing", async () => {
    expect(await fetched(`${total - 1}:${total + 7}`)).toEqual({ status: ok, seqs: [total - 1, total] });
    expect(await fetched(`${total + 1}`)).toEqual({ status: ok, seqs: [] });
    expect(await fetched(`${total + 1}:${total + 5}`)).toEqual({ status: ok, seqs: [] });
    expect(await fetched("4294967296")).toEqual({ status: ok, seqs: [] });
  });

  test("a range from past the end to * is not the last message", async () => {
    // NOTE: RFC 3501 has n:* always include the last message, since the
    // range is unordered. Here a start beyond the mailbox matches nothing.
    expect(await fetched(`${total + 1}:*`)).toEqual({ status: ok, seqs: [] });
  });

  test("0 matches nothing and is not an error", async () => {
    expect(await fetched("0")).toEqual({ status: ok, seqs: [] });
  });

  test("a list names several messages; a message named twice is answered once", async () => {
    const list = await fetched("2,4,9");
    expect({ ...list, seqs: [...list.seqs].sort((a, b) => a - b) }).toEqual({ status: ok, seqs: [2, 4, 9] });
    const repeated = await fetched("2,4,2");
    expect([...repeated.seqs].sort((a, b) => a - b)).toEqual([2, 4]);
    const mixed = await fetched("5,1:2,9:*");
    expect([...mixed.seqs].sort((a, b) => a - b)).toEqual([1, 2, 5, ...all.slice(8)]);
  });

  test("a space after a comma is tolerated; other malformed sets are BAD", async () => {
    expect(await fetched("1, 2")).toMatchObject({ status: ok });
    const bad = "BAD Invalid data items in FETCH";
    expect((await fetched("1,,2")).status).toBe(bad);
    expect((await fetched("1:2:3")).status).toBe(bad);
    expect((await fetched("1 2")).status).toBe(bad);
    expect((await fetched("1:")).status).toBe("BAD Invalid sequence set in FETCH");
    expect((await fetched("abc")).status).toBe("BAD Invalid sequence set in FETCH");
    expect((await fetched("-1")).status).toBe("BAD Invalid sequence set in FETCH");
  });
});

describe("UID FETCH", () => {
  let user: TestUser;
  let own: ImapClient;

  beforeAll(async () => {
    // Five messages, of which UIDs 2 and 4 are then expunged: UIDs 1, 3, 5 at sequence 1, 2, 3.
    user = await createUser(server, "uidfetch");
    for (let i = 0; i < 5; i++) await deliver(server, { to: `hello@${mailDomain(server, user.username)}` });
    own = await openSession(server, user.username, user.password, "INBOX");
    await own.command("STORE 2,4 +FLAGS.SILENT (\\Deleted)");
    await own.command("EXPUNGE");
  });

  afterAll(() => own?.close());

  const fetch = async (args: string) => (await own.command(`UID FETCH ${args}`)).lines.map((line) =>
    line.replace(/^A\d+ /, "")
  );
  const done = "OK FETCH completed";

  test("addresses messages by UID and always answers UID first, once", async () => {
    expect(await fetch("3 FLAGS")).toEqual(["* 2 FETCH (UID 3 FLAGS ())", done]);
    expect(await fetch("3 (FLAGS UID)")).toEqual(["* 2 FETCH (UID 3 FLAGS ())", done]);
    expect(await fetch("3 (UID FLAGS UID)")).toEqual(["* 2 FETCH (UID 3 FLAGS ())", done]);
    expect(await fetch("3 (UID)")).toEqual(["* 2 FETCH (UID 3)", done]);
  });

  test("a UID range covers the UIDs that exist in it", async () => {
    expect(await fetch("1:5 (UID)")).toEqual(["* 1 FETCH (UID 1)", "* 2 FETCH (UID 3)", "* 3 FETCH (UID 5)", done]);
    expect(await fetch("2:4 (UID)")).toEqual(["* 2 FETCH (UID 3)", done]);
    expect(await fetch("4:2 (UID)")).toEqual(["* 2 FETCH (UID 3)", done]);
    expect(await fetch("1:* (UID)")).toEqual(["* 1 FETCH (UID 1)", "* 2 FETCH (UID 3)", "* 3 FETCH (UID 5)", done]);
  });

  test("an expunged or never-assigned UID matches nothing and is not an error", async () => {
    expect(await fetch("2 (UID)")).toEqual([done]);
    expect(await fetch("2,4 (UID)")).toEqual([done]);
    expect(await fetch("600 (UID)")).toEqual([done]);
    expect(await fetch("0 (UID)")).toEqual([done]);
  });

  test("* is the highest UID in the mailbox", async () => {
    expect(await fetch("* (UID)")).toEqual(["* 3 FETCH (UID 5)", done]);
    expect(await fetch("5:* (UID)")).toEqual(["* 3 FETCH (UID 5)", done]);
    expect(await fetch("*:3 (UID)")).toEqual(["* 2 FETCH (UID 3)", "* 3 FETCH (UID 5)", done]);
  });

  test("a range from past the highest UID to * matches nothing", async () => {
    // NOTE: RFC 3501 6.4.8 reads n:* as the unordered range between n and the
    // highest UID, so it always includes the last message (clients poll with
    // UID FETCH <uidnext>:*). Here it is read as "n and above".
    expect(await fetch("600:* (UID)")).toEqual([done]);
    expect(await fetch("6:* (UID)")).toEqual([done]);
    expect(await fetch("*:6 (UID)")).toEqual([done]);
  });

  test("sequence-number FETCH on the same mailbox counts positions, not UIDs", async () => {
    const result = await own.command("FETCH 1:3 (UID)");
    expect(result.untagged).toEqual(["* 1 FETCH (UID 1)", "* 2 FETCH (UID 3)", "* 3 FETCH (UID 5)"]);
    expect((await own.command("FETCH 5 (UID)")).untagged).toEqual([]);
  });
});

describe("FETCH: how many messages one command may return", () => {
  let bulk: ImapClient;

  beforeAll(async () => {
    // 520 messages written straight into the table: text only, UIDs 1..520.
    const user = await createUser(server, "bulkbox");
    const address = `hello@${mailDomain(server, user.username)}`;
    await server.db.query(
      `INSERT INTO mails (user_id, message_id, subject, text, html, date, from_address, from_text,
                          to_address, to_text, envelope_to, uid_domain)
       SELECT $1, '<bulk-' || n || '@fixtures.example>', 'Bulk ' || n, 'Body ' || n, '',
              TIMESTAMPTZ '2026-02-01 00:00:00+00' + n * INTERVAL '1 minute',
              '[{"address":"sender@remote.example","name":"Remote Sender"}]'::jsonb,
              'Remote Sender <sender@remote.example>', $2::jsonb, $3, $2::jsonb, n
       FROM generate_series(1, 520) AS n`,
      [user.id, [{ address }], address]
    );
    bulk = await openSession(server, user.username, user.password, "INBOX");
  });

  afterAll(() => bulk?.close());

  const counted = async (command: string) => {
    const result = await bulk.command(command);
    const seqs = result.untagged.map((line) => Number(/^\* (\d+) FETCH/.exec(line)![1]));
    return {
      status: result.tagged!.replace(/^A\d+ /, ""),
      count: seqs.length,
      lowest: Math.min(...seqs),
      highest: Math.max(...seqs),
    };
  };
  const answered = (count: number, lowest: number, highest: number) => ({
    status: "OK FETCH completed",
    count,
    lowest,
    highest,
  });

  test("flags, UID, dates, sizes and MODSEQ are returned for every message asked", async () => {
    expect(await counted("FETCH 1:* (FLAGS)")).toEqual(answered(520, 1, 520));
    expect(await counted("FETCH 1:* (UID FLAGS INTERNALDATE RFC822.SIZE MODSEQ)")).toEqual(answered(520, 1, 520));
    expect(await counted("FETCH 1:* FAST")).toEqual(answered(520, 1, 520));
    expect(await counted("UID FETCH 1:* (FLAGS)")).toEqual(answered(520, 1, 520));
  });

  test("BODY[HEADER] and BODY[HEADER.FIELDS] stop after the first 500 messages, still completing OK", async () => {
    expect(await counted("FETCH 1:* (BODY.PEEK[HEADER])")).toEqual(answered(500, 1, 500));
    expect(await counted("FETCH 1:* (FLAGS BODY.PEEK[HEADER.FIELDS (SUBJECT)])")).toEqual(answered(500, 1, 500));
    expect(await counted("FETCH 1:501 (BODY.PEEK[HEADER])")).toEqual(answered(500, 1, 500));
    expect(await counted("UID FETCH *:1 (BODY.PEEK[HEADER])")).toEqual(answered(500, 1, 500));
    // A request within the limit is served whole, wherever it starts.
    expect(await counted("UID FETCH 300:* (BODY.PEEK[HEADER])")).toEqual(answered(221, 300, 520));
  });

  test("every other item stops after the first 50 messages", async () => {
    for (const items of [
      "(ENVELOPE)",
      "(BODYSTRUCTURE)",
      "(BODY)",
      "(BODY.PEEK[])",
      "(BODY.PEEK[TEXT])",
      "(BODY.PEEK[1])",
      "(RFC822.HEADER)",
      "(FLAGS ENVELOPE)",
      "ALL",
      "FULL",
    ]) {
      expect({ items, ...(await counted(`FETCH 1:* ${items}`)) }).toEqual({ items, ...answered(50, 1, 50) });
    }
    expect(await counted("UID FETCH 1:51 (BODY.PEEK[])")).toEqual(answered(50, 1, 50));
    expect(await counted("UID FETCH 1:50 (BODY.PEEK[])")).toEqual(answered(50, 1, 50));
  });

  test("the first 50 are counted in the order the set names them", async () => {
    expect(await counted("FETCH 100:* (ENVELOPE)")).toEqual(answered(50, 100, 149));
    expect(await counted("FETCH 500:400 (ENVELOPE)")).toEqual(answered(50, 400, 449));
    expect(await counted("FETCH 1:10,400:* (ENVELOPE)")).toEqual(answered(50, 1, 439));
    expect(await counted("FETCH 510,1:60 (ENVELOPE)")).toEqual(answered(50, 1, 510));
  });

  test("the client continues from where the answer stopped", async () => {
    expect(await counted("UID FETCH 51:* (ENVELOPE)")).toEqual(answered(50, 51, 100));
    expect(await counted("UID FETCH 501:* (ENVELOPE)")).toEqual(answered(20, 501, 520));
  });
});

describe("FETCH: the implicit \\Seen flag", () => {
  let user: TestUser;
  let own: ImapClient;
  let mails: DeliveredMail[];

  beforeAll(async () => {
    user = await createUser(server, "seenbox");
    mails = [];
    for (let i = 0; i < 12; i++) {
      mails.push(await deliver(server, { to: `hello@${mailDomain(server, user.username)}`, text: `Body ${i}` }));
    }
    own = await openSession(server, user.username, user.password, "INBOX");
  });

  afterAll(() => own?.close());

  const flagsOf = async (seq: number) => (await own.command(`FETCH ${seq} FLAGS`)).untagged;
  const readColumn = async (mail: DeliveredMail) =>
    (await server.db.query("SELECT read FROM mails WHERE mail_id = $1", [mail.mail_id]))[0]!.read;

  test("items that only describe a message leave it unread", async () => {
    await own.command(
      "FETCH 1 (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE BODYSTRUCTURE BODY MODSEQ RFC822.HEADER)"
    );
    await own.command("FETCH 1 FULL");
    expect(await flagsOf(1)).toEqual(["* 1 FETCH (FLAGS ())"]);
    expect(await readColumn(mails[0]!)).toBe(false);
  });

  test("every BODY.PEEK[...] form leaves it unread", async () => {
    await own.command(
      "FETCH 2 (BODY.PEEK[] BODY.PEEK[HEADER] BODY.PEEK[TEXT] BODY.PEEK[1] BODY.PEEK[1.MIME] " +
        "BODY.PEEK[HEADER.FIELDS (SUBJECT)] BODY.PEEK[]<0.10>)"
    );
    expect(await flagsOf(2)).toEqual(["* 2 FETCH (FLAGS ())"]);
    expect(await readColumn(mails[1]!)).toBe(false);
  });

  const marking: [string, string][] = [
    ["BODY[]", "BODY[]"],
    ["BODY[TEXT]", "BODY[TEXT]"],
    ["BODY[HEADER]", "BODY[HEADER]"],
    ["BODY[HEADER.FIELDS (DATE)]", "BODY[HEADER.FIELDS (DATE)]"],
    ["BODY[1]", "BODY[1]"],
    ["BODY[1.MIME]", "BODY[1.MIME]"],
    ["a partial BODY[]<0.10>", "BODY[]<0.10>"],
    ["RFC822", "RFC822"],
    ["RFC822.TEXT", "RFC822.TEXT"],
  ];

  test("each non-PEEK body item marks the message read, in the table too", async () => {
    for (const [index, [label, item]] of marking.entries()) {
      const seq = 3 + index;
      expect({ label, before: await flagsOf(seq) }).toEqual({ label, before: [`* ${seq} FETCH (FLAGS ())`] });
      expect((await own.command(`FETCH ${seq} (${item})`)).status).toBe("OK");
      expect({ label, after: await flagsOf(seq) }).toEqual({ label, after: [`* ${seq} FETCH (FLAGS (\\Seen))`] });
      expect({ label, read: await readColumn(mails[seq - 1]!) }).toEqual({ label, read: true });
    }
  });

  test("the FETCH that sets \\Seen reports the flags as they were, and sends no update", async () => {
    // NOTE: RFC 3501 6.4.5 says the flag change should be reported with the
    // fetch. FLAGS in the same response is read before the flag is set.
    const result = await own.command("FETCH 12 (FLAGS BODY[TEXT])");
    expect(result.untagged).toHaveLength(1);
    expect(result.untagged[0]).toStartWith("* 12 FETCH (FLAGS () BODY[TEXT] {");
    expect(await flagsOf(12)).toEqual(["* 12 FETCH (FLAGS (\\Seen))"]);
  });

  test("a body item for a part that does not exist still marks the message read", async () => {
    const extra = await deliver(server, { to: `hello@${mailDomain(server, user.username)}` });
    await own.command("SELECT INBOX");
    const result = await own.command("FETCH 13 (BODY[7])");
    expect(result.untagged).toEqual(["* 13 FETCH ()"]);
    expect(await readColumn(extra)).toBe(true);
  });

  test("RFC822.HEADER does not mark the message read, though BODY[HEADER] does", async () => {
    const extra = await deliver(server, { to: `hello@${mailDomain(server, user.username)}` });
    await own.command("SELECT INBOX");
    await own.command("FETCH 14 (RFC822.HEADER)");
    expect(await readColumn(extra)).toBe(false);
  });
});

describe("FETCH: each kind of mailbox", () => {
  let user: TestUser;
  let own: ImapClient;

  beforeAll(async () => {
    user = await createUser(server, "boxkind");
    const d = mailDomain(server, user.username);
    await deliver(server, { to: `hello@${d}`, subject: "to hello 1" });
    await deliver(server, { to: `work@${d}`, subject: "to work" });
    await deliver(server, { to: `hello@${d}`, subject: "to hello 2" });
    await sendOverHttp(server, user, { sender: "me", to: "x@remote.example", subject: "sent one", html: "<p>s</p>" });
    own = await openSession(server, user.username, user.password);
  });

  afterAll(() => own?.close());

  const subjects = async (mailbox: string) => {
    await own.command(`SELECT ${mailbox}`);
    const result = await own.command("FETCH 1:* (UID BODY.PEEK[HEADER.FIELDS (SUBJECT)])");
    return result.untagged.map((line) => {
      const match = /^\* (\d+) FETCH \(UID (\d+) .*Subject: ([^\r]*)\r\n/s.exec(line)!;
      return `${match[1]}/${match[2]} ${match[3]}`;
    });
  };

  test("INBOX numbers every received message in arrival order", async () => {
    expect(await subjects("INBOX")).toEqual(["1/1 to hello 1", "2/2 to work", "3/3 to hello 2"]);
  });

  test("a per-address mailbox holds that address's messages under its own UIDs", async () => {
    expect(await subjects('"INBOX/accounts/hello"')).toEqual(["1/1 to hello 1", "2/2 to hello 2"]);
    expect(await subjects('"INBOX/accounts/work"')).toEqual(["1/1 to work"]);
  });

  test("the sent mailboxes hold sent mail, flagged \\Seen", async () => {
    expect(await subjects('"Sent Messages"')).toEqual(["1/1 sent one"]);
    expect(await subjects('"Sent Messages/accounts/me"')).toEqual(["1/1 sent one"]);
    const result = await own.command("FETCH 1 (FLAGS ENVELOPE)");
    expect(result.untagged[0]).toMatch(
      /^\* 1 FETCH \(FLAGS \(\\Seen\) ENVELOPE \("[^"]+" "sent one" \(\(NIL NIL "me" "boxkind\.mail\.test"\)\) \(\(NIL NIL "me" "boxkind\.mail\.test"\)\) \(\(NIL NIL "me" "boxkind\.mail\.test"\)\) \(\(NIL NIL "x" "remote\.example"\)\) NIL NIL NIL "<[^"]+>"\)\)$/
    );
  });

  test("an empty mailbox answers any FETCH with just OK", async () => {
    await own.command("SELECT Trash");
    for (const command of ["FETCH 1 FLAGS", "FETCH 1:* FLAGS", "FETCH * FLAGS", "UID FETCH 1:* FLAGS", "UID FETCH * FLAGS"]) {
      const result = await own.command(command);
      expect({ command, lines: result.lines }).toEqual({ command, lines: [`${result.tag} OK FETCH completed`] });
    }
  });
});
