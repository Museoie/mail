import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createUser, sleep, type TestServer, type TestUser } from "../harness";
import { ImapClient, lit, openSession } from "../harness/imap";
import { deliver, mailDomain, raws, sendOverHttp, startImapServer, type DeliveredMail } from "./support";

let server: TestServer;
let userCount = 0;

interface Box {
  user: TestUser;
  imap: ImapClient;
  mails: DeliveredMail[];
  domain: string;
}

/** A fresh account with `count` mails for `hello@` and a session with INBOX selected. */
const inbox = async (count: number): Promise<Box> => {
  const user = await createUser(server, `idle${++userCount}`);
  const domain = mailDomain(server, user.username);
  const mails: DeliveredMail[] = [];
  for (let i = 1; i <= count; i++) mails.push(await deliver(server, { to: `hello@${domain}`, subject: `Mail ${i}` }));
  const imap = await openSession(server, user.username, user.password, "INBOX");
  return { user, imap, mails, domain };
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

describe("IDLE: entering and leaving", () => {
  test("answers '+ idling' and, on DONE, completes under the IDLE tag", async () => {
    const { imap } = await inbox(1);
    const tag = imap.send("IDLE");
    expect(await imap.continuation()).toBe("+ idling");
    imap.write("DONE\r\n");
    expect((await imap.readResult(tag)).lines).toEqual([`${tag} OK IDLE terminated`]);
    // Back to normal command processing.
    expect((await imap.command("NOOP")).status).toBe("OK");
    imap.close();
  });

  test("DONE is read in any letter case, with surrounding spaces, and split across segments", async () => {
    const { imap } = await inbox(1);
    for (const done of ["done\r\n", "Done\r\n", "  DONE  \r\n"]) {
      const { tag } = await imap.idle();
      imap.write(done);
      expect((await imap.readResult(tag)).lines).toEqual([`${tag} OK IDLE terminated`]);
    }
    const { tag } = await imap.idle();
    imap.write("DO");
    await sleep(50);
    imap.write("NE\r");
    await sleep(50);
    imap.write("\n");
    expect((await imap.readResult(tag)).lines).toEqual([`${tag} OK IDLE terminated`]);
    imap.close();
  });

  test("a command pipelined behind DONE is run", async () => {
    const { imap } = await inbox(1);
    const { tag } = await imap.idle();
    imap.write("DONE\r\nP1 NOOP\r\nP2 FETCH 1 UID\r\n");
    const result = await imap.readResult("P2");
    expect(result.lines).toEqual([
      `${tag} OK IDLE terminated`,
      "P1 OK NOOP completed",
      "* 1 FETCH (UID 1)",
      "P2 OK FETCH completed",
    ]);
    imap.close();
  });

  test("anything but DONE is ignored while idling, commands and LOGOUT included", async () => {
    const { imap } = await inbox(1);
    const { tag } = await imap.idle();
    imap.write("X1 NOOP\r\nX2 LOGOUT\r\n\r\nDONEX\r\nX3 FETCH 1 UID\r\n");
    imap.write("DONE\r\n");
    // The first thing the server sends after '+ idling' is the completion.
    expect((await imap.readResult(tag)).lines).toEqual([`${tag} OK IDLE terminated`]);
    expect(imap.closed).toBe(false);
    expect((await imap.command("NOOP")).status).toBe("OK");
    imap.close();
  });

  test("arguments to IDLE are ignored", async () => {
    const { imap } = await inbox(1);
    const tag = imap.send("IDLE now please");
    expect(await imap.continuation()).toBe("+ idling");
    imap.write("DONE\r\n");
    expect((await imap.readResult(tag)).status).toBe("OK");
    imap.close();
  });

  test("needs a selected mailbox", async () => {
    const { imap } = await inbox(1);
    await imap.command("CLOSE");
    expect((await imap.command("IDLE")).lines).toEqual(["A4 NO Not authenticated or no mailbox selected"]);
    imap.close();
  });

  test("works on an examined mailbox and on an empty one", async () => {
    const { imap } = await inbox(0);
    await imap.command("EXAMINE INBOX");
    const { tag, result } = await imap.idle();
    expect(result.lines).toEqual(["+ idling"]);
    expect((await imap.done(tag)).lines).toEqual([`${tag} OK IDLE terminated`]);
    imap.close();
  });

  test("a client that drops the connection while idling does not disturb the server", async () => {
    const { imap, user, domain } = await inbox(1);
    await imap.idle();
    imap.close();
    // A delivery that would have notified the dead session still succeeds.
    await deliver(server, { to: `hello@${domain}`, subject: "After the drop" });
    const again = await openSession(server, user.username, user.password, "INBOX");
    expect((await again.command("STATUS INBOX (MESSAGES)")).untagged).toEqual(['* STATUS "INBOX" (MESSAGES 2)']);
    again.close();
  });
});

describe("IDLE: new mail", () => {
  test("mail delivered over SMTP is announced with EXISTS and RECENT 0", async () => {
    const { imap, domain } = await inbox(2);
    const { tag } = await imap.idle();
    await deliver(server, { to: `hello@${domain}`, subject: "New 1" });
    expect(await untilExists(imap)).toEqual(["* 3 EXISTS", "* 0 RECENT"]);
    await deliver(server, { to: `other@${domain}`, subject: "New 2" });
    expect(await untilExists(imap)).toEqual(["* 4 EXISTS", "* 0 RECENT"]);
    expect((await imap.done(tag)).lines).toEqual([`${tag} OK IDLE terminated`]);
    // The session can address the new messages straight away.
    expect((await imap.command("FETCH 3:4 (UID FLAGS)")).untagged).toEqual([
      "* 3 FETCH (UID 3 FLAGS ())",
      "* 4 FETCH (UID 4 FLAGS ())",
    ]);
    imap.close();
  });

  test("every idling session of the account is told", async () => {
    const { imap, user, domain } = await inbox(1);
    const second = await openSession(server, user.username, user.password, "INBOX");
    const first = await imap.idle();
    const other = await second.idle();
    await deliver(server, { to: `hello@${domain}`, subject: "New" });
    expect(await untilExists(imap)).toEqual(["* 2 EXISTS", "* 0 RECENT"]);
    expect(await untilExists(second)).toEqual(["* 2 EXISTS", "* 0 RECENT"]);
    await imap.done(first.tag);
    await second.done(other.tag);
    imap.close();
    second.close();
  });

  test("another account's mail is not announced", async () => {
    const mine = await inbox(1);
    const theirs = await inbox(1);
    const { tag } = await mine.imap.idle();
    await deliver(server, { to: `hello@${theirs.domain}`, subject: "Not mine" });
    // Then one of my own, so there is something to wait for.
    await deliver(server, { to: `hello@${mine.domain}`, subject: "Mine" });
    expect(await untilExists(mine.imap)).toEqual(["* 2 EXISTS", "* 0 RECENT"]);
    await mine.imap.done(tag);
    mine.imap.close();
    theirs.imap.close();
  });

  test("a session idling on a per-address mailbox hears only of mail for that address", async () => {
    const { imap, domain } = await inbox(1);
    await deliver(server, { to: `work@${domain}`, subject: "Work 1" });
    await imap.command('SELECT "INBOX/accounts/work"');
    const { tag } = await imap.idle();
    await deliver(server, { to: `hello@${domain}`, subject: "For hello" });
    await deliver(server, { to: `work@${domain}`, subject: "Work 2" });
    // Nothing for the first delivery: the next thing sent is the count after the second.
    expect(await untilExists(imap)).toEqual(["* 2 EXISTS", "* 0 RECENT"]);
    expect((await imap.done(tag)).lines).toEqual([`${tag} OK IDLE terminated`]);
    imap.close();
  });

  test("a session idling on a user mailbox, Trash or a sent mailbox hears nothing of new mail", async () => {
    const { imap, user, domain } = await inbox(1);
    await sendOverHttp(server, user, { sender: "me", to: "x@remote.example", subject: "Sent 1", html: "<p>s</p>" });
    await imap.command("CREATE folder");
    for (const mailbox of ["folder", "Trash", '"Sent Messages"']) {
      await imap.command(`SELECT ${mailbox}`);
      const { tag } = await imap.idle();
      await deliver(server, { to: `hello@${domain}`, subject: `While idling on ${mailbox}` });
      await sendOverHttp(server, user, { sender: "me", to: "x@remote.example", subject: "Sent more", html: "<p>s</p>" });
      expect({ mailbox, lines: (await imap.done(tag)).lines }).toEqual({ mailbox, lines: [`${tag} OK IDLE terminated`] });
    }
    imap.close();
  });

  test("mail that arrives while the session is not idling is not announced when IDLE starts", async () => {
    const { imap, domain } = await inbox(1);
    await deliver(server, { to: `hello@${domain}`, subject: "Before IDLE" });
    const { tag, result } = await imap.idle();
    expect(result.lines).toEqual(["+ idling"]);
    // The next delivery reports the whole count, the earlier message included.
    await deliver(server, { to: `hello@${domain}`, subject: "During IDLE" });
    expect(await untilExists(imap)).toEqual(["* 3 EXISTS", "* 0 RECENT"]);
    await imap.done(tag);
    imap.close();
  });
});

describe("IDLE: changes made by another IMAP connection", () => {
  test("flag changes, expunges and appends by another session are not pushed", async () => {
    // NOTE: only SMTP deliveries and HTTP flag changes notify idling
    // sessions. What a second IMAP client does is seen at the next such
    // notification, or when the idling client re-selects.
    const { imap, user, domain } = await inbox(3);
    const other = await openSession(server, user.username, user.password, "INBOX");
    const { tag } = await imap.idle();

    await other.command("STORE 1 +FLAGS.SILENT (\\Seen \\Flagged)");
    await other.command("STORE 2 +FLAGS.SILENT (\\Deleted)");
    await other.command("EXPUNGE");
    await other.command(["APPEND INBOX ", lit("Subject: appended\r\n\r\nx\r\n")]);

    // A delivery then flushes it all: the expunge first, then the new count
    // (two survivors, the appended message and the delivered one).
    await deliver(server, { to: `hello@${domain}`, subject: "New" });
    expect(await untilExists(imap)).toEqual(["* 2 EXPUNGE", "* 4 EXISTS", "* 0 RECENT"]);
    expect((await imap.done(tag)).lines).toEqual([`${tag} OK IDLE terminated`]);
    // The flag change was never announced; a FETCH shows it.
    expect((await imap.command("FETCH 1:* (UID FLAGS)")).untagged).toEqual([
      "* 1 FETCH (UID 1 FLAGS (\\Seen \\Flagged))",
      "* 2 FETCH (UID 3 FLAGS ())",
      "* 3 FETCH (UID 4 FLAGS ())",
      "* 4 FETCH (UID 5 FLAGS ())",
    ]);
    other.close();
    imap.close();
  });

  test("several pending expunges are announced from the highest sequence number down, before EXISTS", async () => {
    const { imap, user, domain } = await inbox(5);
    const other = await openSession(server, user.username, user.password, "INBOX");
    const { tag } = await imap.idle();
    await other.command("STORE 1,3,4 +FLAGS.SILENT (\\Deleted)");
    await other.command("EXPUNGE");
    await deliver(server, { to: `hello@${domain}`, subject: "New" });
    expect(await untilExists(imap)).toEqual(["* 4 EXPUNGE", "* 3 EXPUNGE", "* 1 EXPUNGE", "* 3 EXISTS", "* 0 RECENT"]);
    await imap.done(tag);
    expect((await imap.command("FETCH 1:* (UID)")).untagged).toEqual([
      "* 1 FETCH (UID 2)",
      "* 2 FETCH (UID 5)",
      "* 3 FETCH (UID 6)",
    ]);
    other.close();
    imap.close();
  });
});
