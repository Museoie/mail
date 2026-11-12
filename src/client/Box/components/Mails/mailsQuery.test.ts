import { describe, it, expect } from "bun:test";
import { Category } from "client";
import { getMailsQueryUrl } from "./mailsQuery";

describe("getMailsQueryUrl", () => {
  it("routes the account-scoped categories to /headers with the right flag", () => {
    expect(getMailsQueryUrl("alice@x.com", Category.AllMails)).toBe(
      "/api/mails/headers/alice@x.com"
    );
    expect(getMailsQueryUrl("alice@x.com", Category.SentMails)).toBe(
      "/api/mails/headers/alice@x.com?sent=1"
    );
    expect(getMailsQueryUrl("alice@x.com", Category.SavedMails)).toBe(
      "/api/mails/headers/alice@x.com?saved=1"
    );
    // Spam is account-scoped like received/sent — it groups per receiving account.
    expect(getMailsQueryUrl("alice@x.com", Category.SpamMails)).toBe(
      "/api/mails/headers/alice@x.com?spam=1"
    );
  });

  it("routes Search to the encoded search endpoint", () => {
    expect(getMailsQueryUrl("a b@x.com", Category.Search)).toBe(
      "/api/mails/search/a%20b%40x.com"
    );
  });

  it("routes the unified view to /unified, naming no account", () => {
    expect(getMailsQueryUrl("alice@x.com", Category.AllMails, true)).toBe(
      "/api/mails/unified"
    );
  });

  it("keeps Search per-account even from the unified view", () => {
    // Search stays per-account (design doc §8): the search branch wins over
    // the flag, so entering search parks the account as usual.
    expect(getMailsQueryUrl("alice@x.com", Category.Search, true)).toBe(
      "/api/mails/search/alice%40x.com"
    );
  });

  it("keeps the other folders per-account when the flag is off", () => {
    expect(getMailsQueryUrl("alice@x.com", Category.AllMails, false)).toBe(
      "/api/mails/headers/alice@x.com"
    );
  });
});
