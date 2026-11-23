/**
 * The scripted client conversations `record.ts` runs against the Bun SMTP
 * server, and the inputs of the two pure-function tables.
 *
 * A conversation is a server configuration and a list of steps. Strings to
 * send are "binary": one character per byte, so `\xff` is the byte 0xFF and
 * text outside Latin-1 goes through `utf8()` first.
 *
 * The recording hooks decide by what they are called with (see `record.ts`):
 * the local part of the sender or recipient, or the AUTH username, names the
 * outcome — `refused@`, `slow@`, `nobody@`, `toobig@` and so on.
 */

export interface ServerSpec {
  /** Implicit TLS. */
  secure?: boolean;
  /** The listener has a certificate: STARTTLS is offered. */
  tls?: boolean;
  proxyProtocol?: boolean;
  allowInsecureAuth?: boolean;
  /** `null` leaves the name to the host name. */
  name?: string | null;
  size?: number;
  maxClients?: number;
  closeTimeout?: number;
  socketTimeout?: number | null;
}

/** `on` names the connection a step is for; `c` when left out. */
export type Step =
  | { open: string }
  | { send: string; times?: number; on?: string }
  /** Waits for one complete reply and records it. */
  | { reply: true; within?: number; on?: string }
  /** Waits for this many replies and records their count and digest. */
  | { replies: number; within?: number; on?: string }
  /** Waits for the server to close its side; records whether it did. */
  | { closed: true; within?: number; on?: string }
  /** Records that nothing arrived for this many milliseconds. */
  | { silence: number; on?: string }
  /** Runs the client side of a TLS handshake on the connection. */
  | { tls: true; on?: string }
  /** Half-closes: the client's FIN. */
  | { end: true; on?: string }
  | { destroy: true; on?: string }
  | { wait: number }
  /** Calls `evictSmtpConnections` and records what it returns. */
  | { evict: string }
  /** `close` starts the listener's shutdown; `closed` records whether it has finished. */
  | { server: "close" }
  | { server: "closed"; within?: number };

export interface Conversation {
  name: string;
  server?: ServerSpec;
  steps: Step[];
  /** Also record the failures the server reported. */
  errors?: boolean;
}

type Steps = (Step | Steps)[];

const flatten = (steps: Steps): Step[] => steps.flatMap((step) => (Array.isArray(step) ? flatten(step) : [step]));

const utf8 = (text: string) => Buffer.from(text, "utf8").toString("latin1");

const open = (name = "c"): Step => ({ open: name });
const send = (data: string, on?: string): Step => ({ send: data, ...(on ? { on } : {}) });
const reply = (on?: string): Step => ({ reply: true, ...(on ? { on } : {}) });
const closed = (on?: string): Step => ({ closed: true, ...(on ? { on } : {}) });
const silence = (ms: number, on?: string): Step => ({ silence: ms, ...(on ? { on } : {}) });
const wait = (ms: number): Step => ({ wait: ms });
const end = (on?: string): Step => ({ end: true, ...(on ? { on } : {}) });
const destroy = (on?: string): Step => ({ destroy: true, ...(on ? { on } : {}) });
const startTls = (on?: string): Step => ({ tls: true, ...(on ? { on } : {}) });

/** Sends one command line and waits for its reply. */
const cmd = (line: string, on?: string): Steps => [send(`${line}\r\n`, on), reply(on)];

/** Opens the connection and reads the greeting. */
const greeted = (name = "c"): Steps => [open(name), reply(name)];

const EHLO = "EHLO client.test";
const hello = (name = "c"): Steps => [greeted(name), cmd(EHLO, name)];

/** Greeting, EHLO, one accepted sender and recipient. */
const envelope = (from = "a@x.org", to = "b@y.org"): Steps => [
  hello(),
  cmd(`MAIL FROM:<${from}>`),
  cmd(`RCPT TO:<${to}>`)
];

/** An envelope with DATA accepted: the next bytes are the message. */
const inData = (from = "a@x.org", to = "b@y.org"): Steps => [envelope(from, to), cmd("DATA")];

const MESSAGE = "Subject: hi\r\n\r\nbody\r\n";
const plain = (user: string, pass: string, authorization = "") =>
  Buffer.from(`${authorization}\0${user}\0${pass}`).toString("base64");
const b64 = (text: string) => Buffer.from(text, "latin1").toString("base64");

const conversation = (name: string, steps: Steps, server?: ServerSpec, extra?: Partial<Conversation>): Conversation => ({
  name,
  ...(server ? { server } : {}),
  steps: flatten(steps),
  ...extra
});

const TLS: ServerSpec = { tls: true };
const AUTH: ServerSpec = { allowInsecureAuth: true };
const PROXY: ServerSpec = { proxyProtocol: true };
const IMPLICIT: ServerSpec = { tls: true, secure: true };
const PROXY_V4 = "PROXY TCP4 203.0.113.7 10.0.0.2 51000 25\r\n";

export const conversations: Conversation[] = [
  // ── Greeting, EHLO, HELO ────────────────────────────────────────────────
  conversation("greeting and EHLO", [hello(), cmd("QUIT"), closed()]),
  conversation("greeting names the host when the listener has no name", [hello(), cmd("HELO client.test")], {
    name: null
  }),
  conversation("EHLO spellings", [
    greeted(),
    cmd("ehlo lower.test"),
    cmd("EhLo   Mixed.TEST  "),
    cmd("EHLO"),
    cmd("EHLO "),
    cmd("EHLO a b"),
    cmd("EHLO\tclient.test"),
    cmd(" EHLO client.test"),
    cmd(`EHLO ${utf8("BÜCHER.example")}`),
    cmd(`EHLO a${utf8("\u00a0")}b`),
    cmd("EHLO [192.0.2.1]"),
    cmd("LHLO client.test"),
    cmd("MAIL FROM:<a@x.org>")
  ]),
  conversation("EHLO lowercases the name the session carries", [
    greeted(),
    cmd("EHLO Client.EXAMPLE"),
    cmd("MAIL FROM:<a@x.org>"),
    cmd("HELO Other.EXAMPLE"),
    cmd("MAIL FROM:<a@x.org>")
  ]),
  conversation("HELO spellings", [
    greeted(),
    cmd("HELO"),
    cmd("HELO a b"),
    cmd("helo Client.TEST"),
    cmd("HELO client.test extra"),
    cmd("HELO\tclient.test"),
    cmd("HELO client.test")
  ]),
  conversation("EHLO forgets the envelope", [
    envelope(),
    cmd(EHLO),
    cmd("RCPT TO:<b@y.org>"),
    cmd("MAIL FROM:<a@x.org>"),
    cmd("HELO client.test"),
    cmd("DATA"),
    cmd("RCPT TO:<b@y.org>")
  ]),
  conversation("a failed EHLO keeps the envelope", [envelope(), cmd("EHLO"), cmd("HELO a b"), cmd("RCPT TO:<c@y.org>")]),
  conversation("commands before HELO", [
    greeted(),
    cmd("MAIL FROM:<a@x.org>"),
    cmd("RCPT TO:<b@y.org>"),
    cmd("DATA"),
    cmd("AUTH PLAIN"),
    cmd("mail from:<a@x.org>"),
    cmd("NOOP"),
    cmd("RSET"),
    cmd("VRFY someone"),
    cmd("HELP"),
    cmd("STARTTLS"),
    cmd("QUIT"),
    closed()
  ]),
  conversation("talking before the greeting", [open(), send("EHLO early.test\r\n"), reply(), closed()]),
  conversation("talking before the greeting, several lines", [
    open(),
    send("EHLO early.test\r\nMAIL FROM:<a@x.org>\r\nQUIT\r\n"),
    reply(),
    closed()
  ]),
  conversation("half a line before the greeting is not talking", [
    open(),
    send("EHLO ear"),
    reply(),
    send("ly.test\r\n"),
    reply()
  ]),
  conversation("a blank line before the greeting is talking", [open(), send("\r\n"), reply(), closed()]),
  conversation("commands after a refusal are read and dropped", [
    open(),
    send("EHLO early.test\r\n"),
    reply(),
    closed(),
    wait(200),
    send("EHLO late.test\r\nMAIL FROM:<late@x.org>\r\nRCPT TO:<b@y.org>\r\n"),
    silence(200)
  ]),

  // ── Things that are not SMTP ────────────────────────────────────────────
  conversation("an HTTP request", [greeted(), cmd("GET / HTTP/1.1"), closed()]),
  conversation("an HTTP request in lower case", [greeted(), cmd("post /submit http/1.0"), closed()]),
  conversation("an HTTP request with a long path", [greeted(), cmd("OPTIONS /a/b?c=d e f HTTP/2.0"), closed()]),
  conversation("lines that only resemble HTTP", [
    greeted(),
    cmd("GET /HTTP/1.1"),
    cmd("GET / HTTP/1.1 "),
    cmd("GET / HTTP/11"),
    cmd("PATCH / HTTP/1.1"),
    cmd("GET  / HTTP/1.1"),
    cmd(" GET / HTTP/1.1"),
    cmd("GET / HTTP/1.1\r"),
    cmd("NOOP")
  ]),
  conversation("an HTTP request where an AUTH token was due", [
    hello(),
    cmd("AUTH PLAIN"),
    cmd("DELETE /x HTTP/1.1"),
    closed()
  ], AUTH),
  conversation("ten unrecognized commands", [
    greeted(),
    ...Array.from({ length: 10 }, (_, i) => cmd(`XYZ${i}`)),
    closed()
  ]),
  conversation("unrecognized commands are counted across recognized ones", [
    hello(),
    cmd("FOO"),
    cmd("NOOP"),
    cmd(""),
    cmd("   "),
    cmd("RSET"),
    cmd("EXPN list"),
    cmd("TURN"),
    cmd("MAIL FROM:<a@x.org>"),
    cmd("SEND FROM:<a@x.org>"),
    cmd("X-ANYTHING"),
    cmd("BDAT 10 LAST"),
    cmd("ETRN x"),
    cmd("ATRN"),
    cmd("ONE MORE"),
    closed()
  ]),
  conversation("a finished transaction forgives unrecognized commands", [
    hello(),
    ...Array.from({ length: 9 }, (_, i) => cmd(`XYZ${i}`)),
    cmd("MAIL FROM:<a@x.org>"),
    cmd("RCPT TO:<b@y.org>"),
    cmd("DATA"),
    send(`${MESSAGE}.\r\n`),
    reply(),
    ...Array.from({ length: 9 }, (_, i) => cmd(`XYZ${i}`)),
    cmd("NOOP"),
    cmd("XYZ9"),
    closed()
  ]),
  conversation("odd bytes in a command", [
    greeted(),
    cmd("NO\x00OP"),
    cmd("\xff\xfe"),
    cmd("NOOP\r"),
    cmd("N\rOOP"),
    cmd(utf8("ΝΟΟΡ")),
    cmd(utf8("noop\u2028")),
    cmd("NOOP")
  ]),
  conversation("letters that upper-case into a command", [
    greeted(),
    cmd(utf8("ehlo \u0131.test")),
    cmd(utf8("ma\u0131l from:<a@x.org>")),
    cmd(utf8("r\u017fet")),
    cmd(utf8("qu\u0131t")),
    closed()
  ]),
  conversation("bare line feeds end a command", [greeted(), send("EHLO client.test\nNOOP\nRSET\n"), reply(), reply(), reply()]),
  conversation("a command line at the length limit", [
    greeted(),
    send(`NOOP ${"x".repeat(16 * 1024 - 5)}\r\n`),
    reply(),
    send(`NOOP ${"x".repeat(16 * 1024 - 4)}\r\n`),
    reply(),
    closed()
  ]),
  conversation("a command line past the limit with no end", [
    greeted(),
    send(`NOOP ${"x".repeat(20 * 1024)}`),
    reply(),
    closed()
  ]),
  conversation("a command line that never ends, sent slowly", [
    greeted(),
    send("x".repeat(9000)),
    silence(150),
    send("x".repeat(9000)),
    reply(),
    closed()
  ]),

  // ── The small commands ──────────────────────────────────────────────────
  conversation("NOOP, RSET, HELP and VRFY", [
    greeted(),
    cmd("NOOP"),
    cmd("noop"),
    cmd("NOOP with words"),
    cmd("RSET"),
    cmd("rset now"),
    cmd("HELP"),
    cmd("help MAIL"),
    cmd("VRFY"),
    cmd("vrfy postmaster@x.org"),
    cmd(EHLO),
    cmd("NOOP"),
    cmd("HELP"),
    cmd("VRFY a")
  ]),
  conversation("QUIT", [hello(), cmd("QUIT"), closed()]),
  conversation("QUIT in lower case with words", [greeted(), cmd("quit now please"), closed()]),
  conversation("QUIT in the middle of a transaction", [envelope(), cmd("QUIT"), closed()]),
  conversation("commands after QUIT are read and dropped", [
    hello(),
    send("QUIT\r\nMAIL FROM:<a@x.org>\r\nRCPT TO:<b@y.org>\r\nNOOP\r\n"),
    reply(),
    closed(),
    wait(150),
    send("MAIL FROM:<late@x.org>\r\nRCPT TO:<late@y.org>\r\n"),
    silence(200)
  ]),
  conversation("RSET forgets the sender", [
    hello(),
    cmd("MAIL FROM:<a@x.org>"),
    cmd("RSET"),
    cmd("RCPT TO:<b@y.org>"),
    cmd("MAIL FROM:<c@x.org>"),
    cmd("RCPT TO:<b@y.org>")
  ]),
  conversation("RSET forgets the recipients", [envelope(), cmd("RSET"), cmd("DATA"), cmd("MAIL FROM:<a@x.org>"), cmd("DATA")]),
  conversation("RSET forgets the body type", [
    hello(),
    cmd("MAIL FROM:<a@x.org> BODY=8BITMIME SMTPUTF8"),
    cmd("RSET"),
    cmd("MAIL FROM:<c@x.org>"),
    cmd("RCPT TO:<b@y.org>")
  ]),

  // ── MAIL FROM ───────────────────────────────────────────────────────────
  conversation("MAIL FROM spellings", [
    hello(),
    cmd("mail from:<a@x.org>"),
    cmd("RSET"),
    cmd("MAIL FROM: <a@x.org>"),
    cmd("RSET"),
    cmd("MAIL FROM :<a@x.org>"),
    cmd("RSET"),
    cmd("Mail From:\t<a@x.org>  "),
    cmd("RSET"),
    cmd("MAIL FROM:<>"),
    cmd("RSET"),
    cmd("MAIL FROM:<A.b+c@X.ORG>"),
    cmd("RCPT TO:<b@y.org>")
  ]),
  conversation("MAIL FROM syntax errors", [
    hello(),
    cmd("MAIL"),
    cmd("MAIL FROM"),
    cmd("MAIL FROM:"),
    cmd("MAIL FROM:a@x.org"),
    cmd("MAIL FROM:<a@x.org"),
    cmd("MAIL FROM:a@x.org>"),
    cmd("MAIL FROM:<a b@x.org>"),
    cmd("MAIL FROM:<a@x.org>SIZE=1"),
    cmd("MAIL FROM:<<a@x.org>>"),
    cmd("MAIL  FROM:<a@x.org>"),
    cmd("MAIL FROM <a@x.org>"),
    cmd("MAIL TO:<a@x.org>"),
    cmd("MAIL FROM:<a>"),
    cmd("MAIL FROM:<a@>"),
    cmd("MAIL FROM:<@x.org>"),
    cmd("MAIL FROM:<a@b@x.org>"),
    cmd("MAIL FROM:<.a@x.org>"),
    cmd("MAIL FROM:<a.@x.org>"),
    cmd("MAIL FROM:<a..b@x.org>"),
    cmd("MAIL FROM:<a@.x.org>"),
    cmd("MAIL FROM:<a@x.org.>"),
    cmd("MAIL FROM:<a@x..org>"),
    cmd("MAIL FROM:<a@-x.org>"),
    cmd("MAIL FROM:<a@x-.org>"),
    cmd("MAIL FROM:<a@x.-org>"),
    cmd("MAIL FROM:<a@x_y.org>"),
    cmd("MAIL FROM:<a@x.org:25>"),
    cmd("MAIL FROM:<a@[1.2.3]>"),
    cmd("MAIL FROM:<a@[1.2.3.256]>"),
    cmd("MAIL FROM:<a@[IPv6:1::2::3]>"),
    cmd("MAIL FROM:<a@[::1]>"),
    cmd("MAIL FROM:<a@[x.org]>"),
    cmd("MAIL FROM:<a@[>"),
    cmd(`MAIL FROM:<${"a".repeat(64)}@${"d".repeat(185)}.org>`),
    cmd("MAIL FROM:<a@x.org>")
  ]),
  conversation("MAIL FROM addresses that pass", [
    hello(),
    ...[
      "a@[192.0.2.1]",
      "a@[IPv6:2001:db8::1]",
      "a@[ipv6:::1]",
      "a@[IPV6:fe80::1%eth0]",
      "a@[IPv6:::ffff:192.0.2.1]",
      "a@xn--mnchen-3ya.de",
      "a@XN--MNCHEN-3YA.de",
      "a@xn--a.de",
      "a@sub.xn--bcher-kva.example",
      utf8("δοκιμή@παράδειγμα.δοκιμή"),
      utf8("a@bücher.example"),
      "\xff\xfe@x.org",
      "\"quoted\"@x.org",
      "a!#$%&'*+/=?^_`{|}~-b@x.org",
      "a@localhost",
      "a@1.2.3.4",
      "a@X-1.ORG",
      `${"a".repeat(64)}@${"d".repeat(184)}.org`
    ].flatMap((address) => [cmd(`MAIL FROM:<${address}>`), cmd("RSET")])
  ]),
  conversation("MAIL FROM parameters", [
    hello(),
    ...[
      "SIZE=1000",
      "SIZE=1048576",
      "size=12",
      "SIZE",
      "SIZE=",
      "SIZE=abc",
      "SIZE=-1",
      "SIZE=0",
      "SIZE=12.5",
      "SIZE=1048576.0",
      "BODY=7BIT",
      "BODY=8BITMIME",
      "body=8bitmime",
      "SMTPUTF8",
      "smtputf8 BODY=8bitMIME SIZE=5",
      "AUTH=<>",
      "FOO=bar BAZ",
      "RET=HDRS ENVID=QQ314159",
      "X=a+3Db+2Bc+41+zz+4",
      "X=1 X=2",
      "X=a=b=c",
      "=orphan",
      "A=1 =2 B"
    ].flatMap((parameters) => [cmd(`MAIL FROM:<a@x.org> ${parameters}`), cmd("RCPT TO:<b@y.org>"), cmd("RSET")])
  ]),
  conversation("MAIL FROM parameters that are refused", [
    hello(),
    cmd("MAIL FROM:<a@x.org> SIZE=1048577"),
    cmd("MAIL FROM:<a@x.org> SIZE=1e9"),
    cmd("MAIL FROM:<a@x.org> SIZE=0x200000"),
    cmd("MAIL FROM:<a@x.org> SIZE=Infinity"),
    cmd("MAIL FROM:<a@x.org> SIZE=99999999999999999999999"),
    cmd("MAIL FROM:<a@x.org> BODY=BINARYMIME"),
    cmd("MAIL FROM:<a@x.org> BODY"),
    cmd("MAIL FROM:<a@x.org> BODY=7bit BODY=other"),
    cmd("MAIL FROM:<a@x.org> SMTPUTF8=1"),
    cmd("MAIL FROM:<a@x.org> REQUIRETLS"),
    cmd("MAIL FROM:<a@x.org> REQUIRETLS=1"),
    cmd("MAIL FROM:<a@x.org> SIZE=1048577 BODY=BINARYMIME"),
    cmd("MAIL FROM:<a@x.org> BODY=BINARYMIME SMTPUTF8=1"),
    cmd("MAIL FROM:a@x.org SIZE=1048577"),
    cmd("MAIL FROM:<a@x.org> SIZE=1048576")
  ]),
  conversation("a small size ceiling", [
    hello(),
    cmd("MAIL FROM:<a@x.org> SIZE=101"),
    cmd("MAIL FROM:<a@x.org> SIZE=100"),
    cmd("RSET"),
    cmd("MAIL FROM:<a@x.org> SIZE")
  ], { size: 100 }),
  conversation("a size ceiling of zero", [hello(), cmd("MAIL FROM:<a@x.org> SIZE"), cmd("MAIL FROM:<a@x.org> SIZE=0")], {
    size: 0
  }),
  conversation("a second MAIL FROM", [
    hello(),
    cmd("MAIL FROM:<a@x.org>"),
    cmd("MAIL FROM:<c@x.org>"),
    cmd("MAIL FROM:<>"),
    cmd("MAIL FROM:c@x.org"),
    cmd("MAIL FROM:<c@x.org> SIZE=9999999"),
    cmd("RCPT TO:<b@y.org>")
  ]),
  conversation("a second MAIL FROM after the null sender", [hello(), cmd("MAIL FROM:<>"), cmd("MAIL FROM:<a@x.org>")]),
  conversation("MAIL FROM refused by the hook", [
    hello(),
    cmd("MAIL FROM:<refused@x.org>"),
    cmd("RCPT TO:<b@y.org>"),
    cmd("MAIL FROM:<nocode@x.org>"),
    cmd("MAIL FROM:<lookup@x.org>"),
    cmd("MAIL FROM:<a@x.org>"),
    cmd("RCPT TO:<b@y.org>")
  ]),
  conversation("a refused MAIL FROM leaves its parameters on the envelope", [
    hello(),
    cmd("MAIL FROM:<refused@x.org> BODY=8BITMIME SMTPUTF8"),
    cmd("MAIL FROM:<a@x.org>"),
    cmd("RCPT TO:<b@y.org>")
  ]),
  conversation("MAIL FROM answered 421 by the hook", [hello(), cmd("MAIL FROM:<gone@x.org>"), closed()]),
  conversation("a slow MAIL FROM hook holds the commands behind it", [
    hello(),
    send("MAIL FROM:<slow@x.org>\r\nRCPT TO:<b@y.org>\r\nNOOP\r\n"),
    silence(150),
    reply(),
    reply(),
    reply()
  ]),

  // ── RCPT TO ─────────────────────────────────────────────────────────────
  conversation("RCPT TO before MAIL FROM", [
    hello(),
    cmd("RCPT TO:<b@y.org>"),
    cmd("RCPT TO:<>"),
    cmd("RCPT TO:b@y.org"),
    cmd("rcpt to:<b@y.org>")
  ]),
  conversation("RCPT TO syntax errors", [
    hello(),
    cmd("MAIL FROM:<a@x.org>"),
    cmd("RCPT"),
    cmd("RCPT TO"),
    cmd("RCPT TO:"),
    cmd("RCPT TO:<>"),
    cmd("RCPT TO:b@y.org"),
    cmd("RCPT TO:<b@y.org"),
    cmd("RCPT TO:<b>"),
    cmd("RCPT TO:<postmaster>"),
    cmd("RCPT TO:<b@y..org>"),
    cmd("RCPT FROM:<b@y.org>"),
    cmd("RCPT  TO:<b@y.org>"),
    cmd("RCPT TO:<b@y.org>NOTIFY=NEVER"),
    cmd("RCPT TO:<@relay.example:b@y.org>"),
    cmd("RCPT TO:<b@y.org>")
  ]),
  conversation("RCPT TO spellings and parameters", [
    hello(),
    cmd("MAIL FROM:<a@x.org>"),
    cmd("rcpt to:<b@y.org>"),
    cmd("RCPT TO: <c@y.org>  "),
    cmd("RCPT TO:<d@y.org> NOTIFY=SUCCESS,FAILURE ORCPT=rfc822;d+40y.org"),
    cmd("RCPT TO:<e@[192.0.2.9]>"),
    cmd(`RCPT TO:<${utf8("ü@bücher.example")}>`),
    cmd("RCPT TO:<f@xn--bcher-kva.example> FLAG"),
    cmd("DATA"),
    send(`${MESSAGE}.\r\n`),
    reply()
  ]),
  conversation("a repeated recipient replaces the earlier one", [
    hello(),
    cmd("MAIL FROM:<a@x.org>"),
    cmd("RCPT TO:<b@y.org>"),
    cmd("RCPT TO:<c@y.org> NOTIFY=NEVER"),
    cmd("RCPT TO:<B@Y.ORG> NOTIFY=DELAY"),
    cmd(`RCPT TO:<${utf8("STRASSE@y.org")}>`),
    cmd(`RCPT TO:<${utf8("straße@y.org")}>`),
    cmd(`RCPT TO:<${utf8("ÀB@y.org")}>`),
    cmd(`RCPT TO:<${utf8("àb@y.org")}>`),
    cmd("DATA"),
    send(`${MESSAGE}.\r\n`),
    reply()
  ]),
  conversation("the recipient limit", [
    hello(),
    cmd("MAIL FROM:<a@x.org>"),
    send(Array.from({ length: 100 }, (_, i) => `RCPT TO:<r${i}@y.org>\r\n`).join("")),
    { replies: 100 },
    cmd("RCPT TO:<R0@Y.ORG>"),
    cmd("RCPT TO:<r100@y.org>"),
    cmd("RCPT TO:<nobody@y.org>"),
    cmd("RCPT TO:<r99@y.org> NOTIFY=NEVER"),
    cmd("RCPT TO:<>"),
    cmd("DATA"),
    send(`${MESSAGE}.\r\n`),
    reply()
  ]),
  conversation("RCPT TO refused by the hook", [
    hello(),
    cmd("MAIL FROM:<a@x.org>"),
    cmd("RCPT TO:<nobody@y.org>"),
    cmd("DATA"),
    cmd("RCPT TO:<nocode@y.org>"),
    cmd("RCPT TO:<probes@y.org>"),
    cmd("RCPT TO:<b@y.org>"),
    cmd("RCPT TO:<nobody@y.org>"),
    cmd("DATA"),
    send(`${MESSAGE}.\r\n`),
    reply()
  ]),
  conversation("RCPT TO answered 421 by the hook", [
    hello(),
    cmd("MAIL FROM:<a@x.org>"),
    cmd("RCPT TO:<gone@y.org>"),
    closed()
  ]),
  conversation("a slow RCPT TO hook holds the commands behind it", [
    hello(),
    send("MAIL FROM:<a@x.org>\r\nRCPT TO:<slow@y.org>\r\nRCPT TO:<b@y.org>\r\nDATA\r\n"),
    reply(),
    silence(150),
    reply(),
    reply(),
    reply(),
    send(`${MESSAGE}.\r\n`),
    reply()
  ]),

  // ── DATA ────────────────────────────────────────────────────────────────
  conversation("DATA out of sequence", [
    hello(),
    cmd("DATA"),
    cmd("MAIL FROM:<a@x.org>"),
    cmd("DATA"),
    cmd("data"),
    cmd("RCPT TO:<nobody@y.org>"),
    cmd("DATA")
  ]),
  conversation("a message", [inData(), send(`${MESSAGE}.\r\n`), reply(), cmd("QUIT"), closed()]),
  conversation("DATA in lower case with words", [envelope(), cmd("data please"), send(`${MESSAGE}.\r\n`), reply()]),
  conversation("an empty message", [inData(), send(".\r\n"), reply(), cmd("NOOP")]),
  conversation("a message that is one line ending", [inData(), send("\r\n.\r\n"), reply()]),
  conversation("dot-stuffed lines", [
    inData(),
    send("..leading dot\r\n.x single dot\r\n...\r\n.. \r\n..\r\nmid..dle\r\nlast\r\n.\r\n"),
    reply()
  ]),
  conversation("a dot-stuffed first line", [inData(), send("..first\r\n.\r\n"), reply()]),
  conversation("a lone dot as the first line", [inData(), send(".x\r\n.\r\n"), reply()]),
  conversation("the terminator split across writes", [
    inData(),
    ...["Subject: x\r\n\r\nbody\r", "\n", ".", "\r", "\n"].flatMap((piece) => [send(piece), wait(40)]),
    reply()
  ]),
  conversation("a stuffed dot split across writes", [
    inData(),
    ...["line\r\n", ".", ".stuffed\r\n.", "x\r\n.", "\r", "y\r\n", ".\r", "\n"].flatMap((piece) => [send(piece), wait(40)]),
    reply()
  ]),
  conversation("only CRLF.CRLF ends the message", [
    inData(),
    send(
      "Subject: x\r\n\r\nA\n.\r\nMAIL FROM:<smuggled@x.org>\r\n" +
        "B\r\n.\nRCPT TO:<smuggled@x.org>\r\nC\n.\nD\r.\rE\r\n" +
        ".\r\nNOOP\r\n"
    ),
    reply(),
    reply(),
    silence(150)
  ]),
  conversation("a dot after a bare line feed is unstuffed but ends nothing", [
    inData(),
    send("a\n..b\n.\r\nc\r..d\r\n.\r\n"),
    reply()
  ]),
  conversation("a message that does not end its last line", [inData(), send("no line ending.\r\n"), silence(150), send("\r\n.\r\n"), reply()]),
  conversation("commands inside a message are content", [
    inData(),
    send("QUIT\r\nRSET\r\nDATA\r\n.QUIT\r\nGET / HTTP/1.1\r\n\r\n.\r\n"),
    reply(),
    cmd("NOOP")
  ]),
  conversation("every byte value in a message", [
    inData(),
    send(Array.from({ length: 256 }, (_, i) => String.fromCharCode(i)).join("") + "\r\n.\r\n"),
    reply()
  ]),
  conversation("message bytes in the same write as DATA", [
    envelope(),
    send(`DATA\r\n${MESSAGE}.\r\nNOOP\r\n`),
    reply(),
    reply(),
    reply()
  ]),
  conversation("a whole session in one write", [
    greeted(),
    send(`EHLO client.test\r\nMAIL FROM:<a@x.org>\r\nRCPT TO:<b@y.org>\r\nRCPT TO:<c@y.org>\r\nDATA\r\n${MESSAGE}.\r\nQUIT\r\n`),
    reply(),
    reply(),
    reply(),
    reply(),
    reply(),
    reply(),
    reply(),
    closed()
  ]),
  conversation("two messages on one connection", [
    inData(),
    send("first\r\n.\r\n"),
    reply(),
    cmd("RCPT TO:<b@y.org>"),
    cmd("DATA"),
    cmd("MAIL FROM:<c@x.org> BODY=8BITMIME"),
    cmd("RCPT TO:<d@y.org>"),
    cmd("DATA"),
    send("second\r\n.\r\n"),
    reply(),
    cmd("MAIL FROM:<e@x.org>"),
    cmd("RCPT TO:<f@y.org>")
  ]),
  conversation("two messages in one write", [
    hello(),
    send(
      "MAIL FROM:<a@x.org>\r\nRCPT TO:<b@y.org>\r\nDATA\r\none\r\n.\r\n" +
        "MAIL FROM:<c@x.org>\r\nRCPT TO:<d@y.org>\r\nDATA\r\ntwo\r\n.\r\nQUIT\r\n"
    ),
    { replies: 9 },
    closed()
  ]),
  conversation("a large message", [
    inData(),
    { send: `${"x".repeat(998)}\r\n`, times: 400 },
    send(".\r\n"),
    reply()
  ]),
  conversation("a large message of stuffed dots", [
    inData(),
    { send: "..\r\n", times: 30000 },
    send(".\r\n"),
    reply()
  ]),
  conversation("DATA refused before the message is read", [
    inData("relay@x.org"),
    send(MESSAGE),
    silence(150),
    send(".\r\n"),
    reply(),
    cmd("MAIL FROM:<a@x.org>")
  ]),
  conversation("DATA refused without a code", [inData("unconfigured@x.org"), send(`${MESSAGE}.\r\n`), reply(), cmd("NOOP")]),
  conversation("DATA refused by a ceiling the hook keeps", [
    inData("toobig@x.org"),
    send("x".repeat(90)),
    wait(50),
    send("x".repeat(90)),
    silence(150),
    { send: `${"y".repeat(998)}\r\n`, times: 300 },
    send(".\r\n"),
    reply(),
    cmd("MAIL FROM:<a@x.org>")
  ]),
  conversation("DATA under the hook's ceiling", [inData("toobig@x.org"), send("small\r\n.\r\n"), reply()]),
  conversation("DATA refused after the message", [
    inData("notstored@x.org"),
    send(`${MESSAGE}.\r\n`),
    reply(),
    cmd("RCPT TO:<b@y.org>"),
    cmd("MAIL FROM:<parsefail@x.org>"),
    cmd("RCPT TO:<b@y.org>"),
    cmd("DATA"),
    send(`${MESSAGE}.\r\n`),
    reply()
  ]),
  conversation("DATA answered 421 by the hook", [inData("busy@x.org"), send(`${MESSAGE}.\r\nNOOP\r\n`), reply(), closed()]),
  conversation("a slow DATA hook holds the commands behind it", [
    inData("slowdata@x.org"),
    send(`${MESSAGE}.\r\nNOOP\r\nRSET\r\n`),
    silence(150),
    reply(),
    reply(),
    reply()
  ]),
  conversation("the client drops in the middle of a message", [
    inData(),
    send("Subject: partial\r\n\r\nsome of the bo"),
    wait(100),
    destroy(),
    wait(100)
  ]),
  conversation("the client half-closes in the middle of a message", [
    inData(),
    send("Subject: partial\r\n\r\nsome of the bo"),
    wait(100),
    end(),
    closed()
  ]),
  conversation("the client half-closes right after the message, slow hook", [
    inData("slowdata@x.org"),
    send(`${MESSAGE}.\r\nNOOP\r\n`),
    wait(100),
    end(),
    closed()
  ]),

  // ── AUTH ────────────────────────────────────────────────────────────────
  conversation("AUTH in cleartext is refused", [
    hello(),
    cmd(`AUTH PLAIN ${plain("user", "pass")}`),
    cmd("AUTH PLAIN"),
    cmd("AUTH LOGIN"),
    cmd("AUTH CRAM-MD5"),
    cmd("AUTH"),
    cmd("auth plain x y")
  ]),
  conversation("AUTH PLAIN", [
    hello(),
    cmd(`AUTH PLAIN ${plain("user", "wrong")}`),
    cmd(`AUTH PLAIN ${plain("nobody", "pass")}`),
    cmd(`AUTH PLAIN ${plain("user", "pass")}`),
    cmd(`AUTH PLAIN ${plain("user", "pass")}`),
    cmd("AUTH LOGIN"),
    cmd("AUTH BOGUS"),
    cmd(EHLO),
    cmd("MAIL FROM:<a@x.org>"),
    cmd("RCPT TO:<b@y.org>")
  ], AUTH),
  conversation("AUTH PLAIN in two steps", [
    hello(),
    cmd("AUTH PLAIN"),
    cmd(plain("user", "pass")),
    cmd("MAIL FROM:<a@x.org>")
  ], AUTH),
  conversation("AUTH PLAIN cancelled and malformed", [
    hello(),
    cmd("AUTH PLAIN"),
    cmd("*"),
    cmd("auth plain"),
    cmd("  *  "),
    cmd("AUTH PLAIN *"),
    cmd(`AUTH PLAIN ${b64("nonul")}`),
    cmd(`AUTH PLAIN ${b64("one\0two")}`),
    cmd(`AUTH PLAIN ${b64("a\0b\0c\0d")}`),
    cmd("AUTH PLAIN ="),
    cmd("AUTH PLAIN !!!!"),
    cmd(`AUTH PLAIN ${plain("user", "pass")} extra`),
    cmd("AUTH PLAIN"),
    cmd(""),
    cmd("AUTH PLAIN"),
    cmd("QUIT"),
    cmd("AUTH PLAIN"),
    cmd(b64("\0\0")),
    cmd(`AUTH PLAIN ${b64("\0user\0")}`),
    cmd("NOOP")
  ], AUTH),
  conversation("AUTH PLAIN identities", [
    hello(),
    cmd(`AUTH PLAIN ${plain("", "pass", "user")}`),
    cmd(`AUTH PLAIN ${plain("nobody", "pass", "user")}`),
    cmd(`AUTH PLAIN ${b64("\0\0pass")}`),
    cmd(`AUTH PLAIN ${b64(utf8("\0üser\0pässword"))}`),
    cmd(`AUTH PLAIN ${b64("\0\xff\xfe\0\xc3\x28")}`),
    cmd(`AUTH PLAIN ${plain("user", "pass", "admin")}`),
    cmd("MAIL FROM:<a@x.org>")
  ], AUTH),
  conversation("AUTH PLAIN with forgiving base64", [
    hello(),
    cmd(`AUTH PLAIN ${plain("user", "wrong").replace(/=+$/, "")}`),
    cmd(`AUTH PLAIN ${b64("\0??>\0>>?").replace(/\+/g, "-").replace(/\//g, "_")}`),
    cmd("AUTH PLAIN"),
    cmd(`  ${plain("nobody", "x").slice(0, 6)} \t ${plain("nobody", "x").slice(6)}  `),
    cmd(`AUTH PLAIN ${plain("nobody", "y")}=${plain("user", "pass")}`),
    cmd(`AUTH PLAIN ${plain("nobody", "z").split("").join("*")}`)
  ], AUTH),
  conversation("AUTH LOGIN", [
    hello(),
    cmd("AUTH LOGIN"),
    cmd(b64("user")),
    cmd(b64("wrong")),
    cmd("auth login"),
    cmd(b64("user")),
    cmd(b64("pass")),
    cmd("AUTH LOGIN"),
    cmd(EHLO)
  ], AUTH),
  conversation("AUTH LOGIN with the username up front", [
    hello(),
    cmd(`AUTH LOGIN ${b64("user")}`),
    cmd(b64("pass")),
    cmd("NOOP")
  ], AUTH),
  conversation("AUTH LOGIN cancelled and malformed", [
    hello(),
    cmd("AUTH LOGIN"),
    cmd("*"),
    cmd("AUTH LOGIN"),
    cmd(b64("user")),
    cmd(" * "),
    cmd("AUTH LOGIN *"),
    cmd("AUTH LOGIN ="),
    cmd("AUTH LOGIN"),
    cmd(""),
    cmd("AUTH LOGIN"),
    cmd("!!!"),
    cmd(`AUTH LOGIN ${b64("user")} ${b64("pass")}`),
    cmd(`AUTH LOGIN ${b64("user")}`),
    cmd(""),
    cmd("AUTH LOGIN"),
    cmd(b64("user")),
    cmd("NOOP"),
    cmd("NOOP")
  ], AUTH),
  conversation("AUTH with other mechanisms", [
    hello(),
    cmd("AUTH"),
    cmd("AUTH CRAM-MD5"),
    cmd("AUTH XOAUTH2 dG9rZW4="),
    cmd("AUTH PLAINTEXT"),
    cmd("AUTH  PLAIN  "),
    cmd("*")
  ], AUTH),
  conversation("AUTH answers of the hook", [
    hello(),
    cmd(`AUTH PLAIN ${plain("limited", "x")}`),
    cmd(`AUTH PLAIN ${plain("unavailable", "x")}`),
    cmd(`AUTH PLAIN ${plain("lookup", "x")}`),
    cmd(`AUTH PLAIN ${plain("custom", "x")}`),
    cmd(`AUTH PLAIN ${plain("blank", "x")}`),
    cmd(`AUTH PLAIN ${plain("alias", "x")}`),
    cmd("MAIL FROM:<a@x.org>"),
    cmd(`AUTH PLAIN ${plain("user", "pass")}`)
  ], AUTH),
  conversation("AUTH answered 421 by the hook", [hello(), cmd(`AUTH PLAIN ${plain("gone", "x")}`), closed()], AUTH),
  conversation("a slow AUTH hook holds the commands behind it", [
    hello(),
    send(`AUTH PLAIN ${plain("slow", "x")}\r\nMAIL FROM:<a@x.org>\r\nNOOP\r\n`),
    silence(150),
    reply(),
    reply(),
    reply()
  ], AUTH),
  conversation("AUTH in the middle of a transaction", [
    envelope(),
    cmd(`AUTH PLAIN ${plain("user", "pass")}`),
    cmd("RCPT TO:<c@y.org>"),
    cmd("DATA"),
    send(`${MESSAGE}.\r\n`),
    reply(),
    cmd("MAIL FROM:<a@x.org>")
  ], AUTH),
  conversation("what the hook stored outlives RSET and a transaction", [
    hello(),
    cmd(`AUTH PLAIN ${plain("user", "pass")}`),
    cmd("RSET"),
    cmd("MAIL FROM:<a@x.org>"),
    cmd("RCPT TO:<b@y.org>"),
    cmd("DATA"),
    send(`${MESSAGE}.\r\n`),
    reply(),
    cmd(EHLO),
    cmd("MAIL FROM:<a@x.org>")
  ], AUTH),

  // ── STARTTLS ────────────────────────────────────────────────────────────
  conversation("STARTTLS without a certificate", [hello(), cmd("STARTTLS"), cmd("starttls now"), cmd("NOOP")]),
  conversation("STARTTLS", [
    hello(),
    cmd("STARTTLS"),
    startTls(),
    cmd("MAIL FROM:<a@x.org>"),
    cmd(EHLO),
    cmd("STARTTLS"),
    cmd(`AUTH PLAIN ${plain("user", "pass")}`),
    cmd("MAIL FROM:<a@x.org> REQUIRETLS=1"),
    cmd("MAIL FROM:<a@x.org> REQUIRETLS"),
    cmd("RCPT TO:<b@y.org>"),
    cmd("DATA"),
    send(`${MESSAGE}.\r\n`),
    reply(),
    cmd("QUIT"),
    closed()
  ], TLS),
  conversation("STARTTLS before EHLO, in lower case with words", [
    greeted(),
    cmd("starttls please"),
    startTls(),
    cmd(EHLO)
  ], TLS),
  conversation("STARTTLS forgets what came before it", [
    hello(),
    cmd(`AUTH PLAIN ${plain("user", "pass")}`),
    cmd("MAIL FROM:<a@x.org> BODY=8BITMIME"),
    cmd("RCPT TO:<b@y.org>"),
    cmd("STARTTLS"),
    startTls(),
    cmd("RCPT TO:<b@y.org>"),
    cmd("NOOP"),
    cmd(EHLO),
    cmd("RCPT TO:<b@y.org>"),
    cmd("MAIL FROM:<a@x.org>"),
    cmd(`AUTH PLAIN ${plain("alias", "x")}`),
    cmd("RCPT TO:<b@y.org>")
  ], { tls: true, allowInsecureAuth: true }),
  conversation("cleartext pipelined behind STARTTLS is discarded", [
    hello(),
    send("STARTTLS\r\nMAIL FROM:<injected@x.org>\r\nRCPT TO:<b@y.org>\r\n"),
    reply(),
    startTls(),
    cmd(EHLO),
    cmd("RCPT TO:<b@y.org>"),
    cmd("MAIL FROM:<a@x.org>")
  ], TLS),
  conversation("cleartext sent behind STARTTLS in a write of its own, during a slow hook", [
    hello(),
    send("MAIL FROM:<slow@x.org>\r\n"),
    wait(60),
    send("STARTTLS\r\n"),
    wait(60),
    send("RCPT TO:<injected@y.org>\r\n"),
    reply(),
    reply(),
    // The server waits for a handshake: what it had taken in is not read.
    { closed: true, within: 600 }
  ], TLS, { errors: true }),
  conversation("cleartext sent behind STARTTLS in a write of its own", [
    hello(),
    send("STARTTLS\r\n"),
    reply(),
    send("RCPT TO:<injected@y.org>\r\n"),
    closed()
  ], TLS, { errors: true }),
  conversation("STARTTLS keeps the count of unrecognized commands", [
    greeted(),
    ...Array.from({ length: 8 }, (_, i) => cmd(`XYZ${i}`)),
    cmd("STARTTLS"),
    startTls(),
    cmd("XYZ8"),
    cmd("XYZ9"),
    closed()
  ], TLS),
  conversation("a STARTTLS handshake that is not TLS", [
    hello(),
    cmd("STARTTLS"),
    send("GET / HTTP/1.1\r\nHost: x\r\n\r\n"),
    closed()
  ], TLS, { errors: true }),
  conversation("the client leaves instead of starting TLS", [hello(), cmd("STARTTLS"), wait(50), end(), closed()], TLS, {
    errors: true
  }),
  conversation("the client leaves instead of starting TLS, in the middle of a transaction", [
    envelope(),
    cmd("STARTTLS"),
    wait(50),
    end(),
    closed()
  ], TLS, { errors: true }),

  // ── Implicit TLS ────────────────────────────────────────────────────────
  conversation("implicit TLS", [
    open(),
    startTls(),
    reply(),
    cmd(EHLO),
    cmd("STARTTLS"),
    cmd(`AUTH PLAIN ${plain("user", "pass")}`),
    cmd("MAIL FROM:<a@x.org> REQUIRETLS SMTPUTF8"),
    cmd("RCPT TO:<b@y.org>"),
    cmd("DATA"),
    send(`${MESSAGE}.\r\n`),
    reply(),
    cmd("QUIT"),
    closed()
  ], { tls: true, secure: true }),
  conversation("implicit TLS, a whole session in one write", [
    open(),
    startTls(),
    reply(),
    send(`EHLO client.test\r\nMAIL FROM:<a@x.org>\r\nRCPT TO:<b@y.org>\r\nDATA\r\n..dot\r\n${MESSAGE}.\r\nQUIT\r\n`),
    { replies: 6 },
    closed()
  ], { tls: true, secure: true }),
  conversation("cleartext sent to the implicit TLS port", [open(), send("EHLO client.test\r\n"), closed()], {
    tls: true,
    secure: true
  }, { errors: true }),
  conversation("an HTTP request sent to the implicit TLS port", [open(), send("GET / HTTP/1.1\r\nHost: x\r\n\r\n"), closed()], {
    tls: true,
    secure: true
  }, { errors: true }),
  conversation("a client that leaves the implicit TLS port without a handshake", [open(), wait(50), end(), closed()], {
    tls: true,
    secure: true
  }, { errors: true }),
  conversation("the idle timeout over TLS", [open(), startTls(), reply(), cmd(EHLO), reply(), closed()], {
    tls: true,
    secure: true,
    socketTimeout: 300
  }),

  // ── PROXY protocol ──────────────────────────────────────────────────────
  conversation("a PROXY header names the client", [
    open(),
    send(PROXY_V4),
    reply(),
    cmd(EHLO),
    cmd("MAIL FROM:<a@x.org>"),
    cmd("RCPT TO:<b@y.org>"),
    cmd("DATA"),
    send(`${MESSAGE}.\r\n`),
    reply()
  ], PROXY),
  conversation("a PROXY header for IPv6", [
    open(),
    send("PROXY TCP6 2001:DB8::7 fd00::2 51001 25\r\n"),
    reply(),
    cmd(EHLO),
    cmd("MAIL FROM:<a@x.org>")
  ], PROXY),
  conversation("a PROXY header with an IPv4-mapped client", [
    open(),
    send("PROXY TCP6 ::FFFF:198.51.100.4 fd00::2 4 25\r\n"),
    reply(),
    cmd(EHLO),
    cmd("MAIL FROM:<a@x.org>")
  ], PROXY),
  conversation("a PROXY header that names nobody", [
    open(),
    send("PROXY UNKNOWN\r\n"),
    reply(),
    cmd(EHLO),
    cmd("MAIL FROM:<a@x.org>")
  ], PROXY),
  conversation("a PROXY header split across writes", [
    open(),
    send("PROXY TCP4 203.0."),
    wait(40),
    send("113.7 10.0.0.2 51000 25\r"),
    wait(40),
    send("\n"),
    reply(),
    cmd(EHLO)
  ], PROXY),
  conversation("a PROXY header and talking too soon", [open(), send(`${PROXY_V4}EHLO early.test\r\n`), reply(), closed()], PROXY),
  conversation("no PROXY header", [open(), send("EHLO client.test\r\n"), closed()], PROXY, { errors: true }),
  conversation("a malformed PROXY header", [open(), send("PROXY TCP4 203.0.113.7 10.0.0.2 99999 25\r\n"), closed()], PROXY, {
    errors: true
  }),
  conversation("a PROXY header that is too long", [open(), send(`PROXY ${"x".repeat(120)}`), closed()], PROXY, {
    errors: true
  }),
  conversation("a PROXY header in the wrong case", [open(), send("proxy TCP4 203.0.113.7 10.0.0.2 51000 25\r\n"), closed()], PROXY),
  conversation("a PROXY header before implicit TLS", [
    open(),
    send("PROXY TCP6 2001:db8::7 fd00::2 51000 465\r\n"),
    startTls(),
    reply(),
    cmd(EHLO),
    cmd("MAIL FROM:<a@x.org>"),
    cmd("RCPT TO:<b@y.org>"),
    cmd("DATA"),
    send(`${MESSAGE}.\r\n`),
    reply()
  ], { tls: true, secure: true, proxyProtocol: true }),
  conversation("a PROXY header before STARTTLS", [
    open(),
    send("PROXY TCP4 203.0.113.7 10.0.0.2 51000 587\r\n"),
    reply(),
    cmd(EHLO),
    cmd("STARTTLS"),
    startTls(),
    cmd(EHLO),
    cmd("MAIL FROM:<a@x.org>"),
    cmd("RCPT TO:<b@y.org>"),
    cmd("DATA"),
    send(`${MESSAGE}.\r\n`),
    reply()
  ], { tls: true, proxyProtocol: true }),

  // ── Limits, timeouts, eviction, shutdown ────────────────────────────────
  conversation("one client too many", [
    greeted("first"),
    open("second"),
    reply("second"),
    closed("second"),
    cmd("NOOP", "first"),
    cmd("QUIT", "first"),
    closed("first"),
    greeted("third"),
    cmd("NOOP", "third")
  ], { maxClients: 1 }),
  conversation("a refused client's commands are read and dropped", [
    greeted("first"),
    open("second"),
    reply("second"),
    send("EHLO client.test\r\nMAIL FROM:<a@x.org>\r\n", "second"),
    silence(250, "second"),
    cmd("NOOP", "first")
  ], { maxClients: 1 }),
  conversation("the idle timeout", [greeted(), reply(), closed()], { socketTimeout: 300 }),
  conversation("the idle timeout starts over with each command", [
    greeted(),
    wait(200),
    cmd("NOOP"),
    wait(200),
    cmd("NOOP"),
    silence(200),
    reply(),
    closed()
  ], { socketTimeout: 300 }),
  conversation("the idle timeout while an AUTH token is due", [hello(), cmd("AUTH LOGIN"), reply(), closed()], {
    socketTimeout: 300,
    allowInsecureAuth: true
  }),
  conversation("the idle timeout in the middle of a message", [
    inData(),
    send("Subject: stalled\r\n\r\npart"),
    reply(),
    closed(),
    destroy(),
    wait(100)
  ], { socketTimeout: 300 }),
  conversation("the idle timeout while a hook reads nothing", [
    inData("ignore@x.org"),
    send(`${MESSAGE}.\r\n`),
    reply(),
    closed()
  ], { socketTimeout: 300 }),
  conversation("the idle timeout during a slow hook", [
    hello(),
    send("MAIL FROM:<slow@x.org>\r\nRCPT TO:<b@y.org>\r\n"),
    reply(),
    closed()
  ], { socketTimeout: 150 }),
  // Each write lands 100 ms before the timeout would have fired, and the
  // hook answers 100 ms before the timeout the last write set.
  conversation("what arrives during a slow hook holds off the idle timeout", [
    hello(),
    send("MAIL FROM:<slow@x.org>\r\n"),
    wait(100),
    send("NOOP\r\n"),
    wait(100),
    send("NOOP\r\n"),
    reply(),
    reply(),
    reply(),
    reply(),
    closed()
  ], { socketTimeout: 200 }),
  conversation("the idle timeout counts from the last reply", [
    hello(),
    send("MAIL FROM:<slow@x.org>\r\n"),
    reply(),
    silence(250),
    reply(),
    closed()
  ], { socketTimeout: 400 }),
  conversation("shutdown during a slow hook", [
    hello(),
    send("MAIL FROM:<slow@x.org>\r\nNOOP\r\n"),
    wait(100),
    { server: "close" },
    reply(),
    reply(),
    closed()
  ], { closeTimeout: 2000 }),
  conversation("a rotated credential closes its connections", [
    hello("authed"),
    hello("other"),
    hello("anonymous"),
    cmd(`AUTH PLAIN ${plain("evict-one", "pass")}`, "authed"),
    cmd(`AUTH PLAIN ${plain("evict-two", "pass")}`, "other"),
    { evict: "evict-one" },
    reply("authed"),
    closed("authed"),
    { evict: "evict-one" },
    { evict: "evict-nobody" },
    cmd("NOOP", "other"),
    cmd("NOOP", "anonymous")
  ], AUTH),
  conversation("a rotated credential during a slow hook", [
    hello(),
    cmd(`AUTH PLAIN ${plain("evict-slow", "pass")}`),
    send("MAIL FROM:<slow@x.org>\r\nNOOP\r\n"),
    wait(100),
    { evict: "evict-slow" },
    reply(),
    closed()
  ], AUTH),
  conversation("a rotated credential in the middle of a message", [
    hello(),
    cmd(`AUTH PLAIN ${plain("evict-data", "pass")}`),
    cmd("MAIL FROM:<a@x.org>"),
    cmd("RCPT TO:<b@y.org>"),
    cmd("DATA"),
    send("Subject: cut\r\n\r\npart"),
    wait(50),
    { evict: "evict-data" },
    reply(),
    closed(),
    destroy(),
    wait(100)
  ], AUTH),
  conversation("shutdown answers a command at once and idle clients at the deadline", [
    greeted("busy"),
    greeted("idle"),
    { server: "close" },
    cmd("NOOP", "busy"),
    closed("busy"),
    { server: "closed", within: 100 },
    reply("idle"),
    closed("idle"),
    { server: "closed", within: 1000 }
  ]),
  conversation("shutdown with nobody connected", [{ server: "close" }, { server: "closed", within: 200 }]),
  conversation("shutdown once the last client has left", [
    greeted(),
    { server: "close" },
    wait(50),
    { server: "closed", within: 50 },
    destroy(),
    { server: "closed", within: 300 }
  ], { closeTimeout: 2000 }),
  conversation("shutdown lets a message in flight finish", [
    inData(),
    send("Subject: in flight\r\n\r\n"),
    wait(50),
    { server: "close" },
    send("body\r\n.\r\n"),
    reply(),
    cmd("NOOP"),
    closed(),
    { server: "closed", within: 1000 }
  ], { closeTimeout: 2000 }),
  conversation("shutdown cuts a message that outlasts the deadline", [
    inData(),
    send("Subject: too slow\r\n\r\n"),
    wait(50),
    { server: "close" },
    reply(),
    closed(),
    { server: "closed", within: 300 },
    destroy(),
    wait(100)
  ], { closeTimeout: 300 }),
  conversation("shutdown while an AUTH token is due", [
    hello(),
    cmd("AUTH PLAIN"),
    { server: "close" },
    cmd(plain("user", "pass")),
    closed()
  ], AUTH),

  // ── The client's FIN ────────────────────────────────────────────────────
  conversation("the client half-closes after QUIT", [hello(), cmd("QUIT"), closed(), end()]),
  conversation("the client half-closes while idle", [hello(), end(), closed()]),
  conversation("the client half-closes with half a line sent", [hello(), send("NOOP"), wait(50), end(), closed()]),
  conversation("the client half-closes behind its commands", [
    hello(),
    send("NOOP\r\nRSET\r\nQUIT\r\n"),
    wait(100),
    end(),
    reply(),
    reply(),
    reply(),
    closed()
  ]),
  conversation("the client half-closes right behind a whole session", [
    hello(),
    send(`MAIL FROM:<a@x.org>\r\nRCPT TO:<b@y.org>\r\nDATA\r\n${MESSAGE}.\r\nNOOP\r\nQUIT\r\n`),
    end(),
    { replies: 6 },
    closed()
  ]),
  conversation("the client half-closes right behind commands that call hooks", [
    hello(),
    send("NOOP\r\nMAIL FROM:<a@x.org>\r\nNOOP\r\nRCPT TO:<b@y.org>\r\nRCPT TO:<nobody@y.org>\r\nRSET\r\n"),
    end(),
    reply(),
    reply(),
    reply(),
    reply(),
    reply(),
    reply(),
    closed()
  ]),
  conversation("the client half-closes right behind a session with a slow hook", [
    hello(),
    send(`MAIL FROM:<a@x.org>\r\nRCPT TO:<slow@y.org>\r\nDATA\r\n${MESSAGE}.\r\nQUIT\r\n`),
    end(),
    reply(),
    closed(),
    wait(350)
  ]),
  conversation("the client half-closes right behind a message, slow DATA hook", [
    inData("slowdata@x.org"),
    send(`${MESSAGE}.\r\nQUIT\r\n`),
    end(),
    closed(),
    wait(350)
  ]),
  conversation("the client half-closes right behind a refused DATA", [
    inData("relay@x.org"),
    send(`${MESSAGE}.\r\nNOOP\r\n`),
    end(),
    reply(),
    reply(),
    closed()
  ]),
  conversation("the client half-closes during a slow hook", [
    hello(),
    send("MAIL FROM:<slow@x.org>\r\n"),
    wait(100),
    end(),
    closed(),
    wait(350)
  ]),
  conversation("the client half-closes behind commands sent during a slow hook", [
    hello(),
    send("MAIL FROM:<slow@x.org>\r\n"),
    wait(100),
    send("RCPT TO:<c@y.org>\r\nNOOP\r\nRCPT TO:<e@y.org>\r\nQUIT\r\n"),
    end(),
    reply(),
    reply(),
    closed()
  ]),
  conversation("the client half-closes behind plain commands sent during a slow hook", [
    hello(),
    send("MAIL FROM:<slow@x.org>\r\n"),
    wait(100),
    send("NOOP\r\nRSET\r\nQUIT\r\n"),
    end(),
    reply(),
    reply(),
    reply(),
    reply(),
    closed()
  ]),
  conversation("the client half-closes behind two writes sent during a slow hook", [
    hello(),
    send("MAIL FROM:<slow@x.org>\r\n"),
    wait(80),
    send("RCPT TO:<c@y.org>\r\n"),
    wait(60),
    send("NOOP\r\nQUIT\r\n"),
    end(),
    reply(),
    reply(),
    reply(),
    reply(),
    closed()
  ]),
  conversation("the client half-closes behind two writes that both call hooks, during a slow hook", [
    hello(),
    send("MAIL FROM:<slow@x.org>\r\n"),
    wait(80),
    send("RCPT TO:<c@y.org>\r\n"),
    wait(60),
    send("RCPT TO:<e@y.org>\r\nNOOP\r\nRCPT TO:<f@y.org>\r\n"),
    end(),
    reply(),
    reply(),
    reply(),
    closed()
  ]),
  conversation("commands sent in pieces during a slow hook", [
    hello(),
    send("MAIL FROM:<slow@x.org>\r\n"),
    wait(60),
    send("RCPT TO:<c@y.org>\r\nNO"),
    wait(60),
    send("OP\r\nRCPT TO:<e@"),
    wait(60),
    send("y.org>\r\nDATA\r\n"),
    reply(),
    reply(),
    reply(),
    reply(),
    reply(),
    send(`${MESSAGE}.\r\n`),
    reply()
  ]),
  conversation("the client drops during a slow hook", [
    hello(),
    send("MAIL FROM:<slow@x.org>\r\nRCPT TO:<b@y.org>\r\n"),
    wait(100),
    destroy(),
    wait(350)
  ]),

  // ── The client's FIN, over TLS ──────────────────────────────────────────
  conversation("the client half-closes over TLS while idle", [open(), startTls(), reply(), cmd(EHLO), end(), closed()], IMPLICIT),
  conversation("the client half-closes over TLS after QUIT", [open(), startTls(), reply(), cmd("QUIT"), closed(), end()], IMPLICIT),
  conversation("the client half-closes over TLS behind its commands", [
    open(),
    startTls(),
    reply(),
    send("EHLO client.test\r\nNOOP\r\nQUIT\r\n"),
    wait(100),
    end(),
    reply(),
    reply(),
    reply(),
    closed()
  ], IMPLICIT),
  conversation("the client half-closes over TLS during a slow hook", [
    open(),
    startTls(),
    reply(),
    cmd(EHLO),
    send("MAIL FROM:<slow@x.org>\r\n"),
    wait(100),
    end(),
    closed(),
    wait(350)
  ], IMPLICIT),
  conversation("the client half-closes over TLS in the middle of a message", [
    open(),
    startTls(),
    reply(),
    cmd(EHLO),
    cmd("MAIL FROM:<a@x.org>"),
    cmd("RCPT TO:<b@y.org>"),
    cmd("DATA"),
    send("Subject: partial\r\n\r\nsome of the bo"),
    wait(100),
    end(),
    closed()
  ], IMPLICIT),
  conversation("the client half-closes after STARTTLS", [hello(), cmd("STARTTLS"), startTls(), cmd(EHLO), end(), closed()], TLS),

  // ── More of TLS, timeouts and eviction ──────────────────────────────────
  conversation("STARTTLS and then nothing until the timeout", [hello(), cmd("STARTTLS"), closed()], {
    tls: true,
    socketTimeout: 300
  }, { errors: true }),
  conversation("a credential rotated after STARTTLS forgot the user", [
    hello(),
    cmd(`AUTH PLAIN ${plain("evict-tls", "pass")}`),
    cmd("STARTTLS"),
    startTls(),
    // One exchange first: the client is done with the handshake a moment
    // before the server is, and until then the server still knows the user.
    cmd("NOOP"),
    { evict: "evict-tls" },
    cmd("NOOP"),
    cmd(EHLO),
    cmd(`AUTH PLAIN ${plain("evict-tls", "pass")}`),
    { evict: "evict-tls" },
    reply(),
    closed()
  ], { tls: true, allowInsecureAuth: true }),
  conversation("a rotated credential over implicit TLS", [
    open(),
    startTls(),
    reply(),
    cmd(EHLO),
    cmd(`AUTH LOGIN ${b64("evict-implicit")}`),
    cmd(b64("pass")),
    { evict: "evict-implicit" },
    reply(),
    closed()
  ], IMPLICIT),
  conversation("shutdown over implicit TLS", [
    open(),
    startTls(),
    reply(),
    { server: "close" },
    reply(),
    closed(),
    { server: "closed", within: 300 }
  ], { tls: true, secure: true, closeTimeout: 300 }),
  conversation("the idle timeout with half a line sent", [greeted(), send("NOOP"), reply(), closed()], { socketTimeout: 300 }),
  conversation("the idle timeout before the greeting is read", [open(), reply(), reply(), closed()], { socketTimeout: 150 }),

  // ── More pipelining ─────────────────────────────────────────────────────
  conversation("QUIT twice in one write", [hello(), send("QUIT\r\nQUIT\r\nNOOP\r\n"), reply(), closed()]),
  conversation("commands behind an HTTP request line", [greeted(), send("GET / HTTP/1.1\r\nHost: x\r\nNOOP\r\n"), reply(), closed()]),
  conversation("commands behind the tenth unrecognized one", [
    greeted(),
    send(Array.from({ length: 12 }, (_, i) => `XYZ${i}\r\n`).join("") + "NOOP\r\n"),
    { replies: 10 },
    closed()
  ]),
  conversation("an AUTH LOGIN exchange in one write", [
    hello(),
    send(`AUTH LOGIN\r\n${b64("user")}\r\n${b64("pass")}\r\nMAIL FROM:<a@x.org>\r\nNOOP\r\n`),
    reply(),
    reply(),
    reply(),
    reply(),
    reply()
  ], AUTH),
  conversation("an AUTH PLAIN exchange in one write, cancelled", [
    hello(),
    send(`AUTH PLAIN\r\n*\r\nAUTH PLAIN\r\n${plain("slow", "x")}\r\nNOOP\r\nAUTH PLAIN\r\n`),
    reply(),
    reply(),
    reply(),
    silence(150),
    reply(),
    reply(),
    reply()
  ], AUTH),
  conversation("a 421 from a hook in the middle of a pipeline", [
    hello(),
    send("MAIL FROM:<a@x.org>\r\nRCPT TO:<gone@y.org>\r\nRCPT TO:<b@y.org>\r\nNOOP\r\n"),
    reply(),
    reply(),
    closed()
  ]),
  conversation("a line too long in the middle of a pipeline", [
    hello(),
    send(`NOOP\r\nNOOP ${"x".repeat(17000)}\r\nMAIL FROM:<a@x.org>\r\nNOOP\r\n`),
    reply(),
    reply(),
    closed()
  ]),
  conversation("DATA refused early, then the next transaction in the same write", [
    hello(),
    send(
      `MAIL FROM:<relay@x.org>\r\nRCPT TO:<b@y.org>\r\nDATA\r\n${MESSAGE}.\r\n` +
        `MAIL FROM:<a@x.org>\r\nRCPT TO:<b@y.org>\r\nDATA\r\n${MESSAGE}.\r\nQUIT\r\n`
    ),
    { replies: 9 },
    closed()
  ]),
  conversation("an empty message and the next command in one write", [
    envelope(),
    send("DATA\r\n.\r\nNOOP\r\n"),
    reply(),
    reply(),
    reply()
  ]),

  // ── Floods ──────────────────────────────────────────────────────────────
  conversation("twenty thousand commands in one write", [
    greeted(),
    { send: "NOOP\r\n", times: 20000 },
    { replies: 20000 },
    cmd("QUIT"),
    closed()
  ]),
  conversation("a flood of mixed commands", [
    hello(),
    { send: "RSET\r\nVRFY x\r\nHELP\r\nEHLO client.test\r\nFOO\r\nMAIL FROM:x\r\nRCPT TO:<b@y.org>\r\nDATA\r\n", times: 1 },
    { replies: 8 },
    { send: "RSET\r\nVRFY x\r\nHELP\r\nEHLO client.test\r\nMAIL FROM:x\r\nRCPT TO:<b@y.org>\r\nDATA\r\nNOOP\r\n", times: 2500 },
    { replies: 20000 },
    cmd("QUIT"),
    closed()
  ]),
  conversation("a flood sent during a slow hook, then the client's FIN", [
    hello(),
    send("MAIL FROM:<slow@x.org>\r\n"),
    wait(100),
    { send: "NOOP\r\n", times: 30000 },
    send("QUIT\r\n"),
    end(),
    { replies: 30002 },
    closed()
  ]),
  conversation("a flood, then a transaction, with the client's FIN right behind", [
    hello(),
    { send: "NOOP\r\n", times: 20000 },
    send("MAIL FROM:<a@x.org>\r\nRCPT TO:<b@y.org>\r\nNOOP\r\nQUIT\r\n"),
    end(),
    { replies: 20000 },
    reply(),
    reply(),
    reply(),
    reply(),
    closed()
  ]),
  conversation("thirty transactions started in one write", [
    hello(),
    { send: "MAIL FROM:<a@x.org>\r\nRCPT TO:<b@y.org>\r\nRSET\r\n", times: 30 },
    { replies: 90 },
    cmd("QUIT"),
    closed()
  ])
];

// ── Buffer.from(text, "base64") ─────────────────────────────────────────────

/** A small deterministic generator, so the tables are the same on every run. */
const random = (() => {
  let state = 0x2f6e2b1;
  return (below: number) => {
    state = (Math.imul(state, 1103515245) + 12345) >>> 0;
    return (state >>> 8) % below;
  };
})();

const pick = <T>(items: readonly T[]): T => items[random(items.length)];

const BASE64_ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/-_";
const BASE64_NOISE = ["=", "=", " ", "\t", "*", "!", ".", ",", "é", "\u0141", "中", "😀", "\u0000", "%", "\r", "\n"];

export const base64Inputs: string[] = [
  "", "*", "=", "==", "YQ==", "YQ", "YQ=", "Y", "YWJj", "YWJjZA", "YWJjZA==", "YWJjZA=", "YQ==YQ==", "YQ==YWJj",
  "YW Jj", "YW\tJj", "YW*Jj", "YW-_", "YW+/", "-_-_", "!!!!", "YWJj!", "Y=WJj", "YW=Jj", "YWJ=j", "=YWJj", "YQ=a",
  "é", "YWé Jj", "AA\u0000AA", "a", "ab", "abc", "abcd", "abcde", "====", "a===", "ab==cd", "////", "__--",
  "\u0141\u0141\u0141\u0141", "QUJD\u00a0", "AHVzZXIAcGFzcw==", "AHVzZXIAcGFzcw", "AHVzZXIA cGFzcw==", "😀QUJD",
  "QUJD\ufffd", "Q\u0000U\u0000J\u0000D",
  ...Array.from({ length: 1500 }, () => {
    const length = random(40);
    let text = "";
    for (let i = 0; i < length; i++) text += random(8) === 0 ? pick(BASE64_NOISE) : BASE64_ALPHABET[random(BASE64_ALPHABET.length)];
    return text;
  })
];

// ── parseAddressCommand ─────────────────────────────────────────────────────

/** RFC 3492, so a label can carry any text at all, valid in a domain or not. */
const punycode = (text: string): string => {
  const points = Array.from(text, (c) => c.codePointAt(0)!);
  const basic = points.filter((point) => point < 0x80);
  let output = String.fromCodePoint(...basic) + (basic.length ? "-" : "");
  const digit = (value: number) => String.fromCharCode(value < 26 ? 97 + value : 22 + value);
  const adapt = (delta: number, count: number, first: boolean) => {
    delta = first ? Math.floor(delta / 700) : delta >> 1;
    delta += Math.floor(delta / count);
    let k = 0;
    for (; delta > 455; k += 36) delta = Math.floor(delta / 35);
    return k + Math.floor((36 * delta) / (delta + 38));
  };
  let [n, delta, bias, handled] = [0x80, 0, 72, basic.length];
  while (handled < points.length) {
    const next = Math.min(...points.filter((point) => point >= n));
    delta += (next - n) * (handled + 1);
    n = next;
    for (const point of points) {
      if (point < n) delta++;
      if (point !== n) continue;
      let q = delta;
      for (let k = 36; ; k += 36) {
        const t = k <= bias ? 1 : k >= bias + 26 ? 26 : k - bias;
        if (q < t) break;
        output += digit(t + ((q - t) % (36 - t)));
        q = Math.floor((q - t) / (36 - t));
      }
      output += digit(q);
      bias = adapt(delta, handled + 1, handled === basic.length);
      delta = 0;
      handled++;
    }
    delta++;
    n++;
  }
  return output;
};

/** Text for Punycode labels: what a registry would allow, and what it would not. */
const LABEL_TEXTS = [
  "münchen", "bücher", "пример", "δοκιμή", "例え", "テスト", "مثال", "שלום", "한국", "ß", "straße", "İstanbul", "ΑΣ", "ας",
  "a\u200db", "a\u200cb", "क्\u200dष", "ab\u0301", "\u0301ab", "a·b", "l·l", "😀", "1२3", "a_b", "A", "Ünï", "αβγ1", "אב1", "1אב",
  "אבa", "aאב", "a--b", "ab--c", "-a", "a-", "٠١٢", "x\u00adz", "ǆ", "ǅ", "ℌ", "①", "㈠", "ｅｘ", "é", "e\u0301", "ﬁ", "\u017f", "K",
  "a.b", "a b", "a@b", "\u0000", "\u007f", "\u0080", "\ufffd", "\ud800\udc00", "\udbff\udfff", "a\u2028b", "٣a", "a٣", "۱۲", "א-ב",
  "ابج", "ا1", "1ا", "ب٣", "ב٣", "ｘｎ", "xn--a", "", "abc", "日本語", "中文", "ไทย", "ქართული", "հայ", "ελληνικά", "ĉapelo", "ŋ", "ʻokina"
];

const LOCAL_PARTS = ["a", "A.b", "first.last", ".a", "a.", "a..b", "", "a b", "\"q\"", "a+tag", "ü", "😀", "a<b", "a>b", "x".repeat(64), "x".repeat(250), "a@b"];
const DOMAINS = [
  "x.org", "X.ORG", "x", "x.", ".x", "x..org", "-x.org", "x-.org", "x.-org", "x_y.org", "x y.org", "", "x.org:25",
  "[192.0.2.1]", "[192.0.2.256]", "[01.2.3.4]", "[1.2.3]", "[IPv6:::1]", "[ipv6:2001:db8::1]", "[IPV6:fe80::1%eth0]",
  "[IPv6:fe80::1%]", "[IPv6:1::2::3]", "[IPv6:::ffff:1.2.3.4]", "[IPv6:1:2:3:4:5:6:7:8]", "[IPv6:1:2:3:4:5:6:7:8:9]",
  "[IPv6:1:2:3:4:5:6:1.2.3.4]", "[IPv6:::01.2.3.4]", "[IPv6:]", "[::1]", "[]", "[", "]", "[x.org]", "[IPv6:12345::]",
  "xn--mnchen-3ya.de", "XN--MNCHEN-3YA.de", "xn--MNCHEN-3YA.de", "xn--a.de", "xn--.de", "xn---.de", "xn--80ak6aa92e.com",
  "xn--bcher-kva.xn--p1ai", "xn--zca.de", "xn--1ch.com", "xn--ls8h.la", "xn--u-ccb.com", "xn--0.com", "xn--caf-dma.fr",
  "xn--nxasmq6b.gr", "xn--ngbrx.ae", "xn--11b4c3d.in", "sub.xn--wgv71a119e.jp", "xn--pple-43d.com", "xn--ab-0ea.es",
  "a.xn--abc-.b", "xn--99zt52a.jp", "xn--fa-hia.de", "bücher.example", "例え.テスト", "😀.example",
  "d".repeat(180) + ".org", "d".repeat(250) + ".org", "😀".repeat(90) + ".org", "😀".repeat(95) + ".org"
];
const PARAMETERS = [
  "", " SIZE=100", " size=1 BODY=8BITMIME", " SMTPUTF8", " X=a+3Db", " X=+41+42+GG+4", " X=a=b", " =v", " A= B", " NOTIFY=SUCCESS,FAILURE ORCPT=rfc822;a+40b.c",
  "  A=1   B=2  ", " \tFLAG\t", " A=1 A=2 a=3", " Ü=ü", " ß=1", " X=+e9+E9", "\u00a0NBSP=1", " X=😀"
];
const FRAMES: ((address: string, parameters: string) => string)[] = [
  (a, p) => `MAIL FROM:<${a}>${p}`,
  (a, p) => `mail from:<${a}>${p}`,
  (a, p) => `MAIL FROM: <${a}>${p}`,
  (a, p) => `MAIL FROM :<${a}>${p}`,
  (a, p) => `MAIL FROM:${a}${p}`,
  (a, p) => `MAIL FROM:<${a}${p}`,
  (a, p) => `MAIL FROM<${a}>${p}`,
  (a, p) => `MAIL  FROM:<${a}>${p}`,
  (a, p) => ` MAIL FROM:<${a}>${p} `,
  (a, p) => `RCPT TO:<${a}>${p}`,
  (a, p) => `MAIL FROM:<<${a}>>${p}`,
  (a, p) => `MAIL FROM:<${a}>x${p}`
];

export const addressCommandLines: [string, string][] = [
  ["mail from", "MAIL FROM:<>"],
  ["mail from", "MAIL FROM:<> SIZE=1"],
  ["mail from", ""],
  ["mail from", ":"],
  ["mail from", "MAIL FROM"],
  ["mail from", "MAIL FROM:"],
  ["mail from", "MAIL FROM::<a@x.org>"],
  ["mail from", "MA\u0131L FROM:<a@x.org>"],
  ["mail from", "MAIL FROM\u00a0:<a@x.org>"],
  ["mail from", "\ufeffMAIL FROM:<a@x.org>"],
  ["mail from", "MAIL FROM:\u00a0<a@x.org>\u2003SIZE=1"],
  ["rcpt to", "RCPT TO:<>"],
  ["rcpt to", "rcpt to:<a@x.org> NOTIFY=NEVER"],
  ["rcpt to", "MAIL FROM:<a@x.org>"],
  ["RCPT TO", "rcpt to:<a@x.org>"],
  ...LOCAL_PARTS.flatMap((local) => DOMAINS.map((domain): [string, string] => ["mail from", `MAIL FROM:<${local}@${domain}>`])),
  ...DOMAINS.map((domain): [string, string] => ["rcpt to", `RCPT TO:<b@${domain}> NOTIFY=NEVER`]),
  ...PARAMETERS.flatMap((parameters) => FRAMES.map((frame): [string, string] => ["mail from", frame("a@x.org", parameters)])),
  ...Array.from({ length: 600 }, (): [string, string] => [
    "mail from",
    pick(FRAMES)(`${pick(LOCAL_PARTS)}@${pick(DOMAINS)}`, pick(PARAMETERS) + (random(4) === 0 ? pick(PARAMETERS) : ""))
  ]),
  // Punycode labels made from any text, first and last in the domain (a
  // label ending in `-` passes the syntax check only at the end).
  ...LABEL_TEXTS.flatMap((text): [string, string][] => [
    ["mail from", `MAIL FROM:<a@xn--${punycode(text)}.example>`],
    ["rcpt to", `RCPT TO:<a@sub.xn--${punycode(text)}>`]
  ])
];
