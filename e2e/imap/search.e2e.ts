import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createUser, type TestServer, type TestUser } from "../harness";
import { ImapClient, lit, openSession, utf8 } from "../harness/imap";
import { FIXTURE_NAMES, type FixtureName } from "./messages";
import { deliverFixture, startImapServer } from "./support";

// Every fixture with a Date header of its own, in INBOX in this order, so a
// message's sequence number and UID are its position here. Flags are set once:
// plain and html \Seen, alternative \Flagged, attachmentBase64 \Answered,
// attachmentQp \Deleted.
const NAMES: FixtureName[] = FIXTURE_NAMES.filter((name) => name !== "missingHeaders" && name !== "large");
const EVERY = NAMES.map((_, index) => index + 1);

/** The sequence numbers (and UIDs) of the named fixtures, ascending. */
const of = (...names: FixtureName[]): number[] => names.map((name) => NAMES.indexOf(name) + 1).sort((a, b) => a - b);
const except = (...names: FixtureName[]): number[] => {
  const excluded = new Set(of(...names));
  return EVERY.filter((seq) => !excluded.has(seq));
};

let server: TestServer;
let user: TestUser;
let imap: ImapClient;

/** The numbers a SEARCH returned, or its tagged refusal. */
const search = async (keys: string, command = "SEARCH"): Promise<number[] | string> => {
  const result = await imap.command(`${command} ${keys}`);
  if (result.status !== "OK") return `${result.status} ${result.text}`;
  expect(result.tagged).toBe(`${result.tag} OK SEARCH completed`);
  expect(result.untagged).toHaveLength(1);
  const match = /^\* SEARCH ((?:\d+(?: |$))*)$/.exec(result.untagged[0]!);
  if (!match) throw new Error(`Unexpected SEARCH answer: ${result.untagged[0]}`);
  return match[1]!.split(" ").filter(Boolean).map(Number);
};

beforeAll(async () => {
  server = await startImapServer();
  user = await createUser(server, "srchusr");
  for (const name of NAMES) await deliverFixture(server, name, user.username);
  imap = await openSession(server, user.username, user.password, "INBOX");
  await imap.command(`STORE ${of("plain", "html").join(",")} +FLAGS.SILENT (\\Seen)`);
  await imap.command(`STORE ${of("alternative")[0]} +FLAGS.SILENT (\\Flagged)`);
  await imap.command(`STORE ${of("attachmentBase64")[0]} +FLAGS.SILENT (\\Answered)`);
  await imap.command(`STORE ${of("attachmentQp")[0]} +FLAGS.SILENT (\\Deleted)`);
}, 120_000);

afterAll(async () => {
  imap?.close();
  await server?.stop();
});

describe("SEARCH: the answer", () => {
  test("lists matching sequence numbers in ascending order on one line", async () => {
    const result = await imap.command("SEARCH ALL");
    expect(result.lines).toEqual([`* SEARCH ${EVERY.join(" ")}`, `${result.tag} OK SEARCH completed`]);
  });

  test("an empty result is '* SEARCH ' with a trailing space", async () => {
    const result = await imap.command("SEARCH FROM nobody-at-all");
    expect(result.lines).toEqual(["* SEARCH ", `${result.tag} OK SEARCH completed`]);
  });

  test("UID SEARCH answers with UIDs under the same untagged name", async () => {
    const result = await imap.command("UID SEARCH SEEN");
    expect(result.lines).toEqual(["* SEARCH 1 2", `${result.tag} OK SEARCH completed`]);
  });

  test("with no key at all is BAD", async () => {
    expect(await search("")).toBe("BAD Search criteria is required");
    expect(await search("", "UID SEARCH")).toBe("BAD Search criteria is required");
  });

  test("key names are case-insensitive", async () => {
    expect(await search("all")).toEqual(EVERY);
    expect(await search("seen")).toEqual(of("plain", "html"));
    expect(await search("Unseen fRoM carol")).toEqual(except("plain", "html", "encodedHeaders", "addresses"));
  });
});

describe("SEARCH: flag keys", () => {
  test("SEEN / UNSEEN", async () => {
    expect(await search("SEEN")).toEqual(of("plain", "html"));
    expect(await search("UNSEEN")).toEqual(except("plain", "html"));
  });

  test("FLAGGED / UNFLAGGED", async () => {
    expect(await search("FLAGGED")).toEqual(of("alternative"));
    expect(await search("UNFLAGGED")).toEqual(except("alternative"));
  });

  test("ANSWERED / UNANSWERED", async () => {
    expect(await search("ANSWERED")).toEqual(of("attachmentBase64"));
    expect(await search("UNANSWERED")).toEqual(except("attachmentBase64"));
  });

  test("DELETED / UNDELETED", async () => {
    expect(await search("DELETED")).toEqual(of("attachmentQp"));
    expect(await search("UNDELETED")).toEqual(except("attachmentQp"));
  });

  test("DRAFT / UNDRAFT: no message in INBOX is ever a draft", async () => {
    expect(await search("DRAFT")).toEqual([]);
    expect(await search("UNDRAFT")).toEqual(EVERY);
  });

  test("NEW is UNSEEN; RECENT and OLD match every message (\\Recent is not tracked)", async () => {
    expect(await search("NEW")).toEqual(except("plain", "html"));
    expect(await search("RECENT")).toEqual(EVERY);
    expect(await search("OLD")).toEqual(EVERY);
  });

  test("KEYWORD matches nothing and UNKEYWORD everything: there are no keywords", async () => {
    expect(await search("KEYWORD $Forwarded")).toEqual([]);
    expect(await search("UNKEYWORD $Forwarded")).toEqual(EVERY);
    expect(await search("KEYWORD Junk")).toEqual([]);
    expect(await search("NOT KEYWORD x")).toEqual(EVERY);
  });

  test("KEYWORD needs an atom: a system flag or nothing is BAD", async () => {
    expect(await search("KEYWORD \\Seen")).toBe("BAD Invalid search criteria");
    expect(await search("KEYWORD")).toBe("BAD Invalid search criteria");
    expect(await search("UNKEYWORD")).toBe("BAD Invalid search criteria");
  });
});

describe("SEARCH: address and text keys", () => {
  test("FROM matches a substring of the From header, name or address, ignoring case", async () => {
    expect(await search("FROM carol")).toEqual(except("encodedHeaders", "addresses"));
    expect(await search("FROM CAROL")).toEqual(except("encodedHeaders", "addresses"));
    expect(await search('FROM "Carol Sender"')).toEqual(except("encodedHeaders", "addresses"));
    expect(await search("FROM joerg")).toEqual(of("encodedHeaders"));
    expect(await search("FROM remote.example")).toEqual(EVERY);
    expect(await search("FROM nobody")).toEqual([]);
  });

  test("TO, CC and BCC each match their own header", async () => {
    expect(await search("TO hello")).toEqual(EVERY);
    expect(await search("TO second")).toEqual(of("addresses"));
    expect(await search("TO Zo")).toEqual(of("encodedHeaders"));
    expect(await search("CC dave")).toEqual(of("plain"));
    expect(await search("CC Team")).toEqual(of("addresses"));
    expect(await search("CC two@remote.example")).toEqual(of("addresses"));
    expect(await search("BCC hidden")).toEqual(of("addresses"));
    expect(await search("BCC dave")).toEqual([]);
    expect(await search("TO dave")).toEqual([]);
  });

  test("SUBJECT matches a substring, ignoring case", async () => {
    expect(await search("SUBJECT message")).toEqual(of("plain", "html", "alternative", "reply"));
    expect(await search('SUBJECT "plain text"')).toEqual(of("plain", "reply"));
    expect(await search("SUBJECT PLAIN")).toEqual(of("plain", "reply"));
    expect(await search('SUBJECT "quoted"')).toEqual(of("attachmentQp", "quotedSubject"));
    expect(await search('SUBJECT "\\"quoted\\""')).toEqual(of("quotedSubject"));
    expect(await search('SUBJECT "{5}"')).toEqual(of("quotedSubject"));
    expect(await search('SUBJECT ""')).toEqual(EVERY);
  });

  test("the search string is a LIKE pattern: _ matches any one character and % any run", async () => {
    // NOTE: the string is not escaped before it is matched, so a client
    // searching for a literal underscore or percent sign gets wildcard matches.
    expect(await search('SUBJECT "Pl_in"')).toEqual(of("plain", "reply"));
    expect(await search('SUBJECT "Plain%message"')).toEqual(of("plain", "reply"));
    expect(await search('SUBJECT "100%"')).toEqual([]);
  });

  test("an unquoted % is a syntax error", async () => {
    expect(await search("SUBJECT %")).toBe("BAD Invalid search criteria");
  });

  test("BODY searches the text body only", async () => {
    expect(await search("BODY plain")).toEqual(of("plain", "alternative", "nested"));
    expect(await search("BODY attached")).toEqual(of("attachmentBase64"));
    expect(await search("BODY carol")).toEqual(of("plain"));
    // The text derived from an HTML-only message is searched ...
    expect(await search("BODY Title")).toEqual(of("html"));
    // ... but not the HTML part itself, nor attachments, nor headers.
    expect(await search('BODY "<i>HTML</i>"')).toEqual([]);
    expect(await search("BODY Second")).toEqual([]);
    expect(await search("BODY Alternative")).toEqual(of("alternative"));
  });

  test("TEXT searches subject, From, To and the text body, but not Cc or the Message-ID", async () => {
    expect(await search("TEXT carol")).toEqual(except("encodedHeaders", "addresses"));
    expect(await search("TEXT attached")).toEqual(of("attachmentBase64"));
    expect(await search("TEXT hello@")).toEqual(EVERY);
    expect(await search("TEXT nested")).toEqual(of("nested"));
    expect(await search("TEXT dave")).toEqual([]);
    expect(await search("TEXT fixtures.example")).toEqual([]);
  });

  test("non-ASCII strings are matched as UTF-8, with or without CHARSET", async () => {
    expect(await search(`SUBJECT "${utf8("Grüße")}"`)).toEqual(of("encodedHeaders"));
    expect(await search(`SUBJECT "${utf8("grüsse")}"`)).toEqual([]);
    expect(await search(`CHARSET UTF-8 SUBJECT "${utf8("Köln")}"`)).toEqual(of("encodedHeaders"));
    expect(await search(`BODY "${utf8("日本語")}"`)).toEqual(of("utf8Body"));
    expect(await search(`BODY "${utf8("München")}"`)).toEqual(of("latin1Body"));
    expect(await search(`TEXT "${utf8("Привет")}"`)).toEqual([]);
    expect(await search(`FROM "${utf8("Müller")}"`)).toEqual(of("encodedHeaders"));
  });

  test("CHARSET and its argument are ignored, whatever the charset", async () => {
    expect(await search("CHARSET US-ASCII SUBJECT plain")).toEqual(of("plain", "reply"));
    expect(await search("CHARSET BOGUS-CHARSET SUBJECT plain")).toEqual(of("plain", "reply"));
    expect(await search("CHARSET UTF-8 ALL")).toEqual(EVERY);
  });
});

describe("SEARCH: HEADER", () => {
  test("matches Subject, From, To and Message-ID, the field name in any case", async () => {
    expect(await search("HEADER Subject plain")).toEqual(of("plain", "reply"));
    expect(await search("HEADER SUBJECT plain")).toEqual(of("plain", "reply"));
    expect(await search("HEADER from joerg")).toEqual(of("encodedHeaders"));
    expect(await search("HEADER To second")).toEqual(of("addresses"));
    expect(await search("HEADER Message-ID plain@")).toEqual(of("plain"));
    expect(await search('HEADER Message-ID "<reply@fixtures.example>"')).toEqual(of("reply"));
    expect(await search('HEADER Subject ""')).toEqual(EVERY);
  });

  test("any other field matches nothing, even one the message has", async () => {
    // NOTE: RFC 3501 has HEADER match any header field. Cc, Date, Bcc,
    // Reply-To and arbitrary fields are not searchable here.
    expect(await search("HEADER Cc dave")).toEqual([]);
    expect(await search("HEADER Date 2026")).toEqual([]);
    expect(await search("HEADER Reply-To replies")).toEqual([]);
    expect(await search("HEADER X-Custom-Header custom")).toEqual([]);
    expect(await search('HEADER Cc ""')).toEqual([]);
    expect(await search("NOT HEADER Cc dave")).toEqual(EVERY);
  });

  test("needs a field name and a value", async () => {
    expect(await search("HEADER Subject")).toBe("BAD Invalid search criteria");
    expect(await search("HEADER")).toBe("BAD Invalid search criteria");
  });
});

describe("SEARCH: date keys", () => {
  test("BEFORE, ON and SINCE compare the message's Date header, by day in the server's zone", async () => {
    expect(await search("BEFORE 7-Jan-2026")).toEqual(of("plain", "html"));
    expect(await search("ON 7-Jan-2026")).toEqual(of("alternative"));
    expect(await search("ON 6-Jan-2026")).toEqual(of("html"));
    expect(await search("SINCE 20-Jan-2026")).toEqual(of("reply"));
    expect(await search("SINCE 21-Jan-2026")).toEqual([]);
    // 23:00 UTC on the 19th and 08:00 UTC on the 20th.
    expect(await search("ON 19-Jan-2026")).toEqual(of("quotedSubject"));
    expect(await search("ON 20-Jan-2026")).toEqual(of("reply"));
    expect(await search("BEFORE 5-Jan-2026")).toEqual([]);
    expect(await search("ON 1-Jan-2030")).toEqual([]);
    expect(await search("BEFORE 1-Jan-1970")).toEqual([]);
  });

  test("SENTBEFORE, SENTON and SENTSINCE compare the same date", async () => {
    expect(await search("SENTBEFORE 7-Jan-2026")).toEqual(of("plain", "html"));
    expect(await search("SENTON 7-Jan-2026")).toEqual(of("alternative"));
    expect(await search("SENTSINCE 20-Jan-2026")).toEqual(of("reply"));
  });

  test("date keys combine into a range", async () => {
    expect(await search("SINCE 1-jan-2026 BEFORE 8-JAN-2026")).toEqual(of("plain", "html", "alternative"));
    expect(await search("SINCE 17-Jan-2026 BEFORE 19-Jan-2026")).toEqual(of("emptyBody", "addresses"));
  });

  test("a zero-padded day and an ISO date are accepted", async () => {
    expect(await search("BEFORE 07-Jan-2026")).toEqual(of("plain", "html"));
    expect(await search("SINCE 2026-01-20")).toEqual(of("reply"));
  });

  test("a quoted date is BAD", async () => {
    // NOTE: RFC 3501 allows the date in double quotes.
    expect(await search('SINCE "20-Jan-2026"')).toBe("BAD Invalid search criteria");
  });

  test("a missing or unreadable date is BAD", async () => {
    expect(await search("SINCE")).toBe("BAD Invalid search criteria");
    expect(await search("SINCE 20-Foo-2026")).toBe("BAD Invalid search criteria");
    expect(await search("SINCE tomorrow")).toBe("BAD Invalid search criteria");
    expect(await search("ON")).toBe("BAD Invalid search criteria");
    expect(await search("SENTBEFORE x")).toBe("BAD Invalid search criteria");
  });
});

describe("SEARCH: size keys", () => {
  test("LARGER and SMALLER match nothing, whatever the size", async () => {
    // NOTE: looks like a bug: the size keys are not implemented and fail
    // closed, so SMALLER <huge> is empty and NOT LARGER <n> is everything.
    expect(await search("LARGER 1")).toEqual([]);
    expect(await search("LARGER 1000")).toEqual([]);
    expect(await search("SMALLER 100000000")).toEqual([]);
    expect(await search("NOT LARGER 1")).toEqual(EVERY);
    expect(await search("OR LARGER 1 SEEN")).toEqual(of("plain", "html"));
  });

  test("the size must be a number", async () => {
    expect(await search("LARGER")).toBe("BAD Invalid search criteria");
    expect(await search("LARGER abc")).toBe("BAD Invalid search criteria");
    expect(await search("SMALLER")).toBe("BAD Invalid search criteria");
  });
});

describe("SEARCH: combining keys", () => {
  test("several keys must all match", async () => {
    expect(await search("SEEN FLAGGED")).toEqual([]);
    expect(await search("SEEN UNSEEN")).toEqual([]);
    expect(await search("UNSEEN FROM carol SUBJECT message")).toEqual(of("alternative", "reply"));
    expect(await search("ALL ALL")).toEqual(EVERY);
  });

  test("NOT negates the one key after it", async () => {
    expect(await search("NOT SEEN")).toEqual(except("plain", "html"));
    expect(await search("NOT NOT SEEN")).toEqual(of("plain", "html"));
    expect(await search("NOT ALL")).toEqual([]);
    expect(await search("NOT FROM carol")).toEqual(of("encodedHeaders", "addresses"));
    expect(await search("NOT SEEN FROM joerg")).toEqual(of("encodedHeaders"));
  });

  test("OR takes the two keys after it, and nests", async () => {
    expect(await search("OR SEEN FLAGGED")).toEqual(of("plain", "html", "alternative"));
    expect(await search("OR SEEN OR FLAGGED ANSWERED")).toEqual(of("plain", "html", "alternative", "attachmentBase64"));
    expect(await search("OR ANSWERED OR FLAGGED DELETED")).toEqual(of("alternative", "attachmentBase64", "attachmentQp"));
    expect(await search("OR NOT SEEN FLAGGED")).toEqual(except("plain", "html"));
    expect(await search("NOT OR SEEN FLAGGED")).toEqual(except("plain", "html", "alternative"));
    expect(await search("OR FROM joerg SUBJECT html")).toEqual(of("html", "encodedHeaders"));
    expect(await search("OR SEEN FLAGGED UNDELETED FROM carol")).toEqual(of("plain", "html", "alternative"));
  });

  test("NOT or OR without enough keys is BAD", async () => {
    // NOTE: the text after "Parse error:" is the message of a JavaScript
    // exception, so only its start is pinned here.
    for (const keys of ["NOT", "OR", "OR SEEN"]) {
      const answer = await search(keys);
      expect({ keys, answer: typeof answer === "string" && answer.startsWith("BAD Parse error: ") }).toEqual({
        keys,
        answer: true,
      });
    }
  });

  test("a parenthesized group of keys is BAD", async () => {
    // NOTE: looks like a bug. RFC 3501 allows a parenthesized list of keys
    // anywhere a key may appear, and clients send them (OR (A B) (C D)). The
    // parser has no such form and fails on the opening parenthesis.
    for (const keys of ["(SEEN)", "(OR SEEN FLAGGED)", "SEEN (FLAGGED)", "OR (SEEN) (FLAGGED)", "NOT (SEEN FLAGGED)"]) {
      const answer = await search(keys);
      expect({ keys, answer: typeof answer === "string" && answer.startsWith("BAD Parse error: ") }).toEqual({
        keys,
        answer: true,
      });
    }
  });

  test("an unknown key is skipped; a search of only unknown keys has no criteria", async () => {
    expect(await search("BOGUS SEEN")).toEqual(of("plain", "html"));
    expect(await search("SEEN BOGUS")).toEqual(of("plain", "html"));
    expect(await search("X-GM-RAW SEEN")).toEqual(of("plain", "html"));
    expect(await search("BOGUS")).toBe("BAD Search criteria is required");
  });

  test("MODSEQ is not a search key: it is skipped and its number read as a sequence set", async () => {
    // NOTE: looks like a bug. CONDSTORE is advertised, and RFC 7162 3.1.5
    // makes "SEARCH MODSEQ n" return the messages changed since n. Here the
    // word is ignored like any unknown key and "5" selects message 5.
    expect(await search("MODSEQ 5")).toEqual([5]);
    expect(await search("MODSEQ 1")).toEqual([1]);
    expect(await search("MODSEQ 999999")).toEqual([]);
  });
});

describe("SEARCH: message sets", () => {
  const last = EVERY.length;

  test("a bare set names sequence numbers", async () => {
    expect(await search("1:3")).toEqual([1, 2, 3]);
    expect(await search("2,4")).toEqual([2, 4]);
    expect(await search("3:1")).toEqual([1, 2, 3]);
    expect(await search("*")).toEqual([last]);
    expect(await search(`${last}:*`)).toEqual([last]);
    expect(await search(`${last + 1}:*`)).toEqual([]);
    expect(await search("0")).toEqual([]);
    expect(await search("999")).toEqual([]);
  });

  test("a set combines with other keys", async () => {
    expect(await search("1:3 SEEN")).toEqual([1, 2]);
    expect(await search("NOT 1:3")).toEqual(EVERY.slice(3));
    expect(await search("OR 1 3")).toEqual([1, 3]);
    expect(await search("1:5 4:9")).toEqual([4, 5]);
  });

  test("the UID key is refused in a plain SEARCH, unless it is under NOT or OR", async () => {
    // NOTE: RFC 3501 allows the UID key in SEARCH as well as in UID SEARCH.
    expect(await search("UID 1:3")).toBe("NO Not supported");
    expect(await search("UID 1")).toBe("NO Not supported");
    expect(await search("SEEN UID 1")).toBe("NO Not supported");
    expect(await search("NOT UID 1")).toEqual(EVERY.slice(1));
    expect(await search("OR UID 1 UID 3")).toEqual([1, 3]);
  });

  test("in UID SEARCH a bare set and the UID key both name UIDs", async () => {
    const uid = (keys: string) => search(keys, "UID SEARCH");
    expect(await uid("ALL")).toEqual(EVERY);
    expect(await uid("1:3")).toEqual([1, 2, 3]);
    expect(await uid("UID 1:3")).toEqual([1, 2, 3]);
    expect(await uid("UID 2,4")).toEqual([2, 4]);
    expect(await uid("UID 3:1")).toEqual([1, 2, 3]);
    expect(await uid("UID *")).toEqual([last]);
    expect(await uid(`UID ${last}:*`)).toEqual([last]);
    expect(await uid(`UID ${last + 1}:*`)).toEqual([]);
    expect(await uid("UID 100")).toEqual([]);
    expect(await uid("5:*")).toEqual(EVERY.slice(4));
    expect(await uid("*")).toEqual([last]);
    expect(await uid(`NOT UID 1:${last - 2}`)).toEqual([last - 1, last]);
    expect(await uid("OR UID 1 UID 3")).toEqual([1, 3]);
    expect(await uid("FROM joerg")).toEqual(of("encodedHeaders"));
    expect(await uid("UID 1:3 SEEN")).toEqual([1, 2]);
  });

  test("the UID key needs a set", async () => {
    expect(await search("UID", "UID SEARCH")).toBe("BAD Invalid search criteria");
    expect(await search("UID abc", "UID SEARCH")).toBe("BAD Invalid search criteria");
  });

  test("sequence numbers and UIDs differ once a message has been expunged", async () => {
    const other = await createUser(server, "srchgap");
    for (const name of ["plain", "html", "alternative"] as const) await deliverFixture(server, name, other.username);
    const own = await openSession(server, other.username, other.password, "INBOX");
    await own.command("STORE 1 +FLAGS.SILENT (\\Deleted)");
    await own.command("EXPUNGE");
    expect((await own.command("SEARCH ALL")).untagged).toEqual(["* SEARCH 1 2"]);
    expect((await own.command("UID SEARCH ALL")).untagged).toEqual(["* SEARCH 2 3"]);
    expect((await own.command("SEARCH SUBJECT Alternative")).untagged).toEqual(["* SEARCH 2"]);
    expect((await own.command("UID SEARCH SUBJECT Alternative")).untagged).toEqual(["* SEARCH 3"]);
    expect((await own.command("SEARCH 2")).untagged).toEqual(["* SEARCH 2"]);
    expect((await own.command("UID SEARCH 2")).untagged).toEqual(["* SEARCH 2"]);
    expect((await own.command("UID SEARCH UID 1")).untagged).toEqual(["* SEARCH "]);
    own.close();
  });
});

describe("SEARCH: literals as search strings", () => {
  test("a synchronizing literal", async () => {
    const result = await imap.command(["SEARCH SUBJECT ", lit("Plain text")]);
    expect(result.lines).toEqual(["+ go ahead", "* SEARCH 1 16", `${result.tag} OK SEARCH completed`]);
  });

  test("a non-synchronizing literal", async () => {
    const result = await imap.command(["SEARCH SUBJECT ", lit("Plain text", false)]);
    expect(result.lines).toEqual(["* SEARCH 1 16", `${result.tag} OK SEARCH completed`]);
  });

  test("a literal carries UTF-8 octets, with CHARSET before it", async () => {
    const result = await imap.command(["SEARCH CHARSET UTF-8 TEXT ", lit(utf8("Müller"))]);
    expect(result.lines).toEqual(["+ go ahead", "* SEARCH 8", `${result.tag} OK SEARCH completed`]);
  });

  test("two literals in one command", async () => {
    const result = await imap.command(["SEARCH FROM ", lit("carol"), " SUBJECT ", lit("HTML")]);
    expect(result.lines).toEqual(["+ go ahead", "+ go ahead", "* SEARCH 2", `${result.tag} OK SEARCH completed`]);
    const nonSync = await imap.command(["SEARCH FROM ", lit("carol", false), " SUBJECT ", lit("HTML", false)]);
    expect(nonSync.lines).toEqual(["* SEARCH 2", `${nonSync.tag} OK SEARCH completed`]);
  });

  test("keys before a literal are kept", async () => {
    const result = await imap.command(["SEARCH UNSEEN SUBJECT ", lit("message", false)]);
    expect(result.untagged).toEqual([`* SEARCH ${of("alternative", "reply").join(" ")}`]);
  });

  test("keys after a literal on the same line are lost, and answered as a command of their own", async () => {
    // NOTE: looks like a bug, and one clients hit ("SEARCH SUBJECT {n} ...
    // UNSEEN" is an ordinary command). The command is run as soon as the text
    // up to the literal parses; what follows the literal is then read as a new
    // command line, whose first word is taken for a tag. Here " UNSEEN" is
    // dropped from the search, so the read message 1 is returned too, and a
    // BAD is sent for a command the client never issued.
    const tag = imap.nextTag();
    imap.write(`${tag} SEARCH SUBJECT {5+}\r\nPlain UNSEEN\r\n`);
    const result = await imap.readResult(tag);
    expect(result.lines).toEqual(["* SEARCH 1 16", `${tag} OK SEARCH completed`]);
    expect((await imap.next()).raw).toBe("UNSEEN BAD Invalid command");
  });
});

describe("SEARCH: each kind of mailbox", () => {
  test("searches only the selected mailbox, with that mailbox's numbers", async () => {
    const other = await createUser(server, "srchbox");
    await deliverFixture(server, "plain", other.username);
    const own = await openSession(server, other.username, other.password, "INBOX");
    await own.command("CREATE custom");
    await own.command("COPY 1 custom");
    expect((await own.command('SELECT "INBOX/accounts/hello"')).status).toBe("OK");
    expect((await own.command("SEARCH SUBJECT plain")).untagged).toEqual(["* SEARCH 1"]);
    expect((await own.command("UID SEARCH ALL")).untagged).toEqual(["* SEARCH 1"]);
    await own.command("SELECT custom");
    expect((await own.command("SEARCH FROM carol")).untagged).toEqual(["* SEARCH 1"]);
    await own.command("SELECT Trash");
    expect((await own.command("SEARCH ALL")).untagged).toEqual(["* SEARCH "]);
    expect((await own.command("UID SEARCH ALL")).untagged).toEqual(["* SEARCH "]);
    expect((await own.command("SEARCH 1:*")).untagged).toEqual(["* SEARCH "]);
    own.close();
  });
});
