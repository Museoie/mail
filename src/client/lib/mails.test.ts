import { describe, it, expect } from "bun:test";
import { MailHeaderData } from "common";
import { Category } from "client";
import { isSentMail, canMarkSpam } from "./mails";

const makeMail = (fromAddress: string | undefined, sent = false) =>
  new MailHeaderData({
    from: fromAddress ? { value: [{ address: fromAddress }], text: fromAddress } : undefined,
    sent,
  });

describe("isSentMail", () => {
  it("is true for a sender on one of the user's domains", () => {
    expect(isSentMail(makeMail("me@example.com"), ["example.com"])).toBe(true);
    expect(isSentMail(makeMail("sales@example.org"), ["example.com", "example.org"])).toBe(true);
  });

  it("is false for a sender on another domain", () => {
    expect(isSentMail(makeMail("someone@elsewhere.test"), ["example.com"])).toBe(false);
    expect(isSentMail(makeMail("noreply@github.com"), ["example.com"])).toBe(false);
  });

  it("is case-insensitive on both the address and the domain", () => {
    expect(isSentMail(makeMail("Me@Example.Com"), ["example.com"])).toBe(true);
    expect(isSentMail(makeMail("me@example.com"), ["EXAMPLE.COM"])).toBe(true);
  });

  it("matches the whole domain, never one that merely contains it", () => {
    expect(isSentMail(makeMail("phish@example.com.attacker.test"), ["example.com"])).toBe(false);
    expect(isSentMail(makeMail("phish@fakeexample.com"), ["example.com"])).toBe(false);
    // A name under the domain is a different domain.
    expect(isSentMail(makeMail("bob@bob.example.com"), ["example.com"])).toBe(false);
  });

  it("is false when the from address or the domains are missing", () => {
    expect(isSentMail(makeMail(undefined), ["example.com"])).toBe(false);
    expect(isSentMail(makeMail("me@example.com"), [])).toBe(false);
    expect(isSentMail({ from: { value: [], text: "" } }, ["example.com"])).toBe(false);
  });
});

describe("canMarkSpam", () => {
  const DOMAIN = "hoie.kim";
  // Delivered over MX, so the server wrote sent = FALSE, but the unauthenticated
  // `From` header names the user's own domain.
  const spoofed = makeMail(`billing@${DOMAIN}`, false);
  // The user's own outbound copy, addressed to themselves.
  const selfAddressed = makeMail(`hoie@${DOMAIN}`, true);
  const outsider = makeMail("bad@spamtest.example", false);

  it("offers the toggle on a forged own-domain sender in every view", () => {
    const all = Object.values(Category);
    expect(all.map((c) => canMarkSpam(spoofed, c))).toEqual(all.map(() => true));
  });

  it("withholds the toggle on the user's own outbound mail outside the spam view", () => {
    const others = Object.values(Category).filter((c) => c !== Category.SpamMails);
    expect(others.map((c) => canMarkSpam(selfAddressed, c))).toEqual(
      others.map(() => false)
    );
  });

  it("offers the toggle on an outside sender in every view", () => {
    const all = Object.values(Category);
    expect(all.map((c) => canMarkSpam(outsider, c))).toEqual(all.map(() => true));
  });

  it("offers the un-mark toggle in the spam view even when the payload claims sent", () => {
    expect(canMarkSpam(new MailHeaderData({ sent: true }), Category.SpamMails)).toBe(
      true
    );
  });

  it("pins an explicit answer for every category, so a new member is visible here", () => {
    expect(
      Object.fromEntries(
        Object.values(Category).map((c) => [c, canMarkSpam(selfAddressed, c)])
      )
    ).toEqual({
      "All Mails": false,
      "Saved Mails": false,
      "Sent Mails": false,
      "Spam Mails": true,
      Search: false,
    });
  });
});
