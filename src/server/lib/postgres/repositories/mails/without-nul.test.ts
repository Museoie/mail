import { describe, expect, it } from "bun:test";
import { Attachment } from "common";
import { withoutNul } from "./core";

// Regression: a NUL anywhere in a message (PostgreSQL text cannot hold one,
// and JSONB refuses `\u0000`) failed the INSERT. SMTP answered that with a
// transient 450, so the sender retried a message that could never be stored.
describe("withoutNul", () => {
  it("takes the character out of every string, at any depth", () => {
    expect(
      withoutNul({
        message_id: "<id\0@x>",
        subject: "nu\0l",
        text: "a\0b\0",
        from_address: [{ address: "jo@remote.example", name: "Jo\0e" }],
        reference_ids: ["<r\0@x>"],
        insight: { tags: ["x\0", 1, null, true] },
        read: true,
        uid_domain: 3,
        in_reply_to: null,
        html: undefined
      })
    ).toEqual({
      message_id: "<id@x>",
      subject: "nul",
      text: "ab",
      from_address: [{ address: "jo@remote.example", name: "Joe" }],
      reference_ids: ["<r@x>"],
      insight: { tags: ["x", 1, null, true] },
      read: true,
      uid_domain: 3,
      in_reply_to: null,
      html: undefined
    });
  });

  it("leaves keys alone: they are the row's, not the message's", () => {
    expect(withoutNul({ "a\0": "b\0" })).toEqual({ "a\0": "b" });
  });

  it("writes an attachment model as the same JSON, without the character", () => {
    const attachment = new Attachment({
      filename: "re\0port.pdf",
      contentType: "application/pdf",
      content: { data: "id" },
      size: 4
    });
    expect(JSON.stringify(withoutNul([attachment]))).toBe(
      JSON.stringify([{ ...attachment, filename: "report.pdf" }])
    );
  });

  it("is applied to the input before saveMail reads any of it", async () => {
    const source = await Bun.file(new URL("./core.ts", import.meta.url)).text();
    const saveMail = source.match(/export const saveMail[\s\S]*?\n};/)![0];
    const stripped = saveMail.indexOf("const input = withoutNul(given);");
    expect(stripped).toBeGreaterThan(-1);
    expect(saveMail.slice(0, stripped)).not.toMatch(/\binput\b/);
    expect(saveMail.slice(stripped)).not.toMatch(/\bgiven\b(?!\))/);
  });
});
