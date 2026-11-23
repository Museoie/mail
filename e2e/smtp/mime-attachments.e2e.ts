import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import crypto from "crypto";
import { waitFor, type TestServer } from "../harness";
import { FIXED_DATE, Mailer, attachmentBytes, boot, crlf, incomingLeftovers, octets, sha256, type Row } from "./support";

// Which parts of a message become attachments, what the row says about them,
// and what ends up in the files.
let server: TestServer;
let mailer: Mailer;

beforeAll(async () => {
  server = await boot();
  mailer = new Mailer(server);
}, 180_000);

afterAll(async () => {
  mailer?.close();
  await server?.stop();
});

const HEAD = ["From: Remote Sender <sender@remote.example>", "To: hello@mail.test", "Subject: Files", `Date: ${FIXED_DATE}`];
const PNG = Buffer.from(
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==",
  "base64"
);

const files = (mail: Row) =>
  (mail.attachments as { filename: string; contentType: string; size: number }[]).map(
    ({ filename, contentType, size }) => ({ filename, contentType, size })
  );
const fileBytes = (mail: Row, index = 0) => attachmentBytes(server, mail.attachments[index].content.data);

/** A multipart/mixed message: a line of text, then each part as given. */
const mixed = (...parts: (string | Uint8Array)[][]) =>
  octets(
    crlf(...HEAD, 'Content-Type: multipart/mixed; boundary="mix"', "", "--mix", "Content-Type: text/plain", "", "See attached."),
    ...parts.flatMap((part) => ["--mix\r\n", ...part, "\r\n"]),
    "--mix--\r\n"
  );

/** One part: header lines, a blank line, then the content exactly as given. */
const part = (headers: string[], ...content: (string | Uint8Array)[]) => [crlf(...headers, ""), ...content];

/** Stores a message with one attachment part and returns the first attachment's description. */
const attach = async (headers: string[], content: string | Uint8Array = "x") => {
  const mail = await mailer.store(mixed(part(headers, content)));
  return files(mail)[0];
};

describe("the attachments column", () => {
  test("is an empty array for a message without attachments", async () => {
    const mail = await mailer.store(crlf(...HEAD, "", "No files."));
    expect(mail.attachments).toEqual([]);
  });

  test("lists each attachment as filename, contentType, size and the id of its file, in message order", async () => {
    const mail = await mailer.store(
      mixed(
        part(['Content-Type: image/png; name="pixel.png"', "Content-Transfer-Encoding: base64", 'Content-Disposition: attachment; filename="pixel.png"'], PNG.toString("base64")),
        part(["Content-Type: application/json", 'Content-Disposition: attachment; filename="data.json"'], '{"a":1}'),
        part(["Content-Type: text/csv", 'Content-Disposition: attachment; filename="rows.csv"'], "a,b\r\n1,2")
      )
    );
    expect(mail.text).toBe("See attached.");
    expect(mail.attachments).toEqual([
      { filename: "pixel.png", contentType: "image/png", size: PNG.length, content: { data: expect.stringMatching(/^[0-9a-f-]{36}$/) } },
      { filename: "data.json", contentType: "application/json", size: 7, content: { data: expect.stringMatching(/^[0-9a-f-]{36}$/) } },
      { filename: "rows.csv", contentType: "text/csv", size: 8, content: { data: expect.stringMatching(/^[0-9a-f-]{36}$/) } },
    ]);
    expect(fileBytes(mail, 0).equals(PNG)).toBe(true);
    expect(fileBytes(mail, 1).toString()).toBe('{"a":1}');
    expect(fileBytes(mail, 2).toString()).toBe("a,b\r\n1,2");
    expect(new Set(mail.attachments.map((entry: { content: { data: string } }) => entry.content.data)).size).toBe(3);
  });

  test("two attachments with the same name are both kept", async () => {
    const mail = await mailer.store(
      mixed(
        part(["Content-Type: text/csv", 'Content-Disposition: attachment; filename="same.csv"'], "one"),
        part(["Content-Type: text/csv", 'Content-Disposition: attachment; filename="same.csv"'], "two!")
      )
    );
    expect(files(mail)).toEqual([
      { filename: "same.csv", contentType: "text/csv", size: 3 },
      { filename: "same.csv", contentType: "text/csv", size: 4 },
    ]);
  });

  test("an empty attachment is a zero-length file", async () => {
    const mail = await mailer.store(mixed(part(["Content-Type: application/pdf", 'Content-Disposition: attachment; filename="empty.pdf"'], "")));
    expect(files(mail)).toEqual([{ filename: "empty.pdf", contentType: "application/pdf", size: 0 }]);
    expect(fileBytes(mail).length).toBe(0);
  });

  test("a message that is one attachment and nothing else has empty text", async () => {
    const mail = await mailer.store(
      crlf(...HEAD, "Content-Type: application/pdf", 'Content-Disposition: attachment; filename="doc.pdf"', "Content-Transfer-Encoding: base64", "", Buffer.from("%PDF-1.4").toString("base64"))
    );
    expect([mail.text, mail.html]).toEqual(["", ""]);
    expect(files(mail)).toEqual([{ filename: "doc.pdf", contentType: "application/pdf", size: 8 }]);
    expect(fileBytes(mail).toString()).toBe("%PDF-1.4");
  });

  test("a multipart of attachments only has empty text", async () => {
    const mail = await mailer.store(
      crlf(
        ...HEAD,
        'Content-Type: multipart/mixed; boundary="only"',
        "",
        "--only",
        "Content-Type: application/zip",
        'Content-Disposition: attachment; filename="a.zip"',
        "Content-Transfer-Encoding: base64",
        "",
        "UEsFBgAAAAAAAAAAAAAAAAAAAAAAAA==",
        "--only",
        'Content-Type: image/png; name="b.png"',
        "Content-Transfer-Encoding: base64",
        "",
        PNG.toString("base64"),
        "--only--"
      )
    );
    expect([mail.text, mail.html]).toEqual(["", ""]);
    expect(files(mail)).toEqual([
      { filename: "a.zip", contentType: "application/zip", size: 22 },
      { filename: "b.png", contentType: "image/png", size: PNG.length },
    ]);
  });
});

describe("transfer encodings of an attachment", () => {
  const every = Buffer.from(Array.from({ length: 256 }, (_, i) => i));

  test("base64, wrapped at any width, decodes to the original octets", async () => {
    const data = Buffer.concat([every, every, Buffer.from("tail")]);
    for (const width of [76, 60, 1000]) {
      const encoded = data.toString("base64").replace(new RegExp(`(.{${width}})`, "g"), "$1\r\n");
      const mail = await mailer.store(mixed(part(["Content-Type: application/octet-stream", "Content-Transfer-Encoding: base64"], encoded)));
      expect({ width, ...files(mail)[0] }).toEqual({ width, filename: "attachment", contentType: "application/octet-stream", size: data.length });
      expect(fileBytes(mail).equals(data)).toBe(true);
    }
  });

  test("quoted-printable decodes escapes and soft breaks and keeps hard line breaks as CRLF", async () => {
    const mail = await mailer.store(
      mixed(
        part(
          ["Content-Type: application/octet-stream", "Content-Transfer-Encoding: quoted-printable", 'Content-Disposition: attachment; filename="qp.bin"'],
          "=00=01=FF caf=C3=A9 soft=\r\nbreak\r\nsecond line=0D=0Aescaped break  \r\nlast"
        )
      )
    );
    const expected = octets("\x00\x01\xff caf\xc3\xa9 softbreak\r\nsecond line\r\nescaped break\r\nlast");
    expect(files(mail)).toEqual([{ filename: "qp.bin", contentType: "application/octet-stream", size: expected.length }]);
    expect(fileBytes(mail).toString("latin1")).toBe(expected.toString("latin1"));
  });

  test("7bit, 8bit, binary, a missing and an unknown encoding keep the octets between the boundaries", async () => {
    // Every octet value, with CR, LF and NUL among them, and a line that looks a little like a boundary.
    const data = Buffer.concat([every, Buffer.from("\r\n--mi not a boundary\r\n\r\nlone\rcr lone\nlf\x00end")]);
    for (const encoding of ["7bit", "8bit", "binary", "x-custom", undefined]) {
      const headers = ["Content-Type: application/octet-stream", 'Content-Disposition: attachment; filename="raw.bin"'];
      if (encoding) headers.push(`Content-Transfer-Encoding: ${encoding}`);
      const mail = await mailer.store(mixed(part(headers, data)));
      expect({ encoding, size: files(mail)[0]!.size }).toEqual({ encoding, size: data.length });
      expect({ encoding, hash: sha256(fileBytes(mail)) }).toEqual({ encoding, hash: sha256(data) });
    }
  });

  test("the line break before the closing boundary is not part of the file; one before that is", async () => {
    const mail = await mailer.store(
      mixed(part(["Content-Type: application/octet-stream"], "ends with a break\r\n"), part(["Content-Type: application/octet-stream"], "no break"))
    );
    expect(fileBytes(mail, 0).toString()).toBe("ends with a break\r\n");
    expect(fileBytes(mail, 1).toString()).toBe("no break");
  });

  test("an attachment that is the whole message keeps its final line break", async () => {
    const mail = await mailer.store(crlf(...HEAD, "Content-Type: application/octet-stream", 'Content-Disposition: attachment; filename="whole.bin"', "", "line one", "line two"));
    expect(fileBytes(mail).toString()).toBe("line one\r\nline two\r\n");
  });
});

describe("attachment file names", () => {
  test("from the disposition's filename, quoted or not", async () => {
    expect((await attach(["Content-Type: application/pdf", 'Content-Disposition: attachment; filename="report.pdf"']))!.filename).toBe("report.pdf");
    expect((await attach(["Content-Type: application/pdf", "Content-Disposition: attachment; filename=report.pdf"]))!.filename).toBe("report.pdf");
    expect((await attach(["Content-Type: application/pdf", 'Content-Disposition: attachment; filename="Annual Report (final).pdf"']))!.filename).toBe(
      "Annual Report (final).pdf"
    );
    expect((await attach(["Content-Type: application/pdf", 'CONTENT-DISPOSITION: ATTACHMENT; FILENAME="UPPER.PDF"']))!.filename).toBe("UPPER.PDF");
  });

  test("from the content type's name when the disposition has none, the disposition's when both do", async () => {
    expect((await attach(['Content-Type: application/pdf; name="typed.pdf"']))!.filename).toBe("typed.pdf");
    expect((await attach(['Content-Type: application/pdf; name="typed.pdf"', "Content-Disposition: attachment"]))!.filename).toBe("typed.pdf");
    expect((await attach(['Content-Type: application/pdf; name="typed.pdf"', 'Content-Disposition: attachment; filename="disposed.pdf"']))!.filename).toBe(
      "disposed.pdf"
    );
  });

  test("without either, the name is attachment", async () => {
    expect((await attach(["Content-Type: application/pdf"]))!.filename).toBe("attachment");
    expect((await attach(["Content-Type: application/pdf", 'Content-Disposition: attachment; filename=""']))!.filename).toBe("attachment");
  });

  test("RFC 2047 encoded words in the name are decoded", async () => {
    const b = (text: string) => `=?utf-8?B?${Buffer.from(text).toString("base64")}?=`;
    expect((await attach(["Content-Type: application/pdf", `Content-Disposition: attachment; filename="${b("Übersicht €.pdf")}"`]))!.filename).toBe(
      "Übersicht €.pdf"
    );
    expect((await attach([`Content-Type: application/pdf; name="=?iso-8859-1?Q?caf=E9.pdf?="`]))!.filename).toBe("café.pdf");
    expect(
      (await attach(["Content-Type: application/pdf", `Content-Disposition: attachment;`, ` filename="${b("第一部分")}`, ` ${b("第二部分.pdf")}"`]))!.filename
    ).toBe("第一部分第二部分.pdf");
  });

  test("RFC 2231 charset-tagged names are decoded", async () => {
    expect((await attach(["Content-Type: application/pdf", "Content-Disposition: attachment; filename*=utf-8''%E2%82%AC%20rates.pdf"]))!.filename).toBe(
      "€ rates.pdf"
    );
    expect((await attach(["Content-Type: application/pdf", "Content-Disposition: attachment; filename*=UTF-8'en'na%C3%AFve%20file.pdf"]))!.filename).toBe(
      "naïve file.pdf"
    );
    expect((await attach(["Content-Type: application/pdf", "Content-Disposition: attachment; filename*=iso-8859-1''caf%E9.pdf"]))!.filename).toBe("café.pdf");
    expect((await attach(["Content-Type: application/pdf; name*=utf-8''%E6%97%A5%E6%9C%AC.pdf"]))!.filename).toBe("日本.pdf");
  });

  test("RFC 2231 continuations are joined in order, tagged or not", async () => {
    expect(
      (await attach(["Content-Type: application/pdf", 'Content-Disposition: attachment; filename*0="a very long-"; filename*1="file name"; filename*2=".pdf"']))!.filename
    ).toBe("a very long-file name.pdf");
    // NOTE: a segment is trimmed even inside its quotes, so a space at either
    // edge of a segment is lost where the sender split the name beside it.
    expect(
      (await attach(["Content-Type: application/pdf", 'Content-Disposition: attachment; filename*0="a very long "; filename*1="file name.pdf"']))!.filename
    ).toBe("a very longfile name.pdf");
    expect(
      (await attach(["Content-Type: application/pdf", 'Content-Disposition: attachment; filename*1="second.pdf"; filename*0="first-"']))!.filename
    ).toBe("first-second.pdf");
    expect(
      (await attach(["Content-Type: application/pdf", "Content-Disposition: attachment;", " filename*0*=utf-8''%E2%82%AC%20;", " filename*1*=rates%20;", ' filename*2="2026.pdf"']))!.filename
    ).toBe("€ rates 2026.pdf");
  });

  test("the encoded form wins over a plain filename beside it", async () => {
    expect(
      (await attach(["Content-Type: application/pdf", "Content-Disposition: attachment; filename=\"fallback.pdf\"; filename*=utf-8''%E2%82%AC.pdf"]))!.filename
    ).toBe("€.pdf");
  });

  test("raw UTF-8 octets in the name are read as UTF-8, other 8-bit octets as Latin-1", async () => {
    const utf8 = await mailer.store(
      mixed([octets('Content-Type: application/pdf\r\nContent-Disposition: attachment; filename="', Buffer.from("Größe ✓.pdf"), '"\r\n\r\nx')])
    );
    expect(files(utf8)[0]!.filename).toBe("Größe ✓.pdf");
    const latin1 = await mailer.store(mixed([octets('Content-Type: application/pdf\r\nContent-Disposition: attachment; filename="caf\xe9.pdf"\r\n\r\nx')]));
    expect(files(latin1)[0]!.filename).toBe("café.pdf");
  });

  test("backslash escapes are removed, and a path is kept as a name", async () => {
    expect((await attach(["Content-Type: application/pdf", 'Content-Disposition: attachment; filename="a\\"b\\\\c.pdf"']))!.filename).toBe('a"b\\c.pdf');
    const mail = await mailer.store(
      mixed(part(["Content-Type: application/pdf", 'Content-Disposition: attachment; filename="../../etc/passwd"'], "not really"))
    );
    expect(files(mail)[0]!.filename).toBe("../../etc/passwd");
    // The file on disk is named by its id, never by the message's file name.
    expect(mail.attachments[0].content.data).toMatch(/^[0-9a-f-]{36}$/);
    expect(fileBytes(mail).toString()).toBe("not really");
  });
});

describe("attachment content types", () => {
  test("the declared type is stored lower-cased, without its parameters", async () => {
    expect((await attach(["Content-Type: Application/PDF"]))!.contentType).toBe("application/pdf");
    expect((await attach(['Content-Type: text/csv; charset=utf-8; header=present; name="a.csv"']))!.contentType).toBe("text/csv");
    expect((await attach(["Content-Type: application/vnd.custom+json;version=2"]))!.contentType).toBe("application/vnd.custom+json");
    expect((await attach(['Content-Type: image/jpeg; name="not-a-jpeg.pdf"']))!.contentType).toBe("image/jpeg");
  });

  test("application/octet-stream with a file name takes the type of the extension", async () => {
    const byExtension: Record<string, string> = {
      "a.pdf": "application/pdf",
      "a.PNG": "image/png",
      "a.jpg": "image/jpeg",
      "a.jpeg": "image/jpeg",
      "a.gif": "image/gif",
      "a.svg": "image/svg+xml",
      "a.txt": "text/plain",
      "a.html": "text/html",
      "a.csv": "text/csv",
      "a.json": "application/json",
      "a.js": "text/javascript",
      "a.zip": "application/zip",
      "a.docx": "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
      "a.xlsx": "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
      "a.mp3": "audio/mpeg",
      "a.mp4": "video/mp4",
      "a.ics": "text/calendar",
      "a.eml": "message/rfc822",
      "archive.tar.gz": "application/gzip",
      "a b .pdf": "application/pdf",
      "a.qqqq": "application/octet-stream",
      "noextension": "application/octet-stream",
      "trailingdot.": "application/octet-stream",
    };
    for (const [filename, contentType] of Object.entries(byExtension)) {
      const attachment = await attach(["Content-Type: application/octet-stream", `Content-Disposition: attachment; filename="${filename}"`]);
      expect({ filename, contentType: attachment!.contentType }).toEqual({ filename, contentType });
    }
    // Without a file name there is nothing to go by.
    expect((await attach(["Content-Type: application/octet-stream"]))!.contentType).toBe("application/octet-stream");
  });

  test("a part with no Content-Type: by the file name, else octet-stream for an attachment", async () => {
    expect(await attach(['Content-Disposition: attachment; filename="scan.pdf"'])).toEqual({
      filename: "scan.pdf",
      contentType: "application/pdf",
      size: 1,
    });
    expect(await attach(["Content-Disposition: attachment"])).toEqual({ filename: "attachment", contentType: "application/octet-stream", size: 1 });
    expect(await attach(['Content-Disposition: attachment; filename="README"'])).toEqual({
      filename: "README",
      contentType: "application/octet-stream",
      size: 1,
    });
    // A text file named in an attachment disposition is still an attachment.
    expect(await attach(['Content-Disposition: attachment; filename="notes.txt"'])).toEqual({
      filename: "notes.txt",
      contentType: "text/plain",
      size: 1,
    });
  });
});

describe("what makes a part an attachment", () => {
  test("anything that is not text/plain, text/html or message/delivery-status, whatever its disposition", async () => {
    for (const type of ["text/calendar", "text/csv", "text/enriched", "text/rtf", "text/x-vcard", "application/pdf", "image/png", "audio/mpeg", "application/x-unknown"]) {
      for (const disposition of [[], ["Content-Disposition: inline"]]) {
        const mail = await mailer.store(mixed(part([`Content-Type: ${type}`, ...disposition], "content")));
        expect({ type, text: mail.text, files: files(mail) }).toEqual({
          type,
          text: "See attached.",
          files: [{ filename: "attachment", contentType: type, size: 7 }],
        });
      }
    }
  });

  test("text/plain and text/html with an attachment disposition", async () => {
    const mail = await mailer.store(
      mixed(
        part(["Content-Type: text/plain; charset=iso-8859-1", 'Content-Disposition: attachment; filename="notes.txt"'], octets("caf\xe9")),
        part(["Content-Type: text/html", 'Content-Disposition: ATTACHMENT; filename="page.html"'], "<p>page</p>")
      )
    );
    expect(mail.text).toBe("See attached.");
    expect(files(mail)).toEqual([
      { filename: "notes.txt", contentType: "text/plain", size: 4 },
      { filename: "page.html", contentType: "text/html", size: 11 },
    ]);
    // The file holds the part's octets, not a re-encoding of its text.
    expect(fileBytes(mail, 0).toString("latin1")).toBe("caf\xe9");
  });

  test("a text part with a disposition that is neither inline nor attachment is an attachment", async () => {
    const mail = await mailer.store(mixed(part(["Content-Type: text/plain", "Content-Disposition: form-data"], "odd disposition")));
    expect(mail.text).toBe("See attached.");
    expect(files(mail)).toEqual([{ filename: "attachment", contentType: "text/plain", size: 15 }]);
  });

  test("an inline text part stays in the body even when it carries a file name", async () => {
    const mail = await mailer.store(
      mixed(part(["Content-Type: text/plain", 'Content-Disposition: inline; filename="inline.txt"'], "inline text with a name"))
    );
    expect(mail.text).toBe("See attached.\ninline text with a name");
    expect(mail.attachments).toEqual([]);
  });

  test("a calendar invitation among the alternatives is an attachment", async () => {
    const mail = await mailer.store(
      crlf(
        ...HEAD,
        'Content-Type: multipart/alternative; boundary="alt"',
        "",
        "--alt",
        "Content-Type: text/plain",
        "",
        "You are invited.",
        "--alt",
        "Content-Type: text/html",
        "",
        "<p>You are invited.</p>",
        "--alt",
        'Content-Type: text/calendar; charset="utf-8"; method=REQUEST',
        "Content-Transfer-Encoding: 7bit",
        "",
        "BEGIN:VCALENDAR",
        "METHOD:REQUEST",
        "BEGIN:VEVENT",
        "SUMMARY:Planning",
        "END:VEVENT",
        "END:VCALENDAR",
        "--alt--"
      )
    );
    expect([mail.text, mail.html]).toEqual(["You are invited.", "<p>You are invited.</p>"]);
    const calendar = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nSUMMARY:Planning\r\nEND:VEVENT\r\nEND:VCALENDAR";
    expect(files(mail)).toEqual([{ filename: "attachment", contentType: "text/calendar", size: calendar.length }]);
    expect(fileBytes(mail).toString()).toBe(calendar);
  });

  test("calendar files attached by name", async () => {
    const ics = "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n";
    const mail = await mailer.store(
      mixed(
        part(['Content-Type: text/calendar; name="invite.ics"', 'Content-Disposition: attachment; filename="invite.ics"'], ics),
        part(['Content-Type: application/ics; name="invite2.ics"', "Content-Transfer-Encoding: base64"], Buffer.from(ics).toString("base64")),
        part(['Content-Type: application/octet-stream; name="invite3.ics"'], ics)
      )
    );
    expect(files(mail)).toEqual([
      { filename: "invite.ics", contentType: "text/calendar", size: ics.length },
      { filename: "invite2.ics", contentType: "application/ics", size: ics.length },
      { filename: "invite3.ics", contentType: "text/calendar", size: ics.length },
    ]);
  });

  test("a detached signature", async () => {
    const mail = await mailer.store(
      crlf(
        ...HEAD,
        'Content-Type: multipart/signed; protocol="application/pgp-signature"; micalg=pgp-sha256; boundary="sig"',
        "",
        "--sig",
        "Content-Type: text/plain",
        "",
        "Signed text.",
        "--sig",
        'Content-Type: application/pgp-signature; name="signature.asc"',
        "",
        "-----BEGIN PGP SIGNATURE-----",
        "abc",
        "-----END PGP SIGNATURE-----",
        "--sig--"
      )
    );
    expect(mail.text).toBe("Signed text.");
    expect(files(mail)).toEqual([{ filename: "signature.asc", contentType: "application/pgp-signature", size: 63 }]);
  });
});

describe("inline images", () => {
  const related = (html: string, ...imageParts: (string | Uint8Array)[][]) =>
    octets(
      crlf(...HEAD, 'Content-Type: multipart/related; boundary="rel"', "", "--rel", "Content-Type: text/html; charset=utf-8", "", html),
      ...imageParts.flatMap((image) => ["--rel\r\n", ...image, "\r\n"]),
      "--rel--\r\n"
    );
  const image = (cid: string, extra: string[] = []) =>
    part(["Content-Type: image/png", "Content-Transfer-Encoding: base64", `Content-ID: ${cid}`, ...extra], PNG.toString("base64"));
  const dataUrl = `data:image/png;base64,${PNG.toString("base64")}`;

  test("a cid: reference to an image part is replaced by a data: URL, and the image stays an attachment", async () => {
    const mail = await mailer.store(
      related('<p>Logo: <img src="cid:logo@remote.example" alt="logo"></p>', image("<logo@remote.example>", ['Content-Disposition: inline; filename="logo.png"']))
    );
    expect(mail.html).toBe(`<p>Logo: <img src="${dataUrl}" alt="logo"></p>`);
    // The text is derived from the HTML after the image went in; the alt text is not carried over.
    expect(mail.text).toBe("Logo:");
    expect(files(mail)).toEqual([{ filename: "logo.png", contentType: "image/png", size: PNG.length }]);
    // The attachment entry carries no trace of the Content-ID.
    expect(Object.keys(mail.attachments[0]).sort()).toEqual(["content", "contentType", "filename", "size"]);
  });

  test("every reference to the same image is replaced; references to nothing are left alone", async () => {
    const mail = await mailer.store(
      related(
        `<img src="cid:a@x"><img src='cid:a@x'><div style="background:url(cid:a@x)"></div><img src="cid:missing@x"><img src="CID:a@x">`,
        image("<a@x>")
      )
    );
    expect(mail.html).toBe(
      `<img src="${dataUrl}"><img src='${dataUrl}'><div style="background:url(cid:a@x)"></div><img src="cid:missing@x"><img src="CID:a@x">`
    );
  });

  test("a Content-ID without angle brackets, or with spaces around them, matches the same", async () => {
    const mail = await mailer.store(related('<img src="cid:bare-id"><img src="cid:spaced@x">', image("bare-id"), image("  <spaced@x>  ")));
    expect(mail.html).toBe(`<img src="${dataUrl}"><img src="${dataUrl}">`);
  });

  test("a cid: that names a part which is not an image is left alone", async () => {
    const mail = await mailer.store(
      related('<a href="cid:doc@x">document</a>', part(["Content-Type: application/pdf", "Content-ID: <doc@x>"], "%PDF-1.4"))
    );
    expect(mail.html).toBe('<a href="cid:doc@x">document</a>');
    expect(files(mail)).toEqual([{ filename: "attachment", contentType: "application/pdf", size: 8 }]);
  });

  test("an image with no reference to it is simply an attachment", async () => {
    const mail = await mailer.store(related("<p>No image tags here.</p>", image("<unused@x>")));
    expect(mail.html).toBe("<p>No image tags here.</p>");
    expect(files(mail)).toEqual([{ filename: "attachment", contentType: "image/png", size: PNG.length }]);
  });

  test("a plain-text message is not searched for cid: references", async () => {
    const mail = await mailer.store(mixed(image("<a@x>")).toString("latin1").replace("See attached.", "See cid:a@x here."));
    expect([mail.text, mail.html]).toEqual(["See cid:a@x here.", "See cid:a@x here."]);
  });
});

describe("large attachments", () => {
  test("a 3 MiB attachment arrives intact and its working copy is removed", async () => {
    const data = crypto.randomBytes(3 * 1024 * 1024);
    const encoded = data.toString("base64").replace(/(.{76})/g, "$1\r\n");
    const mail = await mailer.store(
      mixed(part(["Content-Type: application/octet-stream", "Content-Transfer-Encoding: base64", 'Content-Disposition: attachment; filename="big.bin"'], encoded))
    );
    expect(files(mail)).toEqual([{ filename: "big.bin", contentType: "application/octet-stream", size: data.length }]);
    expect(sha256(fileBytes(mail))).toBe(sha256(data));
    await waitFor(() => incomingLeftovers(server).length === 0, { what: "the transaction's working files to go" });
  });

  test("images referenced by cid: are inlined only up to 4 MiB in total", async () => {
    // Two images of about 2.5 MiB: the first fits the budget, the second does not.
    const one = crypto.randomBytes(2_500_000);
    const two = crypto.randomBytes(2_500_000);
    const wrap = (data: Buffer) => data.toString("base64").replace(/(.{76})/g, "$1\r\n");
    const mail = await mailer.store(
      octets(
        crlf(...HEAD, 'Content-Type: multipart/related; boundary="rel"', "", "--rel", "Content-Type: text/html", "", '<img src="cid:one@x"><img src="cid:two@x">'),
        crlf("--rel", "Content-Type: image/png", "Content-Transfer-Encoding: base64", "Content-ID: <one@x>", "", wrap(one)),
        crlf("--rel", "Content-Type: image/png", "Content-Transfer-Encoding: base64", "Content-ID: <two@x>", "", wrap(two)),
        "--rel--\r\n"
      )
    );
    expect(mail.html).toBe(`<img src="data:image/png;base64,${one.toString("base64")}"><img src="cid:two@x">`);
    expect(files(mail).map((file) => file.size)).toEqual([one.length, two.length]);
  });
});
