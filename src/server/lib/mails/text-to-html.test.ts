import { describe, it, expect } from "bun:test";
import { bodyHtml, plainTextToHtml } from "./text-to-html";

describe("plainTextToHtml", () => {
  it("keeps line breaks inside a pre-wrap container", () => {
    const out = plainTextToHtml("one\ntwo\r\nthree");
    expect(out).toBe('<div style="white-space: pre-wrap">one\ntwo\nthree</div>');
  });

  it("escapes markup and entity-like runs so they show literally", () => {
    const out = plainTextToHtml("a < b && c > d &para;");
    expect(out).toContain("a &lt; b &amp;&amp; c &gt; d &amp;para;");
  });

  it("links bare URLs, keeping the query string intact", () => {
    const out = plainTextToHtml("see https://example.com/x?with=query&params=1 now");
    expect(out).toContain(
      '<a href="https://example.com/x?with=query&amp;params=1">https://example.com/x?with=query&amp;params=1</a> now'
    );
  });

  it("leaves sentence punctuation out of a link", () => {
    expect(plainTextToHtml("go to https://example.com/a.")).toContain(
      '<a href="https://example.com/a">https://example.com/a</a>.'
    );
  });

  it("does not link non-http schemes", () => {
    expect(plainTextToHtml("javascript:alert(1)")).not.toContain("<a");
  });

  it("wraps quoted lines in a blockquote per level", () => {
    const out = plainTextToHtml("hi\n> one\n>> two\n> three\nbye");
    expect(out).toBe(
      '<div style="white-space: pre-wrap">hi<blockquote>one<blockquote>two</blockquote>three</blockquote>bye</div>'
    );
  });

  it("closes blockquotes still open at the end", () => {
    expect(plainTextToHtml("> a")).toBe(
      '<div style="white-space: pre-wrap"><blockquote>a</blockquote></div>'
    );
  });
});

describe("bodyHtml", () => {
  it("converts a body whose html column holds the plain text", () => {
    expect(bodyHtml("a\nb", "a\nb")).toBe(plainTextToHtml("a\nb"));
  });

  it("converts a body with no html", () => {
    expect(bodyHtml("", "text")).toBe(plainTextToHtml("text"));
    expect(bodyHtml(null, "text")).toBe(plainTextToHtml("text"));
  });

  it("returns real html untouched", () => {
    expect(bodyHtml("<p>a</p>", "a")).toBe("<p>a</p>");
  });

  it("returns an empty string when both are empty", () => {
    expect(bodyHtml(null, null)).toBe("");
    expect(bodyHtml("", "")).toBe("");
  });
});
