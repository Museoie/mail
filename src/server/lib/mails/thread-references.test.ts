import { describe, it, expect } from "bun:test";
import { threadReferences } from "./send";

describe("threadReferences", () => {
  it("appends the answered id to the answered mail's References", async () => {
    const refs = await threadReferences("u", "<b@x>", async () => ["<a@x>"]);
    expect(refs).toEqual(["<a@x>", "<b@x>"]);
  });

  it("is just the answered id when that mail has no References", async () => {
    expect(await threadReferences("u", "<b@x>", async () => null)).toEqual([
      "<b@x>"
    ]);
  });

  it("doesn't repeat the answered id", async () => {
    expect(
      await threadReferences("u", "<b@x>", async () => ["<a@x>", "<b@x>"])
    ).toEqual(["<a@x>", "<b@x>"]);
  });

  it("still threads the reply when the lookup fails", async () => {
    expect(
      await threadReferences("u", "<b@x>", async () => {
        throw new Error("db down");
      })
    ).toEqual(["<b@x>"]);
  });

  it("keeps the newest 50 ids", async () => {
    const earlier = Array.from({ length: 60 }, (_, i) => `<m${i}@x>`);
    const refs = await threadReferences("u", "<b@x>", async () => earlier);
    expect(refs.length).toBe(50);
    expect(refs[refs.length - 1]).toBe("<b@x>");
  });
});
