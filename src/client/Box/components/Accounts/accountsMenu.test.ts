import { describe, it, expect } from "bun:test";
import { Category } from "client";
import { opensAccountsAfterFolderChange } from "./accountsMenu";

describe("opensAccountsAfterFolderChange", () => {
  it("opens the menu when a phone moves to another folder", () => {
    expect(
      opensAccountsAfterFolderChange(390, Category.AllMails, Category.SpamMails)
    ).toBe(true);
    expect(
      opensAccountsAfterFolderChange(750, Category.Search, Category.SentMails)
    ).toBe(true);
  });

  it("leaves it closed on wider layouts", () => {
    expect(
      opensAccountsAfterFolderChange(751, Category.AllMails, Category.SpamMails)
    ).toBe(false);
  });

  it("leaves it closed when the folder doesn't change", () => {
    expect(
      opensAccountsAfterFolderChange(390, Category.AllMails, Category.AllMails)
    ).toBe(false);
  });

  it("leaves it closed for Search", () => {
    expect(
      opensAccountsAfterFolderChange(390, Category.AllMails, Category.Search)
    ).toBe(false);
  });
});
