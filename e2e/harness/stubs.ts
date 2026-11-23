import crypto from "crypto";

/** One file part of a request the Mailgun stub received. */
export interface StubAttachment {
  filename: string;
  contentType: string;
  size: number;
  sha256: string;
  text: string;
}

/** One `POST /v3/{domain}/messages` the server under test made. */
export interface MailgunRequest {
  method: string;
  path: string;
  domain: string;
  authorization: string | null;
  contentType: string | null;
  /** Every text field, in arrival order; a repeated field keeps each value. */
  fields: Record<string, string[]>;
  /** Field names in the order the multipart body carried them. */
  fieldOrder: string[];
  attachments: StubAttachment[];
}

export type MailgunReply = (request: MailgunRequest) => Response | Promise<Response>;

/**
 * Stands in for Mailgun's HTTP API. The server under test is pointed at it
 * with `MAILGUN_URL`; every message it tries to send lands in `requests`.
 */
export interface MailgunStub {
  url: string;
  requests: MailgunRequest[];
  /** Answers the next request only; later ones get the default acceptance. */
  replyOnce(reply: MailgunReply): void;
  /** Answers every request until `reset`. */
  replyAlways(reply: MailgunReply): void;
  reset(): void;
  stop(): void;
}

const accepted = (request: MailgunRequest) =>
  Response.json({
    id: `<${crypto.randomUUID()}@${request.domain}>`,
    message: "Queued. Thank you.",
  });

export const startMailgunStub = (): MailgunStub => {
  const requests: MailgunRequest[] = [];
  const once: MailgunReply[] = [];
  let always: MailgunReply | undefined;

  const server = Bun.serve({
    port: 0,
    hostname: "127.0.0.1",
    maxRequestBodySize: 1024 * 1024 * 1024,
    fetch: async (req) => {
      const url = new URL(req.url);
      const fields: Record<string, string[]> = {};
      const fieldOrder: string[] = [];
      const attachments: StubAttachment[] = [];
      const contentType = req.headers.get("content-type");
      if (contentType?.startsWith("multipart/form-data")) {
        const form = await req.formData();
        for (const [name, value] of form as unknown as Iterable<[string, string | File]>) {
          fieldOrder.push(name);
          if (typeof value === "string") {
            (fields[name] ??= []).push(value);
            continue;
          }
          const bytes = new Uint8Array(await value.arrayBuffer());
          attachments.push({
            filename: value.name,
            contentType: value.type,
            size: bytes.byteLength,
            sha256: crypto.createHash("sha256").update(bytes).digest("hex"),
            text: bytes.byteLength <= 4096 ? new TextDecoder().decode(bytes) : "",
          });
        }
      } else {
        await req.arrayBuffer();
      }
      const request: MailgunRequest = {
        method: req.method,
        path: url.pathname,
        domain: /^\/v3\/([^/]+)\/messages$/.exec(url.pathname)?.[1] ?? "",
        authorization: req.headers.get("authorization"),
        contentType,
        fields,
        fieldOrder,
        attachments,
      };
      requests.push(request);
      const reply = once.shift() ?? always ?? accepted;
      return reply(request);
    },
  });

  return {
    url: `http://127.0.0.1:${server.port}`,
    requests,
    replyOnce: (reply) => void once.push(reply),
    replyAlways: (reply) => {
      always = reply;
    },
    reset: () => {
      requests.length = 0;
      once.length = 0;
      always = undefined;
    },
    stop: () => void server.stop(true),
  };
};

/** One alarm the server under test posted to its Discord webhook. */
export interface Alarm {
  title: string;
  description: string;
  color?: number;
  timestamp?: string;
}

/**
 * Stands in for the Discord webhook behind `DISCORD_ALARM_WEBHOOK`. The server
 * holds each alarm key to one post a minute, so a test sees the first alarm of
 * a kind per server and should not count on a second.
 */
export interface AlarmStub {
  url: string;
  alarms: Alarm[];
  reset(): void;
  stop(): void;
}

export const startAlarmStub = (): AlarmStub => {
  const alarms: Alarm[] = [];
  const server = Bun.serve({
    port: 0,
    hostname: "127.0.0.1",
    fetch: async (req) => {
      try {
        const body = (await req.json()) as { embeds?: Alarm[] };
        for (const embed of body.embeds ?? []) alarms.push(embed);
      } catch {
        // A body that is not JSON is not an alarm this suite can read.
      }
      return new Response(null, { status: 204 });
    },
  });
  return {
    url: `http://127.0.0.1:${server.port}/alarm`,
    alarms,
    reset: () => {
      alarms.length = 0;
    },
    stop: () => void server.stop(true),
  };
};
