import { describe, it, expect } from "bun:test";
import {
  bareMessageId,
  collectThread,
  isReplySubject,
  linkedByHeaders,
  matchesBySubject,
  normalizeSubject,
  storedIdForms,
  ThreadCandidate
} from "./threading";

// The user is on every stored mail, as here: me@inbox.test on To or From.
const isOwn = (address: string) => address.toLowerCase().endsWith("@inbox.test");

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

describe("message-id spellings", () => {
  it("compares ids without their angle brackets", () => {
    expect(bareMessageId("<a@x>")).toBe("a@x");
    expect(bareMessageId("a@x")).toBe("a@x");
  });

  it("matches both stored spellings", () => {
    expect(storedIdForms(["<a@x>", "b@y"]).sort()).toEqual([
      "<a@x>",
      "<b@y>",
      "a@x",
      "b@y"
    ]);
  });
});

describe("linkedByHeaders", () => {
  it("links a reply to the message it answers", () => {
    expect(linkedByHeaders(mail("a"), mail("b", { in_reply_to: "<a@x>" }))).toBe(
      true
    );
  });

  it("links a reply to an appended original stored without brackets", () => {
    const appended = mail("a", { message_id: "a@x" });
    expect(linkedByHeaders(appended, mail("b", { in_reply_to: "<a@x>" }))).toBe(
      true
    );
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
  it("matches a header-less reply with the same subject and correspondent", () => {
    const original = mail("a", { subject: "Plan" });
    const reply = mail("b", {
      subject: "Re: Plan",
      from_address: [{ address: "me@inbox.test" }],
      to_address: [{ address: "Alice@x.com" }]
    });
    expect(matchesBySubject(original, reply, isOwn)).toBe(true);
  });

  // Two vendors answering two separate quotes, both to the same mailbox:
  // the only address they share is the user's own.
  it("keeps apart replies from different correspondents to the same mailbox", () => {
    const a = mail("a", {
      subject: "Re: Invoice",
      from_address: [{ address: "billing@vendor-a.com" }]
    });
    const b = mail("b", {
      subject: "RE: invoice",
      from_address: [{ address: "ar@vendor-b.com" }]
    });
    expect(matchesBySubject(a, b, isOwn)).toBe(false);
  });

  it("never matches mail that carries thread headers", () => {
    const a = mail("a", {
      subject: "Re: Invoice",
      in_reply_to: "<q1@inbox.test>"
    });
    const b = mail("b", {
      subject: "Re: Invoice",
      in_reply_to: "<q2@inbox.test>"
    });
    expect(matchesBySubject(a, b, isOwn)).toBe(false);
    expect(matchesBySubject(mail("c", { subject: "Re: Invoice" }), a, isOwn)).toBe(
      false
    );
  });

  it("does not match two non-replies with one subject", () => {
    expect(
      matchesBySubject(
        mail("a", { subject: "Weekly digest" }),
        mail("b", { subject: "Weekly digest" }),
        isOwn
      )
    ).toBe(false);
  });

  it("does not match very short subjects", () => {
    expect(
      matchesBySubject(mail("a", { subject: "Re: hi" }), mail("b", { subject: "hi" }), isOwn)
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
      collectThread(c, [a, b, c, other], isOwn).map((m) => m.mail_id).sort()
    ).toEqual(["a", "b", "c"]);
  });

  it("keeps two header-linked conversations with one subject apart", () => {
    const a = mail("a", {
      subject: "Re: Invoice",
      in_reply_to: "<q1@inbox.test>",
      from_address: [{ address: "billing@vendor-a.com" }]
    });
    const b = mail("b", {
      subject: "RE: invoice",
      in_reply_to: "<q2@inbox.test>",
      from_address: [{ address: "ar@vendor-b.com" }]
    });
    expect(collectThread(a, [a, b], isOwn).map((m) => m.mail_id)).toEqual(["a"]);
  });

  it("returns the seed alone when nothing matches", () => {
    const a = mail("a");
    expect(collectThread(a, [a, mail("z", { subject: "Other" })], isOwn)).toEqual([a]);
  });
});
