import { describe, it, expect } from "bun:test";
import { Account } from "common";
import { fromOptions } from "./fromOptions";

const account = (key: string) =>
  new Account({
    key,
    updated: new Date("2026-01-01"),
    doc_count: 1,
    unread_doc_count: 0,
    saved_doc_count: 0
  });

const accounts = {
  received: [
    account("maple@example.com"),
    account("news@elsewhere.com"),
    account("fern@example.com")
  ],
  sent: [account("maple@example.com"), account("billing@Example.com")],
  spam: [account("spam@example.com")],
  shared: []
};

describe("fromOptions", () => {
  it("lists the domains' received and sent accounts once each, sorted, without spam", () => {
    expect(fromOptions(accounts, ["example.com"], "")).toEqual([
      "billing@Example.com",
      "fern@example.com",
      "maple@example.com"
    ]);
  });

  it("keeps the saved choice even when no account lists it", () => {
    expect(fromOptions(undefined, ["example.com"], "old@example.com")).toEqual([
      "old@example.com"
    ]);
  });

  it("offers nothing from the accounts before the domains are known", () => {
    expect(fromOptions(accounts, [], "")).toEqual([]);
  });

  it("offers every domain the user owns, and never one they only view", () => {
    const withShared = {
      ...accounts,
      received: [...accounts.received, account("info@viewed.test")],
      sent: [...accounts.sent, account("hello@example.org")]
    };
    expect(fromOptions(withShared, ["example.com", "example.org"], "")).toEqual([
      "billing@Example.com",
      "fern@example.com",
      "hello@example.org",
      "maple@example.com"
    ]);
  });
});
