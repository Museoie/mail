import { describe, it, expect } from "bun:test";
import { Account } from "common";
import { Category } from "client";
import {
  accountBadgeCount,
  displayNameFor,
  duplicatedDisplayNames,
  filterAccounts
} from "./accountList";

const makeAccount = (key: string, unread = 0, saved = 0) =>
  new Account({
    key,
    updated: new Date("2026-01-01"),
    doc_count: 1,
    unread_doc_count: unread,
    saved_doc_count: saved
  });

describe("displayNameFor", () => {
  it("takes the local part of the address", () => {
    expect(displayNameFor("annie@x.com")).toBe("annie");
    expect(displayNameFor("maple@shop.example")).toBe("maple");
  });

  it("falls back to Unknown when there is no local part", () => {
    expect(displayNameFor("")).toBe("Unknown");
    expect(displayNameFor("@x.com")).toBe("Unknown");
  });
});

describe("duplicatedDisplayNames", () => {
  it("returns the display names that appear more than once", () => {
    const accounts = [
      makeAccount("annie@x.com"),
      makeAccount("annie@y.com"),
      makeAccount("maple@x.com")
    ];
    expect(duplicatedDisplayNames(accounts)).toEqual(new Set(["annie"]));
  });

  it("returns an empty set when every name is unique", () => {
    const accounts = [makeAccount("annie@x.com"), makeAccount("maple@x.com")];
    expect(duplicatedDisplayNames(accounts)).toEqual(new Set());
  });

  it("treats two Unknowns as duplicates", () => {
    const accounts = [makeAccount(""), makeAccount("@x.com")];
    expect(duplicatedDisplayNames(accounts)).toEqual(new Set(["Unknown"]));
  });
});

describe("filterAccounts", () => {
  const accounts = [
    makeAccount("annie@x.com"),
    makeAccount("Annie@y.com"),
    makeAccount("maple@shop.example")
  ];

  it("matches a case-insensitive substring of the address", () => {
    expect(filterAccounts(accounts, "ANNIE").map((a) => a.key)).toEqual([
      "annie@x.com",
      "Annie@y.com"
    ]);
    expect(filterAccounts(accounts, "shop").map((a) => a.key)).toEqual([
      "maple@shop.example"
    ]);
  });

  it("returns everything for an empty or blank filter", () => {
    expect(filterAccounts(accounts, "")).toEqual(accounts);
    expect(filterAccounts(accounts, "   ")).toEqual(accounts);
  });

  it("returns an empty list when nothing matches", () => {
    expect(filterAccounts(accounts, "zzz")).toEqual([]);
  });

  it("keeps the input order", () => {
    expect(filterAccounts(accounts, "x.com").map((a) => a.key)).toEqual([
      "annie@x.com"
    ]);
  });
});

describe("accountBadgeCount", () => {
  it("reads unread everywhere except Saved", () => {
    const account = makeAccount("me@x.com", 3, 2);
    expect(accountBadgeCount(account, Category.NewMails)).toBe(3);
    expect(accountBadgeCount(account, Category.AllMails)).toBe(3);
    expect(accountBadgeCount(account, Category.SentMails)).toBe(3);
    expect(accountBadgeCount(account, Category.SpamMails)).toBe(3);
  });

  it("reads saved for Saved", () => {
    const account = makeAccount("me@x.com", 3, 2);
    expect(accountBadgeCount(account, Category.SavedMails)).toBe(2);
  });
});
