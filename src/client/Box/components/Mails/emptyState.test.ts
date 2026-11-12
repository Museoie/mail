import { describe, it, expect } from "bun:test";
import { Account } from "common";
import { Category } from "client";
import { emptyFolderState } from "./emptyState";

const account = (
  key: string,
  updated: string,
  counts: { doc?: number; unread?: number; saved?: number } = {}
) =>
  new Account({
    key,
    updated: new Date(updated),
    doc_count: counts.doc ?? 1,
    unread_doc_count: counts.unread ?? 0,
    saved_doc_count: counts.saved ?? 0
  });

const accounts = {
  received: [
    account("shop@x.com", "2026-01-01", { doc: 3, unread: 0, saved: 0 }),
    account("bank@x.com", "2026-02-01", { doc: 2, unread: 1, saved: 2 })
  ],
  sent: [],
  spam: [
    account("old@x.com", "2026-01-01", { doc: 1 }),
    account("new@x.com", "2026-03-01", { doc: 4 })
  ]
};

describe("emptyFolderState", () => {
  it("names the selected account in the message", () => {
    expect(emptyFolderState(Category.SpamMails, "shop@x.com", accounts).message)
      .toBe("No spam in shop@x.com.");
  });

  it("lists the accounts with mail in this folder, newest first", () => {
    expect(
      emptyFolderState(Category.SpamMails, "shop@x.com", accounts).others
    ).toEqual([
      { key: "new@x.com", count: 4 },
      { key: "old@x.com", count: 1 }
    ]);
  });

  it("counts saved mail for Saved", () => {
    expect(
      emptyFolderState(Category.SavedMails, "shop@x.com", accounts).others
    ).toEqual([{ key: "bank@x.com", count: 2 }]);
  });

  it("never offers the selected account itself", () => {
    expect(
      emptyFolderState(Category.SpamMails, "new@x.com", accounts).others.map(
        (a) => a.key
      )
    ).toEqual(["old@x.com"]);
  });

  it("offers nothing before the accounts payload loads", () => {
    expect(
      emptyFolderState(Category.SpamMails, "shop@x.com", undefined).others
    ).toEqual([]);
  });

  it("keeps search's own message and offers no accounts there", () => {
    expect(emptyFolderState(Category.Search, "term", accounts)).toEqual({
      message: "No results found.",
      others: []
    });
  });

  it("names the unified view and offers no other accounts there", () => {
    // The unified view already spans every account, so there are no
    // "other accounts" left to list — and the message names the view,
    // not the parked account behind it.
    expect(
      emptyFolderState(Category.AllMails, "shop@x.com", accounts, true)
    ).toEqual({
      message: "No mail in all accounts.",
      others: []
    });
  });
});
