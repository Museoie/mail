import { afterAll, afterEach, beforeAll, describe, expect, test } from "bun:test";
import { createUser, type TestServer, type TestUser } from "../harness";
import { proxyLine } from "../harness/smtp";
import { FIXED_DATE, FIXED_DATE_ISO, Mailer, bootAll, closeClients, crlf, deliver, mailByMessageId, mappingsOf, messageId, session, uniqueName, type Row } from "./support";

// The spam check every inbound message goes through before it is stored:
// the sender allowlist, the content rules, and the per-user classifier. The
// DNS blocklists are only asked about public IPv4 addresses, which these
// tests never connect from: 127.0.0.1 counts as cleared without a lookup.
let server: TestServer;
let proxied: TestServer;
let mailer: Mailer;

beforeAll(async () => {
  [server, proxied] = (await bootAll({}, { env: { PROXY_PROTOCOL: "true" } })) as [TestServer, TestServer];
  mailer = new Mailer(server);
}, 180_000);

afterEach(closeClients);

afterAll(async () => {
  mailer?.close();
  await Promise.all([server?.stop(), proxied?.stop()]);
});

const freshUser = async (on: TestServer = server) => {
  const user = await createUser(on, uniqueName("u"));
  return { ...user, at: (local: string) => `${local}@${user.username}.mail.test` };
};

interface Message {
  from?: string | null;
  replyTo?: string;
  subject?: string | null;
  body?: string;
  /** Replaces the default text/plain content headers and body. */
  content?: string[];
}

const build = (id: string, message: Message) =>
  crlf(
    ...(message.from === null ? [] : [`From: ${message.from ?? "Friend <friend@trusted.example>"}`]),
    ...(message.replyTo ? [`Reply-To: ${message.replyTo}`] : []),
    ...(message.subject === null ? [] : [`Subject: ${message.subject ?? "Lunch on Thursday"}`]),
    `Date: ${FIXED_DATE}`,
    `Message-ID: ${id}`,
    ...(message.content ?? ["Content-Type: text/plain; charset=utf-8", "", message.body ?? "Shall we meet at noon?"])
  );

const verdict = (mail: Row) => ({ score: mail.spam_score, reasons: mail.spam_reasons, spam: mail.is_spam });

/** Delivers one message to the bare domain (admin) and returns what the spam check stored. */
const judge = async (message: Message, envelopeFrom = "friend@trusted.example") =>
  verdict(await mailer.store(build(messageId("spam"), message), { from: envelopeFrom }));

const CLEAN = { score: 0, reasons: [], spam: false };
const only = (score: number, reason: string) => ({ score, reasons: [reason], spam: false });

describe("content rules", () => {
  test("an ordinary message scores nothing", async () => {
    expect(await judge({})).toEqual(CLEAN);
  });

  test("Missing From header: 20", async () => {
    expect(await judge({ from: null })).toEqual(only(20, "Missing From header"));
    // A From that holds no address is as good as missing.
    expect(await judge({ from: "Just A Name" })).toEqual(only(20, "Missing From header"));
    expect(await judge({ from: "friend@trusted.example" })).toEqual(CLEAN);
  });

  test("Reply-To differs from From: 15, when the domains differ", async () => {
    expect(await judge({ replyTo: "collect@elsewhere.example" })).toEqual(only(15, "Reply-To differs from From"));
    expect(await judge({ replyTo: "other@trusted.example" })).toEqual(CLEAN);
    expect(await judge({ replyTo: "Other <other@TRUSTED.example>" })).toEqual(CLEAN);
    // A subdomain is a different domain.
    expect(await judge({ replyTo: "other@mail.trusted.example" })).toEqual(only(15, "Reply-To differs from From"));
    // Without a From there is nothing to differ from.
    expect(await judge({ from: null, replyTo: "collect@elsewhere.example" })).toEqual(only(20, "Missing From header"));
  });

  test("Subject >50% uppercase: 10, counting letters only, from five letters up", async () => {
    const rule = only(10, "Subject >50% uppercase");
    expect(await judge({ subject: "URGENT NEWS for you" })).toEqual(rule);
    expect(await judge({ subject: "ABCdef" })).toEqual(CLEAN);
    expect(await judge({ subject: "ABCDef" })).toEqual(rule);
    expect(await judge({ subject: "ABCD" })).toEqual(CLEAN);
    expect(await judge({ subject: "ABCDE" })).toEqual(rule);
    expect(await judge({ subject: "Re: 2026 Q1 (okay)" })).toEqual(CLEAN);
    // Digits and punctuation do not count, so a short harmless subject can trip it: R, E, Q against o, k.
    expect(await judge({ subject: "RE: 2026 Q1 (ok)" })).toEqual(rule);
    // The subject is judged after decoding.
    expect(await judge({ subject: `=?utf-8?B?${Buffer.from("WAKE UP NOW").toString("base64")}?=` })).toEqual(rule);
  });

  test("URL shorteners in body: 25, for a shortener's host on its own", async () => {
    const rule = only(25, "URL shorteners in body");
    for (const body of ["Look: https://bit.ly/3abc", "tinyurl.com/x", "see T.CO/abc", "(goo.gl)", "ow.ly is.gd buff.ly", "adf.ly", "cutt.ly/q", "rebrand.ly", "short.link/z"]) {
      expect({ body, ...(await judge({ body })) }).toEqual({ body, ...rule });
    }
    // Not inside a longer host name, and not as a suffix of another word.
    for (const body of ["https://wealthfront.com/", "marriott.com and att.com", "https://sub.bit.ly/x", "bit.lyx", "bit.ly-more", "https://example.com/bit"]) {
      expect({ body, ...(await judge({ body })) }).toEqual({ body, ...CLEAN });
    }
  });

  test("Marketing without unsubscribe: 15", async () => {
    const rule = only(15, "Marketing without unsubscribe");
    for (const phrase of ["special offer", "Limited Time", "act now", "BUY NOW", "click here", "free gift", "exclusive deal"]) {
      expect({ phrase, ...(await judge({ body: `A ${phrase} for you.` })) }).toEqual({ phrase, ...rule });
    }
    for (const way of ["unsubscribe", "opt out", "Opt-Out", "manage preferences"]) {
      expect({ way, ...(await judge({ body: `A special offer for you.\nTo ${way}, reply.` })) }).toEqual({ way, ...CLEAN });
    }
  });

  test("Empty subject line: 10", async () => {
    expect(await judge({ subject: null })).toEqual(only(10, "Empty subject line"));
    expect(await judge({ subject: "" })).toEqual(only(10, "Empty subject line"));
    expect(await judge({ subject: "   " })).toEqual(only(10, "Empty subject line"));
  });

  test("Excessive exclamation marks in subject: 10, from three", async () => {
    expect(await judge({ subject: "Wow!! really" })).toEqual(CLEAN);
    expect(await judge({ subject: "Wow! really! yes!" })).toEqual(only(10, "Excessive exclamation marks in subject"));
  });

  test("Contains suspicious spam phrases: 15", async () => {
    const rule = only(15, "Contains suspicious spam phrases");
    for (const phrase of [
      "congratulations you have won",
      "You Are A Winner",
      "claim your prize",
      "urgent action required",
      "your account will be suspended",
      "verify your account immediately",
      "Nigerian prince",
      "wire transfer",
      "lottery winner",
    ]) {
      expect({ phrase, ...(await judge({ body: `Dear friend, ${phrase}. Regards.` })) }).toEqual({ phrase, ...rule });
    }
    // Only the body is read, not the subject.
    expect(await judge({ subject: "claim your prize" })).toEqual(CLEAN);
  });

  test("Cryptocurrency spam indicators: 20", async () => {
    const rule = only(20, "Cryptocurrency spam indicators");
    for (const phrase of ["bitcoin investment", "crypto millionaire", "guaranteed returns", "double your bitcoin", "FREE CRYPTO"]) {
      expect({ phrase, ...(await judge({ body: `Try this ${phrase} today.` })) }).toEqual({ phrase, ...rule });
    }
  });

  test("HTML-only email with no text alternative: 5, for an HTML part inside a multipart", async () => {
    const htmlInMultipart = ['Content-Type: multipart/alternative; boundary="alt"', "", "--alt", "Content-Type: text/html", "", "<p>Shall we meet?</p>", "--alt--"];
    expect(await judge({ content: htmlInMultipart })).toEqual(only(5, "HTML-only email with no text alternative"));
    // A message that is one HTML part gets its text derived on the way in, and does not count.
    expect(await judge({ content: ["Content-Type: text/html", "", "<p>Shall we meet?</p>"] })).toEqual(CLEAN);
  });

  test("the body rules read the text of an HTML message, tags removed", async () => {
    // "special offer" is split by a tag in the markup, and the shortener only appears in an href.
    expect(await judge({ content: ["Content-Type: text/html", "", '<p>A <b>special</b> offer: <a href="https://bit.ly/x">here</a></p>'] })).toEqual({
      score: 40,
      reasons: ["URL shorteners in body", "Marketing without unsubscribe"],
      spam: false,
    });
  });
});

describe("the verdict", () => {
  test("scores add up in rule order, and 50 makes it spam", async () => {
    expect(
      await judge({ from: null, subject: null, body: "free crypto" })
    ).toEqual({ score: 50, reasons: ["Missing From header", "Empty subject line", "Cryptocurrency spam indicators"], spam: true });
    expect(await judge({ body: "bit.ly/x and free crypto" })).toEqual({
      score: 45,
      reasons: ["URL shorteners in body", "Cryptocurrency spam indicators"],
      spam: false,
    });
  });

  test("every rule at once", async () => {
    expect(
      await judge({
        from: null,
        replyTo: "collect@elsewhere.example",
        subject: "ACT NOW!!!",
        content: [
          'Content-Type: multipart/alternative; boundary="alt"',
          "",
          "--alt",
          "Content-Type: text/html",
          "",
          "<p>special offer: claim your prize, free crypto at bit.ly/x</p>",
          "--alt--",
        ],
      })
    ).toEqual({
      score: 120,
      reasons: [
        "Missing From header",
        "Subject >50% uppercase",
        "URL shorteners in body",
        "Marketing without unsubscribe",
        "Excessive exclamation marks in subject",
        "Contains suspicious spam phrases",
        "Cryptocurrency spam indicators",
        "HTML-only email with no text alternative",
      ],
      spam: true,
    });
  });

  test("a spam message is still accepted and stored, filed like any other, and listed apart over HTTP", async () => {
    const user = await freshUser();
    const id = messageId("spam");
    const result = await deliver(server, {
      from: "x@spammer.example",
      to: [user.at("inbox")],
      data: build(id, { from: null, subject: null, body: "free crypto" }),
    });
    expect(result.final.raw).toBe("250 OK: message queued");
    const mail = await mailByMessageId(server, id);
    expect(verdict(mail).spam).toBe(true);
    expect([mail.read, mail.deleted, mail.uid_domain]).toEqual([false, false, 1]);
    expect(await mappingsOf(server, mail.mail_id)).toEqual([
      { mailbox: "INBOX", uid: 1 },
      { mailbox: "INBOX/accounts/inbox", uid: 1 },
    ]);

    const client = server.client();
    await client.login(user.username, user.password);
    const { body } = await client.get("/api/mails/accounts");
    expect(body.body.received).toEqual([]);
    expect(body.body.spam).toEqual([
      { key: user.at("inbox"), doc_count: 1, unread_doc_count: 1, saved_doc_count: 0, updated: FIXED_DATE_ISO },
    ]);
  });

  test("each recipient gets its own verdict", async () => {
    const [listed, unlisted] = [await freshUser(), await freshUser()];
    const client = server.client();
    await client.login(listed.username, listed.password);
    expect((await client.post("/api/mails/spam-allowlist", { pattern: "friend@trusted.example" })).body.status).toBe("success");

    const id = messageId("spam");
    await deliver(server, {
      from: "friend@trusted.example",
      to: [listed.at("a"), unlisted.at("b")],
      data: build(id, { subject: null, body: "free crypto at bit.ly/x" }),
    });
    const rows = await server.db.query("SELECT m.*, u.username FROM mails m JOIN users u USING (user_id) WHERE m.message_id = $1", [id]);
    const byUser = new Map(rows.map((row) => [row.username, verdict(row)]));
    expect(byUser.get(listed.username)).toEqual({ score: 0, reasons: ["Sender is allowlisted"], spam: false });
    expect(byUser.get(unlisted.username)).toEqual({
      score: 55,
      reasons: ["URL shorteners in body", "Empty subject line", "Cryptocurrency spam indicators"],
      spam: true,
    });
  });
});

describe("the sender allowlist", () => {
  const SPAMMY: Message = { subject: null, body: "free crypto at bit.ly/x" };
  const BY_RULES = ["URL shorteners in body", "Empty subject line", "Cryptocurrency spam indicators"];
  const ALLOWED = { score: 0, reasons: ["Sender is allowlisted"], spam: false };

  /** A user with one allowlist entry, and a way to deliver the spammy message to them. */
  const listedUser = async (pattern: string, on: TestServer = server) => {
    const user = await freshUser(on);
    const client = on.client();
    await client.login(user.username, user.password);
    const added = await client.post("/api/mails/spam-allowlist", { pattern });
    expect(added.body.status).toBe("success");
    return user;
  };
  const judgeFor = async (user: TestUser & { at(local: string): string }, message: Message, envelopeFrom: string) => {
    const id = messageId("allow");
    await deliver(server, { from: envelopeFrom, to: [user.at("inbox")], data: build(id, message) });
    return verdict(await mailByMessageId(server, id));
  };

  test("a listed sender whose envelope sender has the same domain skips every rule", async () => {
    const user = await listedUser("friend@trusted.example");
    expect(await judgeFor(user, SPAMMY, "friend@trusted.example")).toEqual(ALLOWED);
    // Any mailbox of that domain in the envelope will do, in any letter case.
    expect(await judgeFor(user, SPAMMY, "bounces+123@TRUSTED.example")).toEqual(ALLOWED);
    expect(await judgeFor(user, { ...SPAMMY, from: "FRIEND@Trusted.Example" }, "friend@trusted.example")).toEqual(ALLOWED);
  });

  test("a domain wildcard lists every sender of the domain, and only that domain", async () => {
    const user = await listedUser("*@trusted.example");
    expect(await judgeFor(user, { ...SPAMMY, from: "anyone@trusted.example" }, "bounce@trusted.example")).toEqual(ALLOWED);
    expect(await judgeFor(user, { ...SPAMMY, from: "anyone@sub.trusted.example" }, "bounce@sub.trusted.example")).toEqual({
      score: 55,
      reasons: BY_RULES,
      spam: true,
    });
  });

  test("an exact entry does not list other senders of the domain", async () => {
    const user = await listedUser("friend@trusted.example");
    expect(await judgeFor(user, { ...SPAMMY, from: "stranger@trusted.example" }, "stranger@trusted.example")).toEqual({
      score: 55,
      reasons: BY_RULES,
      spam: true,
    });
  });

  test("a listed From with a different, missing or subdomain envelope sender is judged, and the reason says why", async () => {
    const user = await listedUser("friend@trusted.example");
    const unconfirmed = { score: 55, reasons: ["Allowlisted sender not confirmed by the envelope sender", ...BY_RULES], spam: true };
    expect(await judgeFor(user, SPAMMY, "friend@forger.example")).toEqual(unconfirmed);
    expect(await judgeFor(user, SPAMMY, "")).toEqual(unconfirmed);
    expect(await judgeFor(user, SPAMMY, "bounce@mail.trusted.example")).toEqual(unconfirmed);
    // The note is added even when the rules find nothing.
    expect(await judgeFor(user, {}, "friend@forger.example")).toEqual({
      score: 0,
      reasons: ["Allowlisted sender not confirmed by the envelope sender"],
      spam: false,
    });
  });

  test("from an address no blocklist can be asked about, a listed sender is judged too", async () => {
    // An IPv6 client: the blocklists are IPv4-only, so nothing vouches for the connection.
    const user = await listedUser("friend@trusted.example", proxied);
    const judgeFrom = async (address: string) => {
      const id = messageId("allow");
      const smtp = await session(proxied, "smtp", { proxy: proxyLine(address) });
      const result = await smtp.transaction({ from: "friend@trusted.example", to: [user.at("inbox")], data: build(id, SPAMMY) });
      expect(result.final.raw).toBe("250 OK: message queued");
      return verdict(await mailByMessageId(proxied, id));
    };
    expect(await judgeFrom("2001:db8::25")).toEqual({
      score: 55,
      reasons: ["Allowlisted sender arrived from an address no blocklist answered for", ...BY_RULES],
      spam: true,
    });
    // Private and loopback IPv4 addresses count as cleared without a lookup.
    for (const address of ["10.1.2.3", "172.16.0.9", "192.168.1.1", "169.254.1.1", "127.0.0.9"]) {
      expect({ address, ...(await judgeFrom(address)) }).toEqual({ address, ...ALLOWED });
    }
  });
});

describe("the per-user classifier", () => {
  /** Seeds a user's training table: document totals, then per-word counts. */
  const train = async (userId: string, docs: { spam: number; ham: number }, words: Record<string, [number, number]>) => {
    const rows: [string, number, number][] = [["__total__", docs.spam, docs.ham], ...Object.entries(words).map(([word, [spam, ham]]) => [word, spam, ham] as [string, number, number])];
    for (const [word, spam, ham] of rows) {
      await server.db.query("INSERT INTO spam_training (user_id, word, spam_count, ham_count) VALUES ($1, $2, $3, $4)", [userId, word, spam, ham]);
    }
  };
  const judgeFor = async (user: { at(local: string): string }, message: Message) => {
    const id = messageId("bayes");
    await deliver(server, { from: "friend@trusted.example", to: [user.at("inbox")], data: build(id, message) });
    return verdict(await mailByMessageId(server, id));
  };
  const PILLS: Message = { subject: "Cheap pills", body: "buy cheap pills today" };

  test("a trained user's spam-like message gets the classifier's score and reason", async () => {
    const user = await freshUser();
    await train(user.id, { spam: 4, ham: 1 }, { cheap: [4, 0], pills: [4, 0], buy: [3, 0], today: [1, 1] });
    // The words are cheap, pills, buy and today (subject and body, each once).
    // P(spam) = 0.8 * 5/6 * 5/6 * 4/6 * 2/6 against P(ham) = 0.2 * 1/3 * 1/3 * 1/3 * 2/3: 96%.
    expect(await judgeFor(user, PILLS)).toEqual({
      score: 96,
      reasons: ["Bayesian classifier: 96% spam probability (4 spam / 1 ham documents trained)"],
      spam: true,
    });
  });

  test("a ham-like message for the same training adds nothing", async () => {
    const user = await freshUser();
    await train(user.id, { spam: 4, ham: 4 }, { lunch: [0, 4], thursday: [0, 4], shall: [0, 3], meet: [0, 4], noon: [0, 3] });
    expect(await judgeFor(user, {})).toEqual(CLEAN);
  });

  test("the classifier's score is added to the rules' score", async () => {
    const user = await freshUser();
    await train(user.id, { spam: 4, ham: 1 }, { cheap: [4, 0], pills: [4, 0], buy: [3, 0], today: [1, 1] });
    expect(await judgeFor(user, { ...PILLS, subject: null })).toEqual({
      score: 106,
      reasons: ["Empty subject line", "Bayesian classifier: 96% spam probability (4 spam / 1 ham documents trained)"],
      spam: true,
    });
  });

  test("with fewer than five documents, or none of one kind, the classifier stays out", async () => {
    const few = await freshUser();
    await train(few.id, { spam: 3, ham: 1 }, { cheap: [3, 0], pills: [3, 0] });
    expect(await judgeFor(few, PILLS)).toEqual(CLEAN);
    const noHam = await freshUser();
    await train(noHam.id, { spam: 9, ham: 0 }, { cheap: [9, 0], pills: [9, 0] });
    expect(await judgeFor(noHam, PILLS)).toEqual(CLEAN);
  });

  test("training is per user", async () => {
    const [trained, untrained] = [await freshUser(), await freshUser()];
    await train(trained.id, { spam: 4, ham: 1 }, { cheap: [4, 0], pills: [4, 0], buy: [3, 0], today: [1, 1] });
    expect((await judgeFor(untrained, PILLS)).spam).toBe(false);
    expect((await judgeFor(trained, PILLS)).spam).toBe(true);
  });
});
