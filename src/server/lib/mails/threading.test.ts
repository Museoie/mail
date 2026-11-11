import { describe, it, expect } from "bun:test";
import {
  collectThread,
  isReplySubject,
  linkedByHeaders,
  matchesBySubject,
  normalizeSubject,
  ThreadCandidate
} from "./threading";

const mail = (
  id: string,
  fields: Partial<ThreadCandidate> = {}
): ThreadCandidate => ({
  mail_id: id,
  message_id: `<${id}@x>`,
  subject: "Plan",
  from_address: [{ address: "alice@x.com" }],
  to_address: [{ address: "me@inbox.test" }],
  ...fields
});

describe("normalizeSubject", () => {
  it("strips stacked reply and forward markers", () => {
    expect(normalizeSubject("Re: RE: Fwd: Project  plan")).toBe("project plan");
    expect(normalizeSubject("AW: Re[2]: Hallo")).toBe("hallo");
  });

  it("leaves other subjects alone", () => {
    expect(normalizeSubject("Return policy")).toBe("return policy");
  });
});

describe("isReplySubject", () => {
  it("recognises reply and forward prefixes", () => {
    expect(isReplySubject("Re: x")).toBe(true);
    expect(isReplySubject("Fwd: x")).toBe(true);
    expect(isReplySubject("Regarding x")).toBe(false);
  });
});

describe("linkedByHeaders", () => {
  it("links a reply to the message it answers", () => {
    const original = mail("a");
    const reply = mail("b", { in_reply_to: "<a@x>" });
    expect(linkedByHeaders(original, reply)).toBe(true);
  });

  it("links two replies that share a References entry", () => {
    const one = mail("b", { reference_ids: ["<a@x>"] });
    const two = mail("c", { reference_ids: ["<a@x>", "<b@x>"] });
    expect(linkedByHeaders(one, two)).toBe(true);
  });

  it("does not link unrelated mail", () => {
    expect(linkedByHeaders(mail("a"), mail("b"))).toBe(false);
  });
});

describe("matchesBySubject", () => {
  it("matches a reply with the same subject and a shared participant", () => {
    const original = mail("a", { subject: "Plan" });
    const reply = mail("b", {
      subject: "Re: Plan",
      from_address: [{ address: "me@inbox.test" }],
      to_address: [{ address: "Alice@x.com" }]
    });
    expect(matchesBySubject(original, reply)).toBe(true);
  });

  it("does not match two non-replies with one subject", () => {
    expect(
      matchesBySubject(
        mail("a", { subject: "Weekly digest" }),
        mail("b", { subject: "Weekly digest" })
      )
    ).toBe(false);
  });

  it("does not match strangers who used the same subject", () => {
    expect(
      matchesBySubject(
        mail("a", { subject: "Re: Plan" }),
        mail("b", {
          subject: "Re: Plan",
          from_address: [{ address: "bob@y.com" }],
          to_address: [{ address: "carol@y.com" }]
        })
      )
    ).toBe(false);
  });

  it("does not match very short subjects", () => {
    expect(
      matchesBySubject(mail("a", { subject: "Re: hi" }), mail("b", { subject: "hi" }))
    ).toBe(false);
  });
});

describe("collectThread", () => {
  it("follows a chain of replies from any message in it", () => {
    const a = mail("a");
    const b = mail("b", { in_reply_to: "<a@x>", reference_ids: ["<a@x>"] });
    const c = mail("c", { in_reply_to: "<b@x>", reference_ids: ["<a@x>", "<b@x>"] });
    const other = mail("z", { subject: "Unrelated" });
    expect(
      collectThread(c, [a, b, c, other]).map((m) => m.mail_id).sort()
    ).toEqual(["a", "b", "c"]);
  });

  it("returns the seed alone when nothing matches", () => {
    const a = mail("a");
    expect(collectThread(a, [a, mail("z", { subject: "Other" })])).toEqual([a]);
  });
});
