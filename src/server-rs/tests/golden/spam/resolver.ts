/**
 * Golden fixture for the blocklist check's resolver path.
 *
 * Points Bun's resolver at scripted DNS servers on loopback UDP ports and runs
 * the Bun server's own `checkDnsbls` against them: listings, open-resolver
 * warnings, NXDOMAIN, empty answers, server failures, refusals, answers on
 * either side of the timeout, names that cannot be asked, and sequences that
 * show what the resolver remembers from one question to the next — answers it
 * caches, a question it repeats, a first name server it stops asking once a
 * second one has to answer for it.
 *
 * `resolver.json` records the scripts, what `checkDnsbls` returned, and for the
 * sequences how often each server was asked; `src/server-rs/tests/spam_golden.rs`
 * replays the scripts to the Rust resolver and requires the same record.
 *
 * From the repository root (takes about twenty seconds: several cases run
 * into the two-second timeout, and the resolver is left to finish in between):
 *
 *   bun src/server-rs/tests/golden/spam/resolver.ts
 */

import dgram from "dgram";
import dns from "dns";
import { writeFileSync } from "fs";
import { join } from "path";

import { checkDnsbls } from "../../../../server/lib/spam/dnsbl";
import { DnsBlocklist } from "../../../../server/lib/spam/types";

/**
 * What a server answers a question with: a response code and A records (`ttl`
 * seconds, 60 unless given), an SOA record in the authority section if `soa`
 * is given, all after `delayMs` if given; or null to stay silent.
 */
type Answer = {
  rcode: number;
  records: string[];
  ttl?: number;
  soa?: { ttl: number; minimum: number };
  delayMs?: number;
} | null;

/** One answer for every question about a name, or one per question in turn (the last repeats). */
type Script = Record<string, Answer | Answer[]>;

const IP = "198.51.100.7";
const REVERSED = "7.100.51.198";

const listed: Answer = { rcode: 0, records: ["127.0.0.2"] };
const nxdomain: Answer = { rcode: 3, records: [] };
const nodata: Answer = { rcode: 0, records: [] };
const servfail: Answer = { rcode: 2, records: [] };
const refused: Answer = { rcode: 5, records: [] };

const list = (hostname: string, score: number): DnsBlocklist => ({ hostname, name: hostname, score });

const u16 = (value: number): number[] => [(value >> 8) & 0xff, value & 0xff];
const u32 = (value: number): number[] => [...u16(value >>> 16), ...u16(value & 0xffff)];
const wireName = (name: string): number[] => [
  ...name.split(".").flatMap((label) => [label.length, ...Buffer.from(label)]),
  0,
];

/** A scripted name server; `asked(host)` counts the questions it received about a blocklist host. */
const startServer = async (
  script: Script
): Promise<{ port: number; asked: (host: string) => number; close: () => void }> => {
  const asked: Record<string, number> = {};
  let closed = false;
  const socket = dgram.createSocket("udp4");
  socket.on("message", (query, peer) => {
    const labels: string[] = [];
    let at = 12;
    while (query[at] !== 0) {
      const size = query[at];
      labels.push(query.subarray(at + 1, at + 1 + size).toString());
      at += 1 + size;
    }
    const name = labels.join(".").toLowerCase();
    const host = name.startsWith(`${REVERSED}.`) ? name.slice(REVERSED.length + 1) : "";
    const turn = asked[host] ?? 0;
    asked[host] = turn + 1;
    const entry = script[host];
    // A name outside the script does not exist; a silent one gets no reply.
    const answer = entry === undefined ? nxdomain : Array.isArray(entry) ? entry[Math.min(turn, entry.length - 1)] : entry;
    if (answer === null) return;
    const response = [
      ...query.subarray(0, 2),
      0x81,
      0x80 | answer.rcode,
      ...u16(1),
      ...u16(answer.records.length),
      ...u16(answer.soa ? 1 : 0),
      ...u16(0),
      ...query.subarray(12, at + 5),
    ];
    for (const record of answer.records) {
      response.push(0xc0, 12, ...u16(1), ...u16(1), ...u32(answer.ttl ?? 60), ...u16(4), ...record.split(".").map(Number));
    }
    if (answer.soa) {
      const rdata = [...wireName("ns.test"), ...wireName("hostmaster.test"), ...u32(1), ...u32(3600), ...u32(600), ...u32(86400), ...u32(answer.soa.minimum)];
      response.push(...wireName("test"), ...u16(6), ...u16(1), ...u32(answer.soa.ttl), ...u16(rdata.length), ...rdata);
    }
    setTimeout(() => {
      if (!closed) socket.send(Buffer.from(response), peer.port, peer.address);
    }, answer.delayMs ?? 0);
  });
  await new Promise<void>((resolve) => socket.bind(0, "127.0.0.1", resolve));
  return {
    port: socket.address().port,
    asked: (host) => asked[host.toLowerCase().replace(/\.$/, "")] ?? 0,
    close: () => {
      closed = true;
      socket.close();
    },
  };
};

const sleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

// ---------------------------------------------------------------------------
// Sequences: what the resolver remembers from one question to the next
// ---------------------------------------------------------------------------

interface Sequence {
  name: string;
  /** One script per name server, in the order the resolver is given them. */
  servers: Script[];
  /** The blocklist hostname each step asks about, one check at a time. */
  steps: string[];
}

const sequences: Sequence[] = [
  {
    // A first server that never answered anything gets the full two seconds.
    // By then the check has given up; the resolver has not, and keeps what it
    // then learns from the second server.
    name: "a first server that was never heard from",
    servers: [
      { "cold-silent.test": null },
      { "cold-silent.test": listed, "cold-second.test": listed },
    ],
    steps: ["cold-silent.test", "cold-second.test", "cold-silent.test"],
  },
  {
    // Whatever a server answers is the answer, a failure included; only
    // silence moves on to the next server. Answers are remembered for as long
    // as they say. Once a server has a record of answering, its silence is
    // noticed within a second, and it is not asked again while another answers.
    name: "a first server that stops answering",
    servers: [
      {
        "warm-servfail.test": servfail,
        "warm-refused.test": refused,
        "warm-listed.test": listed,
        "warm-nodata.test": nodata,
        "warm-ttl0.test": { rcode: 0, records: ["127.0.0.2"], ttl: 0 },
        "warm-nx-soa.test": { rcode: 3, records: [], soa: { ttl: 60, minimum: 30 } },
        "warm-nx-soa0.test": { rcode: 3, records: [], soa: { ttl: 60, minimum: 0 } },
        "warm-silent.test": null,
        "warm-silent-nx.test": null,
      },
      {
        "warm-nx.test": listed,
        "warm-servfail.test": listed,
        "warm-refused.test": listed,
        "warm-nodata.test": listed,
        "warm-nx-soa.test": listed,
        "warm-silent.test": listed,
        "warm-second.test": listed,
      },
    ],
    steps: [
      "warm-nx.test",
      "warm-nx.test",
      "warm-servfail.test",
      "warm-servfail.test",
      "warm-refused.test",
      "warm-listed.test",
      "warm-listed.test",
      "Warm-LISTED.test.",
      "warm-nodata.test",
      "warm-nodata.test",
      "warm-ttl0.test",
      "warm-ttl0.test",
      "warm-nx-soa.test",
      "warm-nx-soa.test",
      "warm-nx-soa0.test",
      "warm-nx-soa0.test",
      "warm-silent.test",
      "warm-second.test",
      "warm-silent.test",
      "warm-silent-nx.test",
    ],
  },
  {
    // One server: a question that goes unanswered is asked again, and only
    // the latest asking is listened to — an answer that takes longer than the
    // resolver waits is never heard, however often it is sent.
    name: "a question that has to be asked twice",
    servers: [
      {
        "twice-lost.test": [null, listed],
        "twice-slow.test": { rcode: 0, records: ["127.0.0.2"], delayMs: 1500 },
      },
    ],
    steps: ["twice-warm-1.test", "twice-warm-2.test", "twice-warm-3.test", "twice-lost.test", "twice-slow.test"],
  },
];

const recorded = [];
for (const sequence of sequences) {
  const servers = [];
  for (const serverScript of sequence.servers) servers.push(await startServer(serverScript));
  dns.setServers(servers.map((server) => `127.0.0.1:${server.port}`));
  const steps = [];
  for (const hostname of sequence.steps) {
    const before = servers.map((server) => server.asked(hostname));
    const result = await checkDnsbls(IP, [list(hostname, 40)]);
    // What the resolver still does once the check has its answer counts too.
    await sleep(300);
    steps.push({ hostname, result, asked: servers.map((server, index) => server.asked(hostname) - before[index]) });
  }
  // Let the resolver finish with whatever it is still asking before the
  // servers change under it.
  await sleep(6000);
  for (const server of servers) server.close();
  recorded.push({ name: sequence.name, servers: sequence.servers, steps });
}

// ---------------------------------------------------------------------------
// One name server: what each kind of answer makes of the verdict
// ---------------------------------------------------------------------------

const script: Script = {
  "listed.test": listed,
  "listed-twice.test": { rcode: 0, records: ["127.0.0.2", "127.0.0.10"] },
  "warning.test": { rcode: 0, records: ["127.255.255.254"] },
  "listed-and-warning.test": { rcode: 0, records: ["127.255.255.254", "127.0.0.4"] },
  "hijacked.test": { rcode: 0, records: ["192.0.2.1"] },
  "clean.test": nxdomain,
  "clean-soa.test": { rcode: 3, records: [], soa: { ttl: 300, minimum: 60 } },
  "nodata.test": nodata,
  "nodata-soa.test": { rcode: 0, records: [], soa: { ttl: 300, minimum: 60 } },
  "formerr.test": { rcode: 1, records: [] },
  "servfail.test": servfail,
  "notimp.test": { rcode: 4, records: [] },
  "refused.test": refused,
  "yxdomain.test": { rcode: 6, records: [] },
  "notauth.test": { rcode: 9, records: [] },
  "silent.test": null,
  // Either side of the two-second timeout, with room for a busy machine.
  "slow.test": { rcode: 0, records: ["127.0.0.2"], delayMs: 1100 },
  "late.test": { rcode: 0, records: ["127.0.0.2"], delayMs: 2900 },
};

const cases: DnsBlocklist[][] = [
  [list("listed.test", 40), list("clean.test", 30)],
  [list("listed.test", 40), list("listed-twice.test", 30), list("listed-and-warning.test", 25)],
  [list("clean.test", 30), list("nodata.test", 25)],
  [list("clean.test", 30)],
  [list("nodata.test", 25)],
  [list("clean-soa.test", 30), list("nodata-soa.test", 25)],
  [list("clean.test", 30), list("warning.test", 25)],
  [list("clean.test", 30), list("hijacked.test", 25)],
  [list("clean.test", 30), list("formerr.test", 25)],
  [list("clean.test", 30), list("servfail.test", 25)],
  [list("clean.test", 30), list("notimp.test", 25)],
  [list("clean.test", 30), list("refused.test", 25)],
  [list("clean.test", 30), list("yxdomain.test", 25)],
  [list("clean.test", 30), list("notauth.test", 25)],
  [list("listed.test", 40), list("servfail.test", 30), list("refused.test", 25)],
  [list("listed.test", 40), list("silent.test", 25)],
  [list("silent.test", 25), list("clean.test", 30)],
  [list("silent.test", 40), list("silent.test", 30), list("silent.test", 25)],
  [list("slow.test", 40), list("clean.test", 30)],
  [list("late.test", 40), list("clean.test", 30)],
  [list("slow.test", 40), list("late.test", 30)],
  // The hostname as an operator might write it.
  [list("Listed.TEST", 40)],
  [list("listed.test.", 40)],
  [list("clean.test.", 40)],
  // Names no resolver can ask for.
  [list("clean.test", 30), list("bad..test", 25)],
  [list("clean.test", 30), list(`${"x".repeat(64)}.test`, 25)],
  [list("clean.test", 30), list("has space.test", 25)],
  [list("clean.test", 30), list("bücher.test", 25)],
  [list("clean.test", 30), list("", 25)],
];

// Every case at once, on a server the resolver has never heard from: each
// question gets the full two seconds.
const single = await startServer(script);
dns.setServers([`127.0.0.1:${single.port}`]);
const results = await Promise.all(cases.map(async (dnsbls) => ({ dnsbls, result: await checkDnsbls(IP, dnsbls) })));
single.close();

const lines = (items: unknown[]): string => items.map((item) => JSON.stringify(item)).join(",\n");
writeFileSync(
  join(import.meta.dir, "resolver.json"),
  `{"ip":${JSON.stringify(IP)},"script":${JSON.stringify(script)},"cases":[\n${lines(results)}\n],"sequences":[\n${lines(recorded)}\n]}\n`
);
console.log(`resolver.json: ${results.length} cases, ${recorded.length} sequences`);
for (const sequence of recorded) {
  console.log(`  ${sequence.name}`);
  for (const step of sequence.steps) {
    console.log(
      `    ${step.hostname.padEnd(22)} score ${String(step.result.score).padEnd(2)} evaluated ${String(step.result.evaluated).padEnd(5)} asked ${JSON.stringify(step.asked)}`
    );
  }
}
process.exit(0);
