import { describe, it, expect } from "bun:test";
import { MailHeaderData } from "common";
import {
  conversationSenders,
  groupConversations,
  ownAddressesFor
} from "./conversations";

const isOwn = ownAddressesFor("admin", "inbox.test");

const mail = (
  id: string,
  day: number,
  fields: Partial<MailHeaderData> = {}
) =>
  new MailHeaderData({
    id,
    messageId: `<${id}@x>`,
    subject: "Plan",
    date: `2026-01-${String(day).padStart(2, "0")}T00:00:00.000Z`,
    read: true,
    from: { value: [{ address: "alice@x.com", name: "Alice" }], text: "Alice" },
    to: { value: [{ address: "me@inbox.test" }], text: "me@inbox.test" },
    ...fields
  });

const ids = (rows: { mails: MailHeaderData[] }[]) =>
  rows.map((c) => c.mails.map((m) => m.id).join(","));

describe("groupConversations", () => {
  it("groups replies with what they answer, newest conversation first", () => {
    const rows = groupConversations(
      [
        mail("c", 3, { inReplyTo: "<b@x>", references: ["<a@x>", "<b@x>"] }),
        mail("z", 2, { subject: "Other" }),
        mail("a", 1)
      ],
      isOwn
    );
    expect(ids(rows)).toEqual(["c,a", "z"]);
    expect(rows[0].latest.id).toBe("c");
  });

  it("links through a shared References root when the middle message is elsewhere", () => {
    // b (the user's sent reply) lives in Sent, not in this list.
    const rows = groupConversations(
      [
        mail("c", 3, { inReplyTo: "<b@x>", references: ["<a@x>", "<b@x>"] }),
        mail("a", 1)
      ],
      isOwn
    );
    expect(ids(rows)).toEqual(["c,a"]);
  });

  it("matches header-less replies by subject and correspondent", () => {
    const rows = groupConversations(
      [mail("b", 2, { subject: "Re: Plan", messageId: "b@x" }), mail("a", 1)],
      isOwn
    );
    expect(ids(rows)).toEqual(["b,a"]);
  });

  it("keeps apart two vendors' same-subject replies that share only the user", () => {
    const rows = groupConversations(
      [
        mail("a", 2, {
          subject: "Re: Invoice",
          from: { value: [{ address: "billing@vendor-a.com" }], text: "" }
        }),
        mail("b", 1, {
          subject: "RE: invoice",
          from: { value: [{ address: "ar@vendor-b.com" }], text: "" }
        })
      ],
      isOwn
    );
    expect(ids(rows)).toEqual(["a", "b"]);
  });

  it("is unread or saved when any message is", () => {
    const [row] = groupConversations(
      [
        mail("b", 2, { inReplyTo: "<a@x>", read: false }),
        mail("a", 1, { saved: true })
      ],
      isOwn
    );
    expect(row.unread).toBe(true);
    expect(row.saved).toBe(true);
  });

  it("leaves each message on its own row when nothing links them", () => {
    const rows = groupConversations(
      [
        mail("a", 2, { subject: "Weekly digest" }),
        mail("b", 1, { subject: "Weekly digest" })
      ],
      isOwn
    );
    expect(ids(rows)).toEqual(["a", "b"]);
  });

  it("is empty for an empty list", () => {
    expect(groupConversations([], isOwn)).toEqual([]);
  });
});

describe("conversationSenders", () => {
  it("lists each sender once in reading order, with the user as me", () => {
    const [row] = groupConversations(
      [
        mail("c", 3, { inReplyTo: "<b@x>" }),
        mail("b", 2, {
          inReplyTo: "<a@x>",
          from: { value: [{ address: "me@inbox.test" }], text: "" }
        }),
        mail("a", 1)
      ],
      isOwn
    );
    expect(
      conversationSenders(row, (m) => m.from?.value?.[0]?.address === "me@inbox.test")
    ).toBe("Alice, me");
  });
});

describe("ownAddressesFor", () => {
  it("is the user's own mail domain", () => {
    expect(ownAddressesFor("bob", "inbox.test")("x@bob.inbox.test")).toBe(true);
    expect(ownAddressesFor("bob", "inbox.test")("x@inbox.test")).toBe(false);
    expect(ownAddressesFor("admin", "inbox.test")("x@inbox.test")).toBe(true);
  });
});
