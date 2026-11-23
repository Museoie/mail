import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { ADMIN_PASSWORD, sleep, waitFor, type TestServer } from "../harness";
import { ImapClient, lit } from "../harness/imap";
import { CAPABILITIES, exchange, raws, startImapServer } from "./support";

let server: TestServer;

beforeAll(async () => {
  server = await startImapServer();
}, 90_000);

afterAll(async () => {
  await server?.stop();
});

const GREETING = `* OK [CAPABILITY ${CAPABILITIES}] IMAP4rev1 Service Ready`;

describe("greeting and the commands valid in any state", () => {
  test("greets with the capability list when no certificate is configured", async () => {
    const imap = await ImapClient.connect(server);
    expect(imap.greeting).toBe(GREETING);
    expect(imap.transcript).toBe(`${GREETING}\r\n`);
    imap.close();
  });

  test("CAPABILITY lists the same capabilities before and after login", async () => {
    const imap = await ImapClient.connect(server);
    const before = await imap.command("CAPABILITY");
    expect(before.lines).toEqual([`* CAPABILITY ${CAPABILITIES}`, "A1 OK CAPABILITY completed"]);

    const login = await imap.command(`LOGIN admin ${ADMIN_PASSWORD}`);
    expect(login.lines).toEqual([`A2 OK [CAPABILITY ${CAPABILITIES}] LOGIN completed`]);

    const after = await imap.command("CAPABILITY");
    expect(after.lines).toEqual([`* CAPABILITY ${CAPABILITIES}`, "A3 OK CAPABILITY completed"]);
    await imap.logout();
  });

  test("NOOP and CHECK complete without a login", async () => {
    const imap = await ImapClient.connect(server);
    expect((await imap.command("NOOP")).lines).toEqual(["A1 OK NOOP completed"]);
    expect((await imap.command("CHECK")).lines).toEqual(["A2 OK CHECK completed"]);
    imap.close();
  });

  test("ID answers NIL whatever the client sends", async () => {
    const imap = await ImapClient.connect(server);
    for (const [index, args] of ["ID NIL", 'ID ("name" "e2e" "version" "1.0")', "ID", "ID garbage ("].entries()) {
      const result = await imap.command(args);
      expect(result.lines).toEqual(["* ID NIL", `A${index + 1} OK ID completed`]);
    }
    imap.close();
  });

  test("NAMESPACE reports one personal namespace with / as delimiter, even before login", async () => {
    const imap = await ImapClient.connect(server);
    expect((await imap.command("NAMESPACE")).lines).toEqual([
      '* NAMESPACE (("" "/")) NIL NIL',
      "A1 OK NAMESPACE completed",
    ]);
    imap.close();
  });

  test("GETQUOTAROOT is refused, with or without a mailbox", async () => {
    const imap = await ImapClient.connect(server);
    expect((await imap.command("GETQUOTAROOT INBOX")).lines).toEqual(["A1 NO Quota not supported"]);
    expect((await imap.command("GETQUOTAROOT")).lines).toEqual(["A2 NO Quota not supported"]);
    await imap.login("admin", ADMIN_PASSWORD);
    expect((await imap.command('GETQUOTAROOT "INBOX"')).lines).toEqual(["A4 NO Quota not supported"]);
    imap.close();
  });

  test("ENABLE echoes only CONDSTORE, and only the first time", async () => {
    const imap = await ImapClient.connect(server);
    expect((await imap.command("ENABLE CONDSTORE")).lines).toEqual([
      "* ENABLED CONDSTORE",
      "A1 OK ENABLE completed",
    ]);
    expect((await imap.command("ENABLE CONDSTORE")).lines).toEqual(["* ENABLED", "A2 OK ENABLE completed"]);
    expect((await imap.command("ENABLE QRESYNC UTF8=ACCEPT")).lines).toEqual([
      "* ENABLED",
      "A3 OK ENABLE completed",
    ]);
    expect((await imap.command("ENABLE")).lines).toEqual(["* ENABLED", "A4 OK ENABLE completed"]);
    imap.close();
  });

  test("ENABLE matches the capability name in any letter case", async () => {
    const imap = await ImapClient.connect(server);
    expect((await imap.command("enable qresync condstore")).lines).toEqual([
      "* ENABLED CONDSTORE",
      "A1 OK ENABLE completed",
    ]);
    imap.close();
  });
});

describe("LOGOUT", () => {
  test("says BYE, completes and closes the connection", async () => {
    const imap = await ImapClient.connect(server);
    const result = await imap.command("LOGOUT");
    expect(result.lines).toEqual(["* BYE IMAP4rev1 Server logging out", "A1 OK LOGOUT completed"]);
    expect(await imap.waitForClose()).toEqual([]);
    expect(imap.closed).toBe(true);
  });

  test("works after login and with a mailbox selected", async () => {
    const imap = await ImapClient.connect(server);
    await imap.login("admin", ADMIN_PASSWORD);
    await imap.command("SELECT INBOX");
    const result = await imap.command("LOGOUT");
    expect(result.lines).toEqual(["* BYE IMAP4rev1 Server logging out", "A3 OK LOGOUT completed"]);
    await imap.waitForClose();
  });

  test("ignores trailing arguments, and drops the commands pipelined behind it", async () => {
    const imap = await ImapClient.connect(server);
    imap.write("Q1 logout now\r\nQ2 NOOP\r\n");
    const rest = await imap.waitForClose();
    expect(raws(rest)).toEqual(["* BYE IMAP4rev1 Server logging out", "Q1 OK LOGOUT completed"]);
  });
});

describe("commands in the not-authenticated state", () => {
  const refusals: [string, string][] = [
    ['LIST "" "*"', "NO Not authenticated."],
    ['LSUB "" "*"', "NO Not authenticated."],
    ["SELECT INBOX", "NO Not authenticated."],
    ["EXAMINE INBOX", "NO Not authenticated."],
    ["CREATE foo", "NO Not authenticated."],
    ["DELETE foo", "NO Not authenticated."],
    ["RENAME foo bar", "NO Not authenticated."],
    ["SUBSCRIBE foo", "NO Not authenticated."],
    ["UNSUBSCRIBE foo", "NO Not authenticated."],
    ["STATUS INBOX (MESSAGES)", "NO Not authenticated."],
    ["FETCH 1 FLAGS", "NO Not authenticated."],
    ["SEARCH ALL", "NO Not authenticated."],
    // An empty SEARCH is refused for the state before its missing criteria.
    ["SEARCH", "NO Not authenticated."],
    ["STORE 1 +FLAGS (\\Seen)", "NO Not authenticated."],
    ["COPY 1 INBOX", "NO Not authenticated or no mailbox selected."],
    ["MOVE 1 INBOX", "NO Not authenticated or no mailbox selected."],
    ["UID FETCH 1 FLAGS", "NO Not authenticated."],
    ["UID SEARCH ALL", "NO Not authenticated."],
    ["UID STORE 1 +FLAGS (\\Seen)", "NO Not authenticated."],
    ["UID COPY 1 INBOX", "NO Not authenticated or no mailbox selected."],
    ["UID MOVE 1 INBOX", "NO Not authenticated or no mailbox selected."],
    ["EXPUNGE", "NO Not authenticated."],
    ["CLOSE", "NO Not authenticated."],
    ["UNSELECT", "NO Not authenticated."],
    // No trailing full stop on this one.
    ["IDLE", "NO Not authenticated or no mailbox selected"],
    ["STARTTLS", "NO STARTTLS is not available"],
  ];

  test("every mailbox and message command is refused with its own text", async () => {
    const imap = await ImapClient.connect(server);
    for (const [command, expected] of refusals) {
      const result = await imap.command(command);
      expect({ command, lines: result.lines }).toEqual({ command, lines: [`${result.tag} ${expected}`] });
    }
    // The connection is still usable after all of those.
    expect((await imap.command("NOOP")).status).toBe("OK");
    imap.close();
  });

  test("APPEND takes its literal, then refuses", async () => {
    const imap = await ImapClient.connect(server);
    const message = "Subject: x\r\n\r\nbody\r\n";
    const sync = await imap.command(["APPEND INBOX ", lit(message)]);
    expect(sync.lines).toEqual(["+ go ahead", "A1 NO Not authenticated."]);
    const nonSync = await imap.command(["APPEND INBOX ", lit(message, false)]);
    expect(nonSync.lines).toEqual(["A2 NO Not authenticated."]);
    imap.close();
  });

  test("DONE outside IDLE is not a command", async () => {
    const imap = await ImapClient.connect(server);
    expect((await imap.command("DONE")).lines).toEqual(["A1 BAD Unknown command"]);
    imap.close();
  });
});

describe("unknown commands and malformed command lines", () => {
  test("an unknown command is BAD and names the command in upper case", async () => {
    const imap = await ImapClient.connect(server);
    expect((await imap.command("FOO")).lines).toEqual(["A1 BAD Unknown command: FOO"]);
    expect((await imap.command("xyzzy with args")).lines).toEqual(["A2 BAD Unknown command: XYZZY"]);
    expect((await imap.command("X-CUSTOM")).lines).toEqual(["A3 BAD Unknown command: X-CUSTOM"]);
    imap.close();
  });

  test("an unknown command name is echoed whole, however long", async () => {
    const imap = await ImapClient.connect(server);
    const name = "X".repeat(300);
    expect((await imap.command(name)).lines).toEqual([`A1 BAD Unknown command: ${name}`]);
    imap.close();
  });

  test("a command name with an invalid UTF-8 octet is echoed with U+FFFD in its place", async () => {
    const imap = await ImapClient.connect(server);
    const result = await imap.command("BOGUS\xe9");
    expect(result.lines).toEqual(["A1 BAD Unknown command: BOGUS\xef\xbf\xbd"]);
    imap.close();
  });

  test("UID with no or an unsupported subcommand is BAD", async () => {
    const imap = await ImapClient.connect(server);
    expect((await imap.command("UID")).lines).toEqual(["A1 BAD Invalid UID subcommand"]);
    expect((await imap.command("UID NOOP")).lines).toEqual(["A2 BAD UID NOOP not supported"]);
    expect((await imap.command("UID EXPUNGE 1")).lines).toEqual(["A3 BAD UID EXPUNGE not supported"]);
    expect((await imap.command("UID CAPABILITY")).lines).toEqual(["A4 BAD UID CAPABILITY not supported"]);
    expect((await imap.command("UID BOGUS 1")).lines).toEqual(["A5 BAD Unknown command: BOGUS"]);
    imap.close();
  });

  test("command names are matched in any letter case", async () => {
    const imap = await ImapClient.connect(server);
    expect((await imap.command("nOoP")).lines).toEqual(["A1 OK NOOP completed"]);
    expect((await imap.command("capability")).status).toBe("OK");
    expect((await imap.command("Namespace")).status).toBe("OK");
    expect((await imap.command(`login admin ${ADMIN_PASSWORD}`)).status).toBe("OK");
    expect((await imap.command("select inbox")).tagged).toBe("A5 OK [READ-WRITE] SELECT completed");
    expect((await imap.command("uid fetch 1:* flags")).tagged).toBe("A6 OK FETCH completed");
    imap.close();
  });

  const syntaxErrors: [string, string][] = [
    ["LOGIN", "BAD Invalid username in LOGIN"],
    ["LOGIN onlyuser", "BAD Invalid password in LOGIN"],
    ['LOGIN "unterminated', "BAD Invalid username in LOGIN"],
    ["AUTHENTICATE", "BAD Invalid mechanism in AUTHENTICATE"],
    ["SELECT", "BAD Invalid mailbox name"],
    ["EXAMINE", "BAD Invalid mailbox name"],
    ["CREATE", "BAD Invalid mailbox name in CREATE"],
    ["DELETE", "BAD Invalid mailbox name in DELETE"],
    ["RENAME", "BAD Invalid existing mailbox name in RENAME"],
    ["RENAME a", "BAD Invalid new mailbox name in RENAME"],
    ["SUBSCRIBE", "BAD Invalid mailbox name in SUBSCRIBE"],
    ["UNSUBSCRIBE", "BAD Invalid mailbox name in UNSUBSCRIBE"],
    ["LIST", "BAD Invalid reference name in LIST"],
    ['LIST ""', "BAD Invalid mailbox name in LIST"],
    ["LSUB", "BAD Invalid reference name in LIST"],
    ["STATUS", "BAD Invalid mailbox name in STATUS"],
    ["STATUS INBOX", "BAD Expected opening parenthesis in STATUS"],
    ["STATUS INBOX (BOGUS)", "BAD Unknown status item: BOGUS"],
    ["FETCH", "BAD Invalid sequence set in FETCH"],
    ["FETCH 1", "BAD Invalid data items in FETCH"],
    ["FETCH 1 (BOGUS)", "BAD Invalid data items in FETCH"],
    ["FETCH abc FLAGS", "BAD Invalid sequence set in FETCH"],
    ["FETCH 1: FLAGS", "BAD Invalid sequence set in FETCH"],
    ["UID FETCH", "BAD Invalid sequence set in FETCH"],
    ["STORE", "BAD Invalid sequence set in STORE"],
    ["STORE 1", "BAD Invalid item name in STORE"],
    ["STORE 1 BOGUS (\\Seen)", "BAD Invalid store operation: BOGUS"],
    ["STORE 1 +FLAGS", "BAD Invalid flag in STORE"],
    ["COPY", "BAD Invalid sequence set in COPY"],
    ["COPY 1", "BAD Invalid mailbox name in COPY"],
    ["MOVE", "BAD Invalid sequence set in MOVE"],
    ["MOVE 1", "BAD Invalid mailbox name in MOVE"],
    ["APPEND", "BAD Expected mailbox name"],
    ["APPEND INBOX", "BAD Expected literal"],
    ["APPEND INBOX (\\Seen)", "BAD Expected literal"],
  ];

  test("each command reports its own syntax error, before any state check", async () => {
    const imap = await ImapClient.connect(server);
    for (const [command, expected] of syntaxErrors) {
      const result = await imap.command(command);
      expect({ command, lines: result.lines }).toEqual({ command, lines: [`${result.tag} ${expected}`] });
    }
    imap.close();
  });
});

describe("tags", () => {
  test("a tag may be digits, or carry punctuation that is not an atom-special", async () => {
    const imap = await ImapClient.connect(server);
    expect(await exchange(imap, "1 NOOP\r\n")).toBe("1 OK NOOP completed");
    expect(await exchange(imap, "a.b-c_1 NOOP\r\n")).toBe("a.b-c_1 OK NOOP completed");
    expect(await exchange(imap, "TAG:9/=! NOOP\r\n")).toBe("TAG:9/=! OK NOOP completed");
    imap.close();
  });

  test("the same tag may be reused", async () => {
    const imap = await ImapClient.connect(server);
    expect(await exchange(imap, "A1 NOOP\r\n")).toBe("A1 OK NOOP completed");
    expect(await exchange(imap, "A1 NOOP\r\n")).toBe("A1 OK NOOP completed");
    imap.close();
  });

  test("a tag of 128 octets is echoed; one of 129 is answered untagged and the command is not run", async () => {
    const imap = await ImapClient.connect(server);
    const longest = "T".repeat(128);
    expect(await exchange(imap, `${longest} NOOP\r\n`)).toBe(`${longest} OK NOOP completed`);
    expect(await exchange(imap, `${"T".repeat(129)} NOOP\r\n`)).toBe("* BAD Command tag too long");
    // The parse error of a line with an over-long tag is untagged too.
    expect(await exchange(imap, `${"T".repeat(129)} BOGUS\r\n`)).toBe("* BAD Unknown command: BOGUS");
    imap.close();
  });

  test("a tag containing + or a non-ASCII octet is answered untagged", async () => {
    const imap = await ImapClient.connect(server);
    expect(await exchange(imap, "A+1 NOOP\r\n")).toBe("* BAD Command tag too long");
    expect(await exchange(imap, "A\xe9 NOOP\r\n")).toBe("* BAD Command tag too long");
    imap.close();
  });

  test("a line that starts with * or a quote has no tag", async () => {
    const imap = await ImapClient.connect(server);
    expect(await exchange(imap, "* NOOP\r\n")).toBe("* BAD Invalid tag");
    expect(await exchange(imap, '"quoted" NOOP\r\n')).toBe("* BAD Invalid tag");
    imap.close();
  });

  test("a tag with no command is BAD under that tag", async () => {
    const imap = await ImapClient.connect(server);
    expect(await exchange(imap, "A1\r\n")).toBe("A1 BAD Invalid command");
    expect(await exchange(imap, "A2 \r\n")).toBe("A2 BAD Invalid command");
    expect(await exchange(imap, 'A3 "NOOP"\r\n')).toBe("A3 BAD Invalid command");
    imap.close();
  });

  test("leading spaces are skipped, so a lone command name is read as a tag", async () => {
    const imap = await ImapClient.connect(server);
    expect(await exchange(imap, " NOOP\r\n")).toBe("NOOP BAD Invalid command");
    expect(await exchange(imap, "  A1 NOOP\r\n")).toBe("A1 OK NOOP completed");
    imap.close();
  });

  test("more than one space between tag and command is accepted", async () => {
    const imap = await ImapClient.connect(server);
    expect(await exchange(imap, "A1   NOOP\r\n")).toBe("A1 OK NOOP completed");
    imap.close();
  });
});

describe("line framing and pipelining", () => {
  test("empty and blank lines are ignored", async () => {
    const imap = await ImapClient.connect(server);
    imap.write("\r\n   \r\n\r\nA1 NOOP\r\n");
    expect((await imap.next()).raw).toBe("A1 OK NOOP completed");
    imap.close();
  });

  test("pipelined commands are answered in order", async () => {
    const imap = await ImapClient.connect(server);
    imap.write("P1 NOOP\r\nP2 CAPABILITY\r\nP3 BOGUS\r\nP4 NOOP\r\n");
    const result = await imap.readResult("P4");
    expect(result.lines).toEqual([
      "P1 OK NOOP completed",
      `* CAPABILITY ${CAPABILITIES}`,
      "P2 OK CAPABILITY completed",
      "P3 BAD Unknown command: BOGUS",
      "P4 OK NOOP completed",
    ]);
    imap.close();
  });

  test("a command split across TCP segments is assembled", async () => {
    const imap = await ImapClient.connect(server);
    imap.write("S1 NO");
    await sleep(50);
    imap.write("OP\r");
    await sleep(50);
    imap.write("\n");
    expect((await imap.next()).raw).toBe("S1 OK NOOP completed");
    imap.close();
  });

  test("a bare LF does not end a line: the next line is swallowed into the first command", async () => {
    // NOTE: only CRLF terminates a command, so "A1 CAPABILITY\nA2 NOOP\r\n" is
    // one line. CAPABILITY ignores its arguments, and A2 is never answered.
    const imap = await ImapClient.connect(server);
    imap.write("A1 CAPABILITY\n");
    imap.write("A2 NOOP\r\nA3 NOOP\r\n");
    const result = await imap.readResult("A3");
    expect(result.lines).toEqual([
      `* CAPABILITY ${CAPABILITIES}`,
      "A1 OK CAPABILITY completed",
      "A3 OK NOOP completed",
    ]);
    imap.close();
  });

  test("arguments after a command that takes none are ignored", async () => {
    const imap = await ImapClient.connect(server);
    expect((await imap.command("NOOP extra args here")).lines).toEqual(["A1 OK NOOP completed"]);
    expect((await imap.command("CAPABILITY x")).status).toBe("OK");
    expect((await imap.command("CHECK (1 2 3)")).lines).toEqual(["A3 OK CHECK completed"]);
    imap.close();
  });

  test("a 60 KB command line is processed", async () => {
    const imap = await ImapClient.connect(server);
    imap.write(`B1 NOOP ${"x".repeat(60_000)}\r\nB2 NOOP\r\n`);
    const result = await imap.readResult("B2");
    expect(result.lines).toEqual(["B1 OK NOOP completed", "B2 OK NOOP completed"]);
    imap.close();
  });

  test("a 300 KB command line is never answered: the server stops reading and keeps the connection", async () => {
    // NOTE: past 64 KiB of unread command text the server pauses the socket
    // rather than answering BAD or closing; the line's own CRLF is then never
    // read, so the client hangs until the 5-minute inactivity timeout.
    const imap = await ImapClient.connect(server);
    imap.write(`B1 NOOP ${"x".repeat(300_000)}\r\nB2 NOOP\r\n`);
    await sleep(1500);
    expect(raws(imap.drain())).toEqual([]);
    expect(imap.closed).toBe(false);
    imap.close();

    // Other connections are unaffected.
    const other = await ImapClient.connect(server);
    expect((await other.command("NOOP")).status).toBe("OK");
    other.close();
  });

  test("more than 100 commands in a second are delayed, not dropped", async () => {
    const imap = await ImapClient.connect(server);
    const started = Date.now();
    let burst = "";
    for (let i = 1; i <= 120; i++) burst += `N${i} NOOP\r\n`;
    imap.write(burst);
    const first = await imap.readResult("N100");
    expect(first.lines).toHaveLength(100);
    const rest = await imap.readResult("N120");
    expect(rest.lines).toEqual(Array.from({ length: 20 }, (_, i) => `N${101 + i} OK NOOP completed`));
    // The 101st command waits for the one-second window to free a slot.
    expect(Date.now() - started).toBeGreaterThanOrEqual(900);
    imap.close();
  });
});

describe("literals", () => {
  test("a synchronizing literal is granted with '+ go ahead' on any command", async () => {
    const imap = await ImapClient.connect(server);
    const result = await imap.command(["NOOP ", lit("abc")]);
    expect(result.lines).toEqual(["+ go ahead", "A1 OK NOOP completed"]);
    imap.close();
  });

  test("a non-synchronizing literal draws no continuation", async () => {
    const imap = await ImapClient.connect(server);
    const result = await imap.command(["NOOP ", lit("abc", false)]);
    expect(result.lines).toEqual(["A1 OK NOOP completed"]);
    imap.close();
  });

  test("LOGIN takes both credentials as synchronizing literals", async () => {
    const imap = await ImapClient.connect(server);
    const result = await imap.command(["LOGIN ", lit("admin"), " ", lit(ADMIN_PASSWORD)]);
    expect(result.lines).toEqual(["+ go ahead", "+ go ahead", `A1 OK [CAPABILITY ${CAPABILITIES}] LOGIN completed`]);
    imap.close();
  });

  test("LOGIN takes both credentials as non-synchronizing literals", async () => {
    const imap = await ImapClient.connect(server);
    const result = await imap.command(["LOGIN ", lit("admin", false), " ", lit(ADMIN_PASSWORD, false)]);
    expect(result.lines).toEqual([`A1 OK [CAPABILITY ${CAPABILITIES}] LOGIN completed`]);
    imap.close();
  });

  test("a literal argument of 8192 octets is accepted, one of 8193 is refused with TOOBIG and no continuation", async () => {
    const imap = await ImapClient.connect(server);
    const accepted = await imap.command(["APPEND INBOX ", lit("x".repeat(8192))]);
    expect(accepted.lines).toEqual(["+ go ahead", "A1 NO Not authenticated."]);

    const refused = await imap.command(["LOGIN ", lit("x".repeat(8193))]);
    expect(refused.lines).toEqual(["A2 NO [TOOBIG] Literal exceeds 8192 octets"]);
    // Nothing is pending: the next command is read as a command.
    expect((await imap.command("NOOP")).lines).toEqual(["A3 OK NOOP completed"]);
    imap.close();
  });

  test("before login an APPEND message is held to the 8192-octet argument cap too", async () => {
    const imap = await ImapClient.connect(server);
    const refused = await imap.command(["APPEND INBOX ", lit("x".repeat(8193))]);
    expect(refused.lines).toEqual(["A1 NO [TOOBIG] Literal exceeds 8192 octets"]);
    imap.close();
  });

  test("an oversized non-synchronizing literal is refused and its octets are discarded", async () => {
    const imap = await ImapClient.connect(server);
    imap.write(`L1 LOGIN {9000+}\r\n${"x".repeat(9000)} password\r\nL2 NOOP\r\n`);
    const result = await imap.readResult("L2");
    expect(result.lines).toEqual(["L1 NO [TOOBIG] Literal exceeds 8192 octets", "L2 OK NOOP completed"]);
    imap.close();
  });

  test("after an oversized LITERAL+ only the rest of that line is dropped: a chained literal's payload is read as a command", async () => {
    // NOTE: the discard ends at the first CRLF after the oversized payload, so
    // the octets of a second literal on the same command ("abc") are parsed as
    // a command line and echoed back as a tag.
    const imap = await ImapClient.connect(server);
    imap.write(`L1 LOGIN {9000+}\r\n${"x".repeat(9000)} {3+}\r\nabc\r\nL2 NOOP\r\n`);
    const result = await imap.readResult("L2");
    expect(result.lines).toEqual([
      "L1 NO [TOOBIG] Literal exceeds 8192 octets",
      "abc BAD Invalid command",
      "L2 OK NOOP completed",
    ]);
    imap.close();
  });

  test("a non-synchronizing literal past the command ceiling ends the session with BYE", async () => {
    const imap = await ImapClient.connect(server);
    imap.write("L1 LOGIN {40000000+}\r\n");
    const rest = await imap.waitForClose();
    expect(raws(rest)).toEqual(["* BYE Command too long"]);
  });

  test("a command chaining more than 64 literals ends the session with BYE", async () => {
    const imap = await ImapClient.connect(server);
    let chain = "C1 LOGIN {1+}\r\na";
    for (let i = 0; i < 70; i++) chain += " {1+}\r\na";
    imap.write(`${chain}\r\n`);
    const rest = await imap.waitForClose();
    expect(raws(rest)).toEqual(["* BYE Command too long"]);
  });

  test("an empty literal is a complete argument", async () => {
    const imap = await ImapClient.connect(server);
    // LOGIN with an empty user and password: both literals are zero octets.
    const result = await imap.command(["LOGIN ", lit(""), " ", lit("")]);
    expect(result.lines).toEqual(["+ go ahead", "+ go ahead", "A1 NO [AUTHENTICATIONFAILED] Invalid credentials."]);
    imap.close();
  });
});

describe("connection teardown", () => {
  test("a client that disconnects mid-command leaves the server serving others", async () => {
    const half = await ImapClient.connect(server);
    half.write("A1 LOGIN {5}\r\n");
    expect(await half.continuation()).toBe("+ go ahead");
    half.close();

    const partial = await ImapClient.connect(server);
    partial.write("A1 NOO");
    partial.close();

    const imap = await ImapClient.connect(server);
    expect((await imap.command("NOOP")).status).toBe("OK");
    imap.close();
  });

  test("many connections can be open at once", async () => {
    const clients = await Promise.all(Array.from({ length: 20 }, () => ImapClient.connect(server)));
    const answers = await Promise.all(clients.map((client) => client.command("NOOP")));
    expect(answers.map((answer) => answer.status)).toEqual(Array(20).fill("OK"));
    for (const client of clients) client.close();
  });

  test("the port serves 100 connections at once; the 101st is closed without a greeting", async () => {
    const limited = await startImapServer();
    try {
      const clients: ImapClient[] = [];
      for (let i = 0; i < 100; i++) clients.push(await ImapClient.connect(limited));
      const extra = await ImapClient.connect(limited, { greeting: false });
      expect(raws(await extra.waitForClose())).toEqual([]);
      expect(extra.transcript).toBe("");
      // The hundred that got in are served normally.
      expect((await clients[99]!.command("NOOP")).status).toBe("OK");

      // When one leaves, the next connection is greeted.
      clients[0]!.close();
      const next = await waitFor(
        async () => {
          const candidate = await ImapClient.connect(limited, { greeting: false });
          try {
            return (await candidate.next(2000)).raw === GREETING ? candidate : undefined;
          } catch {
            return undefined;
          }
        },
        { what: "a freed connection slot" }
      );
      next.close();
      for (const client of clients) client.close();
    } finally {
      await limited.stop();
    }
  }, 60_000);

  test("on shutdown every connection is told BYE and closed, idle or not", async () => {
    const doomed = await startImapServer();
    try {
      const fresh = await ImapClient.connect(doomed);
      const selected = await ImapClient.connect(doomed);
      await selected.login("admin", ADMIN_PASSWORD);
      await selected.command("SELECT INBOX");
      const idling = await ImapClient.connect(doomed);
      await idling.login("admin", ADMIN_PASSWORD);
      await idling.command("SELECT INBOX");
      await idling.idle();

      const exited = doomed.kill("SIGTERM");
      for (const client of [fresh, selected, idling]) {
        expect(raws(await client.waitForClose())).toEqual(["* BYE Server shutting down"]);
      }
      expect(await exited).toBe(0);
    } finally {
      await doomed.stop();
    }
  }, 60_000);
});
