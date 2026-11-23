/**
 * The predicate that filters an owner's mail to one domain is the boundary a
 * viewer and a scoped login read through. Its behaviour against Postgres is
 * probed in `../domains.test.ts` (opt-in, INBOX_TEST_PG_URL); these pin its
 * SQL shape without a database, so CI fails on any change to it. Each test
 * compares the whole statement, so an appended `OR` (which would let a
 * header the sender wrote widen what a viewer sees) fails it too.
 */
import { describe, expect, it } from "bun:test";
import {
  buildFindViewableAttachmentQuery,
  buildFindViewableMailQuery,
  domainsArrayLiteral,
  mailInDomainsCondition,
  mailInDomainsExpression,
  scopeToDomains,
} from "./domain-scope";
import { membershipCondition } from "./views";

const squash = (sql: string) => sql.replace(/\s+/g, " ").trim();

const arrayOrEmpty = (column: string) =>
  `CASE WHEN jsonb_typeof(${column}) = 'array' THEN ${column} ELSE '[]'::jsonb END`;
const anyAddressIn = (elements: string, names: string) =>
  `EXISTS (SELECT 1 FROM jsonb_array_elements(${elements}) AS party WHERE lower(regexp_replace(party->>'address', '^.*@', '')) = ANY(${names}))`;

/** The predicate, spelled out: the only shape these tests accept. */
const expected = (names: string, p = "") =>
  `(CASE WHEN ${p}sent THEN ${anyAddressIn(arrayOrEmpty(`${p}from_address`), names)} ` +
  `ELSE ${anyAddressIn(
    `CASE WHEN ${p}envelope_to IS NOT NULL THEN ${arrayOrEmpty(`${p}envelope_to`)} ` +
      `ELSE ${arrayOrEmpty(`${p}to_address`)} || ${arrayOrEmpty(`${p}cc_address`)} || ${arrayOrEmpty(`${p}bcc_address`)} END`,
    names
  )} END)`;

describe("mailInDomainsExpression", () => {
  it("is exactly: sent mail by From; received mail by envelope, headers only without one", () => {
    expect(squash(mailInDomainsExpression("$2::text[]"))).toBe(squash(expected("$2::text[]")));
  });

  it("qualifies every column with the prefix", () => {
    expect(squash(mailInDomainsExpression("$3::text[]", "m."))).toBe(
      squash(expected("$3::text[]", "m."))
    );
  });

  it("never reads the headers of a received mail that has an envelope", () => {
    // The header columns appear only in the ELSE of the envelope CASE.
    const sql = squash(mailInDomainsExpression("$2::text[]"));
    const received = sql.slice(sql.indexOf("ELSE EXISTS"));
    const envelopeCase = received.indexOf("CASE WHEN envelope_to IS NOT NULL THEN");
    const headersStart = received.indexOf("to_address");
    expect(envelopeCase).toBeGreaterThan(-1);
    expect(headersStart).toBeGreaterThan(received.indexOf("ELSE", envelopeCase));
  });

  it("binds the names as a placeholder through mailInDomainsCondition", () => {
    expect(mailInDomainsCondition(4, "m.")).toBe(mailInDomainsExpression("$4::text[]", "m."));
  });
});

describe("domainsArrayLiteral", () => {
  it("inlines domain names, and refuses anything that is not one", () => {
    expect(domainsArrayLiteral(["example.com", "a-b.example.org"])).toBe(
      "ARRAY['example.com', 'a-b.example.org']::text[]"
    );
    expect(domainsArrayLiteral([])).toBe("ARRAY[]::text[]");
    for (const name of ["x'); DROP TABLE mails; --", "Example.com", "a b.example", ""]) {
      expect(() => domainsArrayLiteral([name])).toThrow();
    }
  });

  it("is what the IMAP membership condition carries for a scoped login", () => {
    expect(squash(membershipCondition(null, true, "m.", ["example.com"]))).toBe(
      squash(` AND ${expected("ARRAY['example.com']::text[]", "m.")}`)
    );
  });
});

describe("scopeToDomains", () => {
  it("leaves an unscoped query alone and binds the names as the next parameter otherwise", () => {
    expect(scopeToDomains("SELECT 1 WHERE x = $1", ["a"], undefined)).toEqual({
      sql: "SELECT 1 WHERE x = $1",
      values: ["a"],
    });
    const scoped = scopeToDomains("SELECT 1 WHERE x = $1", ["a"], ["example.com"]);
    expect(squash(scoped.sql)).toBe(squash(`SELECT 1 WHERE x = $1 AND ${expected("$2::text[]")}`));
    expect(scoped.values).toEqual(["a", ["example.com"]]);
  });
});

describe("the viewer lookups", () => {
  const join = `FROM mails m JOIN domains d ON d.user_id = m.user_id JOIN domain_roles r ON r.domain_id = d.domain_id AND r.user_id = $1`;

  it("finds a mail only through a domain the viewer holds a role on, filtered to that domain", () => {
    const { sql, values } = buildFindViewableMailQuery("u-viewer", "m-1");
    expect(squash(sql)).toBe(
      squash(
        `SELECT m.user_id, array_agg(d.name ORDER BY d.name) AS domains ${join} ` +
          `WHERE m.mail_id = $2 AND m.expunged = FALSE AND ${expected("ARRAY[d.name]::text[]", "m.")} ` +
          `GROUP BY m.user_id`
      )
    );
    expect(values).toEqual(["u-viewer", "m-1"]);
  });

  it("finds an attachment by the same rule", () => {
    const { sql, values } = buildFindViewableAttachmentQuery("u-viewer", "att-1");
    expect(squash(sql)).toBe(
      squash(
        `SELECT m.mail_id ${join} WHERE m.attachments @> $2::jsonb AND m.expunged = FALSE ` +
          `AND ${expected("ARRAY[d.name]::text[]", "m.")} LIMIT 1`
      )
    );
    expect(values).toEqual(["u-viewer", JSON.stringify([{ content: { data: "att-1" } }])]);
  });
});
