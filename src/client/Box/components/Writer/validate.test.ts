import { describe, it, expect } from "bun:test";
import { invalidAddresses } from "./validate";

describe("invalidAddresses", () => {
  it("accepts a comma-separated list of addresses", () => {
    expect(invalidAddresses("a@x.com, b.c@y.co.uk")).toEqual([]);
  });

  it("accepts semicolons and a Name <address> entry", () => {
    expect(invalidAddresses("Bob <bob@x.com>; c@x.com")).toEqual([]);
  });

  it("ignores empty entries and blank input", () => {
    expect(invalidAddresses("")).toEqual([]);
    expect(invalidAddresses("a@x.com, ,")).toEqual([]);
  });

  it("reports each entry that isn't an address", () => {
    expect(invalidAddresses("not-an-email, a@x.com, b@nodot, @x.com")).toEqual([
      "not-an-email",
      "b@nodot",
      "@x.com"
    ]);
  });
});
