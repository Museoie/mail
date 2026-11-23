import { describe, it, expect, afterAll, beforeAll } from "bun:test";
import fs from "fs";
import os from "os";
import path from "path";

import type { IncomingMailAddress } from "common";
import {
  convertMailAddress,
  convertAddressValue,
  saveFile,
} from "./receive";
import { getAttachmentFilePath } from "./util";

// Mirror of saveIncomingMail's spam-context extraction.
const extract = (from?: IncomingMailAddress | IncomingMailAddress[], replyTo?: IncomingMailAddress | IncomingMailAddress[]) => {
  const f = convertMailAddress(from);
  const r = convertMailAddress(replyTo);
  return {
    fromAddress: f?.value?.[0]?.address,
    fromName: f?.text,
    replyToAddress: r?.value?.[0]?.address,
  };
};

describe("convertMailAddress (spam EmailContext source, #528)", () => {
  it("normalizes a single-object address: lowercases the address, preserves text", () => {
    const result = convertMailAddress({
      value: { address: "Sender@Example.COM", name: "Sender" },
      text: "Sender <Sender@Example.COM>",
    } as unknown as IncomingMailAddress);
    expect(result?.value?.[0]?.address).toBe("sender@example.com");
    expect(result?.value?.[0]?.name).toBe("Sender");
    expect(result?.text).toBe("Sender <Sender@Example.COM>");
  });

  it("normalizes an array address: first address from the flattened value, text joined with ', '", () => {
    const result = convertMailAddress([
      { value: [{ address: "First@X.com" }], text: "First" },
      { value: [{ address: "Second@Y.com" }], text: "Second" },
    ] as unknown as IncomingMailAddress[]);
    expect(result?.value?.[0]?.address).toBe("first@x.com");
    expect(result?.value?.[1]?.address).toBe("second@y.com");
    expect(result?.text).toBe("First, Second");
  });

  it("returns undefined for missing input", () => {
    expect(convertMailAddress(undefined)).toBeUndefined();
  });

  it("returns undefined for an empty array", () => {
    expect(convertMailAddress([])).toBeUndefined();
  });

  it("EmailContext extraction: single from + single replyTo → lowercased addresses", () => {
    expect(
      extract(
        { value: { address: "From@A.com" }, text: "from" } as unknown as IncomingMailAddress,
        { value: { address: "Reply@B.com" }, text: "reply" } as unknown as IncomingMailAddress,
      ),
    ).toEqual({
      fromAddress: "from@a.com",
      fromName: "from",
      replyToAddress: "reply@b.com",
    });
  });

  it("EmailContext extraction: array from + array replyTo → first address of each", () => {
    expect(
      extract(
        [
          { value: [{ address: "F1@A.com" }], text: "f1" },
          { value: [{ address: "F2@A.com" }], text: "f2" },
        ] as unknown as IncomingMailAddress[],
        [
          { value: [{ address: "R1@B.com" }], text: "r1" },
          { value: [{ address: "R2@B.com" }], text: "r2" },
        ] as unknown as IncomingMailAddress[],
      ),
    ).toEqual({
      fromAddress: "f1@a.com",
      fromName: "f1, f2",
      replyToAddress: "r1@b.com",
    });
  });

  it("EmailContext extraction: absent from/replyTo → undefined fields (no crash)", () => {
    expect(extract(undefined, undefined)).toEqual({
      fromAddress: undefined,
      fromName: undefined,
      replyToAddress: undefined,
    });
  });
});

describe("convertAddressValue", () => {
  it("returns undefined for empty / nullish input", () => {
    expect(convertAddressValue(undefined)).toBeUndefined();
    expect(convertAddressValue([])).toBeUndefined();
  });

  it("lowercases and keeps a simple address", () => {
    expect(convertAddressValue({ address: "Alice@Example.COM", name: "Alice" })).toEqual([
      { address: "alice@example.com", name: "Alice" },
    ]);
  });

  it("drops empty / whitespace-only addresses instead of storing { address: '' } (#535)", () => {
    expect(convertAddressValue({ address: "" })).toBeUndefined();
    expect(convertAddressValue({ address: "   ", name: "Nobody" })).toBeUndefined();
    expect(
      convertAddressValue([{ address: "" }, { address: "bob@example.com" }])
    ).toEqual([{ address: "bob@example.com", name: undefined }]);
  });

  it("expands RFC 2822 group members instead of dropping them (#535)", () => {
    const grouped = {
      address: "",
      name: "Team",
      group: [
        { address: "Alice@Example.com", name: "Alice" },
        { address: "bob@example.com", name: "Bob" },
      ],
    };
    expect(convertAddressValue(grouped)).toEqual([
      { address: "alice@example.com", name: "Alice" },
      { address: "bob@example.com", name: "Bob" },
    ]);
  });

  it("expands a single (non-array) group member", () => {
    expect(
      convertAddressValue({ address: "", group: { address: "solo@example.com", name: "Solo" } })
    ).toEqual([{ address: "solo@example.com", name: "Solo" }]);
  });

  it("splits comma-concatenated addresses into separate rows (#535)", () => {
    expect(
      convertAddressValue({
        address: "annie@bratko.net, presidential@hoie.kim",
        name: "Both",
      })
    ).toEqual([
      { address: "annie@bratko.net", name: "Both" },
      { address: "presidential@hoie.kim", name: "Both" },
    ]);
  });

  it("drops empty fragments produced by a trailing comma", () => {
    expect(convertAddressValue({ address: "a@x.com, , b@x.com" })).toEqual([
      { address: "a@x.com", name: undefined },
      { address: "b@x.com", name: undefined },
    ]);
  });
});

describe("saveFile", () => {
  let sourceDir: string;
  const saved: string[] = [];

  beforeAll(() => {
    sourceDir = fs.mkdtempSync(path.join(os.tmpdir(), "receive-save-file-"));
  });

  afterAll(() => {
    fs.rmSync(sourceDir, { recursive: true, force: true });
    saved.forEach((id) => fs.rmSync(getAttachmentFilePath(id), { force: true }));
  });

  it("stores a copy that outlives the source file", async () => {
    const source = path.join(sourceDir, "0");
    fs.writeFileSync(source, "attachment bytes");

    const first = await saveFile(source);
    const second = await saveFile(source);
    saved.push(first, second);
    fs.rmSync(source);

    expect(first).not.toBe(second);
    expect(fs.readFileSync(getAttachmentFilePath(first), "utf8")).toBe("attachment bytes");
    expect(fs.readFileSync(getAttachmentFilePath(second), "utf8")).toBe("attachment bytes");
  });
});
