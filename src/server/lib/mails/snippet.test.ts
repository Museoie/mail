import { describe, it, expect } from "bun:test";
import { buildSnippet, SNIPPET_MAX_LENGTH } from "./snippet";

describe("buildSnippet", () => {
  it("passes short plain text through unchanged", () => {
    expect(buildSnippet("Hello, this is the body.")).toBe(
      "Hello, this is the body."
    );
  });

  it("collapses newlines and extra whitespace into one line", () => {
    expect(
      buildSnippet("First line\nsecond   line\n\n  third line  ")
    ).toBe("First line second line third line");
  });

  it("strips quoted lines including nested chains", () => {
    expect(
      buildSnippet(
        "My reply is here.\n> Quoted original\n>> Nested quote\n> More quote"
      )
    ).toBe("My reply is here.");
  });

  it("drops the 'On ... wrote:' attribution line", () => {
    expect(
      buildSnippet(
        "See below.\nOn Tue, Jan 5, 2021 at 3:00 PM, John <john@x.com> wrote:\n> old text"
      )
    ).toBe("See below.");
  });

  it("drops a French 'a écrit :' attribution line", () => {
    expect(
      buildSnippet(
        "See below.\nLe 5 janv. 2021 à 15:00, Jean <jean@x.com> a écrit :\n> old text"
      )
    ).toBe("See below.");
  });

  it("stops at an Original Message separator", () => {
    expect(
      buildSnippet(
        "New content here.\n-----Original Message-----\nFrom: old@x.com\nOld body"
      )
    ).toBe("New content here.");
  });

  it("stops at a forwarded-message separator", () => {
    expect(
      buildSnippet(
        "Forwarding this along.\nBegin forwarded message:\nFrom: old@x.com"
      )
    ).toBe("Forwarding this along.");
  });

  it("stops at a Gmail forwarded-message separator", () => {
    expect(
      buildSnippet(
        "See below.\n---------- Forwarded message ---------\nFrom: old@x.com\nOld body"
      )
    ).toBe("See below.");
  });

  it("returns undefined for a forward with no added text", () => {
    expect(
      buildSnippet(
        "---------- Forwarded message ---------\nFrom: old@x.com\nOld body"
      )
    ).toBeUndefined();
  });

  it("truncates long text at a word boundary with an ellipsis", () => {
    const long = Array.from({ length: 30 }, (_, i) => `word${i}`).join(" ");
    const snippet = buildSnippet(long);
    expect(snippet).toBeDefined();
    expect(snippet!.length).toBeLessThanOrEqual(SNIPPET_MAX_LENGTH + 1);
    expect(snippet!.endsWith("…")).toBe(true);
    // The cut lands on a word boundary: no partial word before the ellipsis.
    expect(snippet!.slice(0, -1).endsWith(" ")).toBe(false);
    expect(long.startsWith(snippet!.slice(0, -1))).toBe(true);
  });

  it("keeps text that fits the limit exactly", () => {
    const exact = "x".repeat(SNIPPET_MAX_LENGTH);
    expect(buildSnippet(exact)).toBe(exact);
  });

  it("returns undefined for empty, blank, or all-quoted text", () => {
    expect(buildSnippet("")).toBeUndefined();
    expect(buildSnippet("   \n  ")).toBeUndefined();
    expect(buildSnippet("> only quotes\n> here")).toBeUndefined();
    expect(buildSnippet(undefined)).toBeUndefined();
    expect(buildSnippet(null)).toBeUndefined();
  });
});
