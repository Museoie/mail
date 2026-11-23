import { afterAll, afterEach, beforeAll, describe, expect, test } from "bun:test";
import { createUser, type TestServer } from "../harness";
import { proxyLine, type SmtpClient } from "../harness/smtp";
import {
  MAX_MESSAGE_BYTES,
  bootAll,
  closeClients,
  mailByMessageId,
  mailsByMessageId,
  messageId,
  plain,
  raws,
  session,
  uniqueName,
} from "./support";

// `server` is reached directly, so every connection is 127.0.0.1 and shares
// one recipient-probe budget: no test here may name an unknown local mailbox.
// Those tests run against `proxied`, where each one picks its own client address.
let server: TestServer;
let proxied: TestServer;

beforeAll(async () => {
  [server, proxied] = (await bootAll(
    { tls: true, env: { ADMIN_RO_PASSWORD: "read-only-password-e2e" } },
    { env: { PROXY_PROTOCOL: "true" } }
  )) as [TestServer, TestServer];
  await createUser(server, "alice");
  await createUser(server, "bob");
  await createUser(proxied, "alice");
}, 180_000);

afterEach(closeClients);

afterAll(async () => {
  await Promise.all([server?.stop(), proxied?.stop()]);
});

let nextAddress = 0;
/** A private client address no other test has used, so its budgets start empty. */
const freshAddress = () => {
  const n = ++nextAddress;
  return `10.77.${Math.floor(n / 250)}.${(n % 250) + 1}`;
};
const from = (address: string) => session(proxied, "smtp", { proxy: proxyLine(address) });

const BAD_SENDER = "501 Error: Bad sender address syntax";
const BAD_RECIPIENT = "501 Error: Bad recipient address syntax";
const ACCEPTED = "250 Accepted";
const NO_MAILBOX = "550 5.1.1 Error: no such mailbox here";
const TOO_MANY_PROBES = "451 4.7.0 Error: too many unknown recipients, try again later";

/** Sends one MAIL line on a clean envelope and returns the reply. */
const mailLine = async (smtp: SmtpClient, line: string) => {
  await smtp.send("RSET");
  return (await smtp.send(line)).raw;
};

/** Sends one RCPT line after a fresh MAIL and returns the reply. */
const rcptLine = async (smtp: SmtpClient, line: string) => {
  await smtp.send("RSET");
  await smtp.send("MAIL FROM:<bounce@remote.example>");
  return (await smtp.send(line)).raw;
};

describe("MAIL FROM syntax", () => {
  test("accepted forms", async () => {
    const smtp = await session(server);
    const accepted = [
      "MAIL FROM:<a@remote.example>",
      "MAIL FROM:<>",
      "MAIL FROM: <a@remote.example>",
      "MAIL FROM:   <a@remote.example>   ",
      "MAIL FROM : <a@remote.example>",
      "mail from:<A@Remote.Example>",
      "MAIL FROM:<a+tag@remote.example>",
      "MAIL FROM:<a.b-c_d@remote.example>",
      'MAIL FROM:<"quoted"@remote.example>',
      "MAIL FROM:<a@localhost>",
      "MAIL FROM:<a@[192.0.2.1]>",
      "MAIL FROM:<a@[IPv6:2001:db8::1]>",
      "MAIL FROM:<a@[ipv6:::1]>",
      "MAIL FROM:<a@xn--bcher-kva.example>",
      "MAIL FROM:<a@bücher.example>",
      "MAIL FROM:<ü@remote.example>",
      "MAIL FROM:<a@sub-domain.remote.example>",
      "MAIL FROM:<a@-remote.example>",
      "MAIL FROM:<x@mail.test>",
    ];
    for (const line of accepted) expect({ line, reply: await mailLine(smtp, line) }).toEqual({ line, reply: ACCEPTED });
  });

  test("refused forms", async () => {
    const smtp = await session(server);
    const refused = [
      "MAIL",
      "MAIL FROM",
      "MAIL FROM:",
      "MAIL FROM:a@remote.example",
      "MAIL FROM <a@remote.example>",
      "MAIL  FROM:<a@remote.example>",
      "MAIL TO:<a@remote.example>",
      "MAIL FROM:<a@remote.example",
      "MAIL FROM:a@remote.example>",
      "MAIL FROM:<<a@remote.example>>",
      "MAIL FROM:<a b@remote.example>",
      "MAIL FROM:<a@remote.example>SIZE=1",
      "MAIL FROM:<a>",
      "MAIL FROM:<a@>",
      "MAIL FROM:<@remote.example>",
      "MAIL FROM:<a@b@remote.example>",
      "MAIL FROM:<@relay.example:a@remote.example>",
      "MAIL FROM:<.a@remote.example>",
      "MAIL FROM:<a.@remote.example>",
      "MAIL FROM:<a..b@remote.example>",
      "MAIL FROM:<a@.remote.example>",
      "MAIL FROM:<a@remote.example.>",
      "MAIL FROM:<a@remote..example>",
      "MAIL FROM:<a@remote-.example>",
      "MAIL FROM:<a@remote.-example>",
      "MAIL FROM:<a@remote_host.example>",
      "MAIL FROM:<a@remote.example/path>",
      "MAIL FROM:<a@[192.0.2.999]>",
      "MAIL FROM:<a@[IPv6:zz]>",
      "MAIL FROM:<a@[remote.example]>",
      "MAIL FROM:<a@[2001:db8::1]>",
    ];
    for (const line of refused) expect({ line, reply: await mailLine(smtp, line) }).toEqual({ line, reply: BAD_SENDER });
  });

  test("an address is at most 253 octets, local part, @ and domain together", async () => {
    const smtp = await session(server);
    const domain = "remote.example";
    const longest = `${"a".repeat(253 - 1 - domain.length)}@${domain}`;
    expect(longest.length).toBe(253);
    expect(await mailLine(smtp, `MAIL FROM:<${longest}>`)).toBe(ACCEPTED);
    expect(await mailLine(smtp, `MAIL FROM:<a${longest}>`)).toBe(BAD_SENDER);
  });

  test("a refused MAIL leaves no sender behind", async () => {
    const smtp = await session(server);
    expect((await smtp.send("MAIL FROM:nobody")).raw).toBe(BAD_SENDER);
    expect((await smtp.send("RCPT TO:<hello@mail.test>")).raw).toBe("503 Error: need MAIL command");
  });
});

describe("MAIL FROM parameters", () => {
  test("SIZE up to the ceiling is accepted, one more is 552", async () => {
    const smtp = await session(server);
    expect(await mailLine(smtp, "MAIL FROM:<a@remote.example> SIZE=0")).toBe(ACCEPTED);
    expect(await mailLine(smtp, "MAIL FROM:<a@remote.example> SIZE=1000")).toBe(ACCEPTED);
    expect(await mailLine(smtp, `MAIL FROM:<a@remote.example> SIZE=${MAX_MESSAGE_BYTES}`)).toBe(ACCEPTED);
    const tooLarge = `552 Error: message exceeds fixed maximum message size ${MAX_MESSAGE_BYTES}`;
    expect(await mailLine(smtp, `MAIL FROM:<a@remote.example> SIZE=${MAX_MESSAGE_BYTES + 1}`)).toBe(tooLarge);
    expect(await mailLine(smtp, "MAIL FROM:<a@remote.example> size=99999999999")).toBe(tooLarge);
    expect(await mailLine(smtp, "MAIL FROM:<> SIZE=1e9")).toBe(tooLarge);
    // The refusal leaves the transaction unopened.
    expect((await smtp.send("RCPT TO:<hello@mail.test>")).raw).toBe("503 Error: need MAIL command");
  });

  test("a SIZE that is not a number is let through", async () => {
    // NOTE: RFC 1870 gives 501 to a malformed size-value; nothing is checked here.
    const smtp = await session(server);
    for (const parameter of ["SIZE=abc", "SIZE", "SIZE=", "SIZE=-5", "SIZE=12abc"]) {
      expect(await mailLine(smtp, `MAIL FROM:<a@remote.example> ${parameter}`)).toBe(ACCEPTED);
    }
  });

  test("BODY takes 7BIT or 8BITMIME in any case, nothing else", async () => {
    const smtp = await session(server);
    for (const parameter of ["BODY=7BIT", "BODY=8BITMIME", "body=8bitmime", "Body=7bit"]) {
      expect(await mailLine(smtp, `MAIL FROM:<a@remote.example> ${parameter}`)).toBe(ACCEPTED);
    }
    const invalid = "501 Invalid BODY parameter value. Must be 7BIT or 8BITMIME";
    for (const parameter of ["BODY=BINARYMIME", "BODY=8BIT", "BODY=x"]) {
      expect(await mailLine(smtp, `MAIL FROM:<a@remote.example> ${parameter}`)).toBe(invalid);
    }
    // A BODY with no value is refused the same way.
    expect(await mailLine(smtp, "MAIL FROM:<a@remote.example> BODY")).toBe(invalid);
  });

  test("SMTPUTF8 is a flag", async () => {
    const smtp = await session(server);
    expect(await mailLine(smtp, "MAIL FROM:<a@remote.example> SMTPUTF8")).toBe(ACCEPTED);
    expect(await mailLine(smtp, "MAIL FROM:<a@remote.example> smtputf8")).toBe(ACCEPTED);
    expect(await mailLine(smtp, "MAIL FROM:<a@remote.example> SMTPUTF8=1")).toBe(
      "501 Invalid SMTPUTF8 parameter. This flag does not accept a value"
    );
  });

  test("REQUIRETLS is refused in cleartext and is a flag over TLS", async () => {
    const cleartext = await session(server);
    const notPermitted = "530 REQUIRETLS not permitted on non-TLS connections";
    expect(await mailLine(cleartext, "MAIL FROM:<a@remote.example> REQUIRETLS")).toBe(notPermitted);
    expect(await mailLine(cleartext, "MAIL FROM:<a@remote.example> REQUIRETLS=1")).toBe(notPermitted);

    const encrypted = await session(server, "smtps");
    expect(await mailLine(encrypted, "MAIL FROM:<a@remote.example> REQUIRETLS")).toBe(ACCEPTED);
    expect(await mailLine(encrypted, "MAIL FROM:<a@remote.example> REQUIRETLS=1")).toBe(
      "501 Invalid REQUIRETLS parameter. This flag does not accept a value"
    );
  });

  test("several parameters together, and unknown ones, are accepted", async () => {
    const smtp = await session(server);
    for (const parameters of [
      "SIZE=1000 BODY=8BITMIME SMTPUTF8",
      "SMTPUTF8   BODY=7BIT",
      "RET=HDRS ENVID=QQ314159",
      "AUTH=<>",
      "X-UNKNOWN=1 Y",
      "=orphan",
      "MT-PRIORITY=3",
    ]) {
      expect({ parameters, reply: await mailLine(smtp, `MAIL FROM:<a@remote.example> ${parameters}`) }).toEqual({
        parameters,
        reply: ACCEPTED,
      });
    }
  });

  test("the first failing check answers: nested MAIL, then SIZE, BODY, SMTPUTF8, REQUIRETLS", async () => {
    const smtp = await session(server);
    const tooLarge = `552 Error: message exceeds fixed maximum message size ${MAX_MESSAGE_BYTES}`;
    expect(await mailLine(smtp, "MAIL FROM:<a@remote.example> REQUIRETLS=1 SMTPUTF8=1 BODY=x SIZE=99999999999")).toBe(tooLarge);
    expect(await mailLine(smtp, "MAIL FROM:<a@remote.example> REQUIRETLS=1 SMTPUTF8=1 BODY=x")).toBe(
      "501 Invalid BODY parameter value. Must be 7BIT or 8BITMIME"
    );
    expect(await mailLine(smtp, "MAIL FROM:<a@remote.example> REQUIRETLS=1 SMTPUTF8=1")).toBe(
      "501 Invalid SMTPUTF8 parameter. This flag does not accept a value"
    );
    await smtp.send("RSET");
    await smtp.send("MAIL FROM:<a@remote.example>");
    expect((await smtp.send("MAIL FROM:<b@remote.example> SIZE=99999999999")).raw).toBe("503 Error: nested MAIL command");
  });
});

describe("RCPT TO syntax", () => {
  test("accepted forms", async () => {
    const smtp = await session(server);
    const accepted = [
      "RCPT TO:<hello@mail.test>",
      "RCPT TO: <hello@mail.test>",
      "RCPT TO : <hello@mail.test>",
      "rcpt to:<hello@mail.test>",
      "RCPT TO:<hello@mail.test> NOTIFY=SUCCESS,FAILURE ORCPT=rfc822;hello@mail.test",
      "RCPT TO:<hello@mail.test> X-UNKNOWN",
      "RCPT TO:<someone@remote.example>",
      "RCPT TO:<someone@[192.0.2.1]>",
    ];
    for (const line of accepted) expect({ line, reply: await rcptLine(smtp, line) }).toEqual({ line, reply: ACCEPTED });
  });

  test("refused forms", async () => {
    const smtp = await session(server);
    const refused = [
      "RCPT",
      "RCPT TO",
      "RCPT TO:",
      "RCPT TO:<>",
      "RCPT TO:hello@mail.test",
      "RCPT TO <hello@mail.test>",
      "RCPT  TO:<hello@mail.test>",
      "RCPT FROM:<hello@mail.test>",
      "RCPT TO:<hello>",
      "RCPT TO:<hello@>",
      "RCPT TO:<@mail.test>",
      "RCPT TO:<hello@mail..test>",
      "RCPT TO:<hello@mail.test.>",
      "RCPT TO:<hello world@mail.test>",
      "RCPT TO:<@relay.example:hello@mail.test>",
      "RCPT TO:<hello@mail.test>NOTIFY=NEVER",
    ];
    for (const line of refused) expect({ line, reply: await rcptLine(smtp, line) }).toEqual({ line, reply: BAD_RECIPIENT });
  });

  test("the reserved <Postmaster> without a domain is refused as bad syntax", async () => {
    // NOTE: RFC 5321 §4.1.1.3 requires a server to accept `RCPT TO:<Postmaster>`
    // (no domain); this one answers 501.
    const smtp = await session(server);
    expect(await rcptLine(smtp, "RCPT TO:<Postmaster>")).toBe(BAD_RECIPIENT);
    expect(await rcptLine(smtp, "RCPT TO:<postmaster@mail.test>")).toBe(ACCEPTED);
  });

  test("syntax is checked before the need for MAIL", async () => {
    const smtp = await session(server);
    expect((await smtp.send("RCPT TO:hello@mail.test")).raw).toBe(BAD_RECIPIENT);
    expect((await smtp.send("RCPT TO:<>")).raw).toBe(BAD_RECIPIENT);
    expect((await smtp.send("RCPT TO:<hello@mail.test>")).raw).toBe("503 Error: need MAIL command");
  });
});

describe("local recipients", () => {
  test("the bare domain and admin.<domain> are admin's, <user>.<domain> is the user's", async () => {
    const smtp = await session(server);
    await smtp.send("MAIL FROM:<bounce@remote.example>");
    for (const recipient of [
      "hello@mail.test",
      "anything.at.all@mail.test",
      "x@admin.mail.test",
      "me@alice.mail.test",
      "other-box@alice.mail.test",
      "x@bob.mail.test",
    ]) {
      expect({ recipient, reply: (await smtp.rcptTo(recipient)).raw }).toEqual({ recipient, reply: ACCEPTED });
    }
  });

  test("the domain is matched in any letter case", async () => {
    const id = messageId("case");
    const smtp = await session(server);
    await smtp.send("MAIL FROM:<Bounce@Remote.EXAMPLE>");
    expect((await smtp.send("RCPT TO:<Me@ALICE.Mail.TEST>")).raw).toBe(ACCEPTED);
    expect((await smtp.send("RCPT TO:<HELLO@MAIL.TEST>")).raw).toBe(ACCEPTED);
    expect((await smtp.data(plain({ messageId: id }))).raw).toBe("250 OK: message queued");
    const rows = await mailsByMessageId(server, id);
    expect(rows.map((row) => row.username)).toEqual(["admin", "alice"]);
    // Envelope addresses are stored lower-cased.
    expect(rows[0]!.envelope_from).toEqual([{ address: "bounce@remote.example" }]);
    expect(rows[0]!.envelope_to).toEqual([{ address: "me@alice.mail.test" }, { address: "hello@mail.test" }]);
  });

  test("a mailbox is only looked up for the served domain: look-alike domains pass as foreign", async () => {
    const smtp = await session(server);
    await smtp.send("MAIL FROM:<bounce@remote.example>");
    for (const recipient of ["x@notmail.test", "x@mail.test.example", "x@test", "x@alice.mail.testing"]) {
      expect({ recipient, reply: (await smtp.rcptTo(recipient)).raw }).toEqual({ recipient, reply: ACCEPTED });
    }
  });

  test("mail for the read-only account's own subdomain is accepted and stored under it", async () => {
    // NOTE: `admin-ro` signs in as a read-only view of admin's mail, so nothing
    // ever shows what is stored under its own user id; the message is accepted
    // with 250 and is then unreachable.
    const id = messageId("ro");
    const smtp = await session(server);
    const result = await smtp.transaction({ from: "bounce@remote.example", to: ["x@admin-ro.mail.test"], data: plain({ messageId: id }) });
    expect(raws(result.replies)).toEqual([ACCEPTED, ACCEPTED, "250 OK: message queued"]);
    const rows = await mailsByMessageId(server, id);
    expect(rows.map((row) => row.username)).toEqual(["admin-ro"]);
  });

  test("the same recipient twice is accepted twice and delivered once", async () => {
    const id = messageId("dup");
    const smtp = await session(server);
    await smtp.send("MAIL FROM:<bounce@remote.example>");
    expect((await smtp.send("RCPT TO:<dup@mail.test>")).raw).toBe(ACCEPTED);
    expect((await smtp.send("RCPT TO:<DUP@Mail.Test>")).raw).toBe(ACCEPTED);
    expect((await smtp.send("RCPT TO:<dup@mail.test> NOTIFY=NEVER")).raw).toBe(ACCEPTED);
    expect((await smtp.data(plain({ messageId: id }))).raw).toBe("250 OK: message queued");
    const mail = await mailByMessageId(server, id);
    expect(mail.envelope_to).toEqual([{ address: "dup@mail.test" }]);
  });

  test("an IDN sender written in punycode is stored in Unicode", async () => {
    const id = messageId("idn");
    const smtp = await session(server);
    const result = await smtp.transaction({ from: "a@xn--bcher-kva.example", to: ["hello@mail.test"], data: plain({ messageId: id }) });
    expect(result.final.raw).toBe("250 OK: message queued");
    expect((await mailByMessageId(server, id)).envelope_from).toEqual([{ address: "a@bücher.example" }]);
  });
});

describe("unknown local recipients", () => {
  test("a label no account owns is refused 550 5.1.1, after half a second", async () => {
    const smtp = await from(freshAddress());
    await smtp.send("MAIL FROM:<bounce@remote.example>");
    const started = Date.now();
    expect((await smtp.send("RCPT TO:<x@nobody.mail.test>")).raw).toBe(NO_MAILBOX);
    expect(Date.now() - started).toBeGreaterThanOrEqual(450);
    // The refused recipient is not in the envelope.
    expect((await smtp.send("DATA")).raw).toBe("503 Error: need RCPT command");
  });

  test("nested labels, and a label that only contains a username, are refused too", async () => {
    const smtp = await from(freshAddress());
    await smtp.send("MAIL FROM:<bounce@remote.example>");
    for (const recipient of ["x@sub.alice.mail.test", "x@alice2.mail.test", "x@a.b.c.mail.test"]) {
      expect({ recipient, reply: (await smtp.rcptTo(recipient)).raw }).toEqual({ recipient, reply: NO_MAILBOX });
    }
    expect((await smtp.send("RCPT TO:<x@alice.mail.test>")).raw).toBe(ACCEPTED);
  });

  test("a soft-deleted account has no mailbox", async () => {
    const name = uniqueName("gone");
    await createUser(proxied, name);
    const smtp = await from(freshAddress());
    await smtp.send("MAIL FROM:<bounce@remote.example>");
    expect((await smtp.rcptTo(`x@${name}.mail.test`)).raw).toBe(ACCEPTED);
    await proxied.db.query("UPDATE users SET is_deleted = TRUE WHERE username = $1", [name]);
    expect((await smtp.rcptTo(`y@${name}.mail.test`)).raw).toBe(NO_MAILBOX);
  });

  test("a refused recipient does not spoil the rest of the transaction", async () => {
    const id = messageId("partial");
    const smtp = await from(freshAddress());
    await smtp.send("MAIL FROM:<bounce@remote.example>");
    expect((await smtp.send("RCPT TO:<x@nobody.mail.test>")).raw).toBe(NO_MAILBOX);
    expect((await smtp.send("RCPT TO:<x@alice.mail.test>")).raw).toBe(ACCEPTED);
    expect((await smtp.data(plain({ messageId: id }))).raw).toBe("250 OK: message queued");
    expect((await mailByMessageId(proxied, id)).envelope_to).toEqual([{ address: "x@alice.mail.test" }]);
  });

  test("after ten refusals an address gets 451 for every local recipient, known or not", async () => {
    const prober = freshAddress();
    const smtp = await from(prober);
    await smtp.send("MAIL FROM:<bounce@remote.example>");
    for (let i = 1; i <= 10; i++) {
      expect((await smtp.send(`RCPT TO:<x@probe${i}.mail.test>`)).raw).toBe(NO_MAILBOX);
    }
    const started = Date.now();
    for (let i = 11; i <= 15; i++) {
      expect((await smtp.send(`RCPT TO:<x@probe${i}.mail.test>`)).raw).toBe(TOO_MANY_PROBES);
    }
    // No lookup and no half-second wait once the budget is spent.
    expect(Date.now() - started).toBeLessThan(2000);
    expect((await smtp.send("RCPT TO:<x@alice.mail.test>")).raw).toBe(TOO_MANY_PROBES);
    expect((await smtp.send("RCPT TO:<hello@mail.test>")).raw).toBe(TOO_MANY_PROBES);
    // Foreign recipients are not this host's to judge and still pass.
    expect((await smtp.send("RCPT TO:<someone@remote.example>")).raw).toBe(ACCEPTED);

    // The budget belongs to the address, not the connection.
    const again = await from(prober);
    await again.send("MAIL FROM:<bounce@remote.example>");
    expect((await again.send("RCPT TO:<x@alice.mail.test>")).raw).toBe(TOO_MANY_PROBES);

    const other = await from(freshAddress());
    await other.send("MAIL FROM:<bounce@remote.example>");
    expect((await other.send("RCPT TO:<x@alice.mail.test>")).raw).toBe(ACCEPTED);
  }, 30_000);

  test("an account with no password yet (a sign-up in progress) has a mailbox, and is not charged to the budget", async () => {
    // The row holds the name from the first token request; the sign-up
    // completes on the same row, so what is delivered now is kept for it.
    const name = uniqueName("pending");
    const [row] = await proxied.db.query<{ user_id: string }>(
      "INSERT INTO users (username, email) VALUES ($1, $2) RETURNING user_id",
      [name, `${name}@elsewhere.example`]
    );
    const id = messageId("pending");
    const smtp = await from(freshAddress());
    await smtp.send("MAIL FROM:<bounce@remote.example>");
    const started = Date.now();
    for (let i = 0; i < 12; i++) {
      expect((await smtp.rcptTo(`x${i}@${name}.mail.test`)).raw).toBe(ACCEPTED);
    }
    expect(Date.now() - started).toBeLessThan(3000);
    expect((await smtp.data(plain({ messageId: id }))).raw).toBe("250 OK: message queued");
    const mail = await mailByMessageId(proxied, id);
    expect({ user_id: mail.user_id, username: mail.username }).toEqual({ user_id: row!.user_id, username: name });
  });

  test("recipients that resolve are never charged to the budget", async () => {
    const smtp = await from(freshAddress());
    await smtp.send("MAIL FROM:<bounce@remote.example>");
    for (let i = 1; i <= 15; i++) {
      expect((await smtp.send(`RCPT TO:<box${i}@alice.mail.test>`)).raw).toBe(ACCEPTED);
    }
    expect((await smtp.send("RCPT TO:<x@nobody.mail.test>")).raw).toBe(NO_MAILBOX);
    expect((await smtp.send("RCPT TO:<x@alice.mail.test>")).raw).toBe(ACCEPTED);
  });
});

describe("a mailbox that goes away mid-transaction", () => {
  test("DATA is answered 451 when the only recipient's account is gone by then", async () => {
    const name = uniqueName("vanish");
    await createUser(server, name);
    const id = messageId("vanish");
    const smtp = await session(server);
    await smtp.send("MAIL FROM:<bounce@remote.example>");
    expect((await smtp.rcptTo(`x@${name}.mail.test`)).raw).toBe(ACCEPTED);
    await server.db.query("DELETE FROM users WHERE username = $1", [name]);
    expect((await smtp.data(plain({ messageId: id }))).raw).toBe("451 4.2.1 Error: mailbox unavailable, try again later");
    expect(await mailsByMessageId(server, id)).toEqual([]);
    // The session carries on.
    expect((await smtp.send("NOOP")).raw).toBe("250 OK");
  });

  test("DATA is accepted when another recipient still has a mailbox", async () => {
    const name = uniqueName("vanish");
    await createUser(server, name);
    const id = messageId("vanish");
    const smtp = await session(server);
    await smtp.send("MAIL FROM:<bounce@remote.example>");
    expect((await smtp.rcptTo(`x@${name}.mail.test`)).raw).toBe(ACCEPTED);
    expect((await smtp.rcptTo("x@bob.mail.test")).raw).toBe(ACCEPTED);
    await server.db.query("DELETE FROM users WHERE username = $1", [name]);
    expect((await smtp.data(plain({ messageId: id }))).raw).toBe("250 OK: message queued");
    expect((await mailsByMessageId(server, id)).map((row) => row.username)).toEqual(["bob"]);
  });
});

describe("relay refusal", () => {
  const RELAY_DENIED = "550 Error: relay access denied";

  test("foreign recipients are accepted at RCPT and refused 550 once DATA has been read", async () => {
    const smtp = await session(server);
    expect((await smtp.send("MAIL FROM:<a@remote.example>")).raw).toBe(ACCEPTED);
    expect((await smtp.send("RCPT TO:<b@elsewhere.example>")).raw).toBe(ACCEPTED);
    expect((await smtp.send("DATA")).raw).toBe("354 End data with <CR><LF>.<CR><LF>");
    // Nothing is said until the message has been sent in full.
    smtp.write(plain());
    expect(await smtp.staysQuiet(300)).toBe(true);
    smtp.write(".\r\n");
    expect((await smtp.read()).raw).toBe(RELAY_DENIED);
    // The connection stays open and the envelope is clean.
    expect((await smtp.send("RCPT TO:<hello@mail.test>")).raw).toBe("503 Error: need MAIL command");
    expect(server.mailgun.requests).toEqual([]);
  });

  test("a local-looking sender does not make an unauthenticated session a submission", async () => {
    const smtp = await session(server);
    for (const sender of ["me@alice.mail.test", "admin@mail.test", ""]) {
      const result = await smtp.transaction({ from: sender, to: ["b@elsewhere.example"], data: plain() });
      expect({ sender, replies: raws(result.replies) }).toEqual({ sender, replies: [ACCEPTED, ACCEPTED, RELAY_DENIED] });
    }
    expect(server.mailgun.requests).toEqual([]);
  });

  test("a local-looking sender delivering to a local recipient is ordinary inbound mail", async () => {
    const id = messageId("spoof");
    const smtp = await session(server);
    const result = await smtp.transaction({ from: "me@alice.mail.test", to: ["x@bob.mail.test"], data: plain({ messageId: id }) });
    expect(result.final.raw).toBe("250 OK: message queued");
    const mail = await mailByMessageId(server, id);
    expect(mail.username).toBe("bob");
    expect(mail.sent).toBe(false);
    expect(mail.envelope_from).toEqual([{ address: "me@alice.mail.test" }]);
  });

  test("with local and foreign recipients mixed, the local ones are stored and the foreign ones dropped", async () => {
    // NOTE: the foreign recipient was answered 250 at RCPT and the message
    // 250 at DATA, but nothing is relayed to it and no bounce is produced.
    const id = messageId("mixed");
    const smtp = await session(server);
    const result = await smtp.transaction({
      from: "a@remote.example",
      to: ["b@elsewhere.example", "x@bob.mail.test", "c@elsewhere.example"],
      data: plain({ messageId: id }),
    });
    expect(raws(result.replies)).toEqual([ACCEPTED, ACCEPTED, ACCEPTED, ACCEPTED, "250 OK: message queued"]);
    const rows = await mailsByMessageId(server, id);
    expect(rows.map((row) => row.username)).toEqual(["bob"]);
    expect(rows[0]!.envelope_to).toEqual([
      { address: "b@elsewhere.example" },
      { address: "x@bob.mail.test" },
      { address: "c@elsewhere.example" },
    ]);
    expect(server.mailgun.requests).toEqual([]);
  });
});

describe("recipient limit", () => {
  test("100 recipients are accepted and the 101st is 452", async () => {
    const id = messageId("limit");
    const smtp = await session(server);
    await smtp.send("MAIL FROM:<bounce@remote.example>");
    expect((await smtp.send("RCPT TO:<r0@alice.mail.test>")).raw).toBe(ACCEPTED);
    smtp.write(Array.from({ length: 99 }, (_, i) => `RCPT TO:<r${i + 1}@elsewhere.example>\r\n`).join(""));
    const replies = new Set<string>();
    for (let i = 0; i < 99; i++) replies.add((await smtp.read()).raw);
    expect([...replies]).toEqual([ACCEPTED]);

    const tooMany = "452 Error: too many recipients";
    expect((await smtp.send("RCPT TO:<r100@elsewhere.example>")).raw).toBe(tooMany);
    expect((await smtp.send("RCPT TO:<x@bob.mail.test>")).raw).toBe(tooMany);
    // Syntax still comes first, and a recipient already listed is not a new one.
    expect((await smtp.send("RCPT TO:<>")).raw).toBe(BAD_RECIPIENT);
    expect((await smtp.send("RCPT TO:<R7@Elsewhere.Example>")).raw).toBe(ACCEPTED);
    expect((await smtp.send("RCPT TO:<R0@Alice.Mail.Test>")).raw).toBe(ACCEPTED);

    expect((await smtp.data(plain({ messageId: id }))).raw).toBe("250 OK: message queued");
    const mail = await mailByMessageId(server, id);
    expect(mail.username).toBe("alice");
    expect(mail.envelope_to).toHaveLength(100);
    expect(mail.envelope_to[0]).toEqual({ address: "r0@alice.mail.test" });
    expect(mail.envelope_to[7]).toEqual({ address: "r7@elsewhere.example" });
    expect(mail.envelope_to[99]).toEqual({ address: "r99@elsewhere.example" });

    // The count starts over with the next transaction.
    await smtp.send("MAIL FROM:<bounce@remote.example>");
    expect((await smtp.send("RCPT TO:<r100@elsewhere.example>")).raw).toBe(ACCEPTED);
  });
});
