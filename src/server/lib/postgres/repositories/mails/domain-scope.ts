import { pool } from "../../client";
import type { ParamValue } from "../../database";
import { DOMAINS, DOMAIN_ROLES } from "../../models";

/**
 * Which of an owner's mail rows belong to one of their domains.
 *
 * A domain's mail is stored with its owner, beside the mail of the owner's
 * other domains, so reading one domain (a viewer of it, or a login scoped to
 * it) filters the owner's rows to that domain:
 *
 * - a received mail belongs to the domains it was delivered to: its envelope
 *   recipients, which this server recorded. The `To`/`Cc`/`Bcc` headers are
 *   the sender's to write, so a header naming a domain proves nothing; they
 *   stand in only for a row stored before envelopes were recorded.
 * - a sent mail belongs to the domain of its `From` address, which the owner
 *   chose when sending from one of their domains.
 */

const arrayOrEmpty = (column: string) =>
  `CASE WHEN jsonb_typeof(${column}) = 'array' THEN ${column} ELSE '[]'::jsonb END`;

const addressDomainIn = (elements: string, names: string) =>
  `EXISTS (SELECT 1 FROM jsonb_array_elements(${elements}) AS party
     WHERE lower(regexp_replace(party->>'address', '^.*@', '')) = ANY(${names}))`;

/**
 * The predicate as SQL, over the domain names `names` (a `text[]` expression)
 * and the `mails` columns under `prefix`.
 */
export const mailInDomainsExpression = (names: string, prefix = ""): string => {
  const p = prefix;
  const param = names;
  const received = `CASE WHEN ${p}envelope_to IS NOT NULL THEN ${arrayOrEmpty(`${p}envelope_to`)}
    ELSE ${arrayOrEmpty(`${p}to_address`)} || ${arrayOrEmpty(`${p}cc_address`)} || ${arrayOrEmpty(`${p}bcc_address`)} END`;
  return `(CASE WHEN ${p}sent THEN ${addressDomainIn(arrayOrEmpty(`${p}from_address`), param)}
    ELSE ${addressDomainIn(received, param)} END)`;
};

const DOMAIN_NAME_CHARS = /^[a-z0-9.-]+$/;

/**
 * `domains` as an SQL `text[]` literal. Domain names are lower-case letters,
 * digits, dots and hyphens (the only spelling `normalizeDomainName` and the
 * domain rows produce), so anything else is refused rather than quoted.
 */
export const domainsArrayLiteral = (domains: string[]): string => {
  for (const domain of domains) {
    if (!DOMAIN_NAME_CHARS.test(domain)) throw new Error(`Not a domain name: ${JSON.stringify(domain)}`);
  }
  return domains.length
    ? `ARRAY[${domains.map((domain) => `'${domain}'`).join(", ")}]::text[]`
    : "ARRAY[]::text[]";
};

/** {@link mailInDomainsExpression} with the names bound at placeholder `$<paramIndex>`. */
export const mailInDomainsCondition = (paramIndex: number, prefix = ""): string =>
  mailInDomainsExpression(`$${paramIndex}::text[]`, prefix);

/**
 * Appends the predicate to a query's `WHERE` when `domains` scopes it, binding
 * the names as the next parameter. Unscoped (`undefined`), the query reads the
 * owner's whole store and is returned unchanged.
 */
export const scopeToDomains = (
  sql: string,
  values: ParamValue[],
  domains: string[] | undefined,
  prefix = ""
): { sql: string; values: ParamValue[] } => {
  if (domains === undefined) return { sql, values };
  return {
    sql: `${sql} AND ${mailInDomainsCondition(values.length + 1, prefix)}`,
    values: [...values, domains],
  };
};

/** Where a mail a person may read is stored, and which of its domains let them. */
export interface ViewableMail {
  ownerId: string;
  /** The domains the person views that the mail belongs to. */
  domains: string[];
}

export const buildFindViewableMailQuery = (
  viewerId: string,
  mailId: string
): { sql: string; values: ParamValue[] } => ({
  // The mail's owner has to own a domain the viewer holds a role on, and the
  // mail has to belong to that domain: a role on one domain reads nothing of
  // the owner's other domains.
  sql: `
    SELECT m.user_id, array_agg(d.name ORDER BY d.name) AS domains
    FROM mails m
    JOIN ${DOMAINS} d ON d.user_id = m.user_id
    JOIN ${DOMAIN_ROLES} r ON r.domain_id = d.domain_id AND r.user_id = $1
    WHERE m.mail_id = $2
      AND m.expunged = FALSE
      AND ${mailInDomainsExpression("ARRAY[d.name]::text[]", "m.")}
    GROUP BY m.user_id`,
  values: [viewerId, mailId],
});

/** The owner a viewer reads `mailId` from, or null when it belongs to no domain they view. */
export const findViewableMail = async (
  viewerId: string,
  mailId: string
): Promise<ViewableMail | null> => {
  const { sql, values } = buildFindViewableMailQuery(viewerId, mailId);
  const result = await pool.query<{ user_id: string; domains: string[] }>(sql, values);
  const row = result.rows[0];
  return row ? { ownerId: row.user_id, domains: row.domains } : null;
};

export const buildFindViewableAttachmentQuery = (
  viewerId: string,
  attachmentId: string
): { sql: string; values: ParamValue[] } => ({
  // The same rule as `buildFindViewableMailQuery`, found by an attachment the
  // mail carries instead of by its id.
  sql: `
    SELECT m.mail_id
    FROM mails m
    JOIN ${DOMAINS} d ON d.user_id = m.user_id
    JOIN ${DOMAIN_ROLES} r ON r.domain_id = d.domain_id AND r.user_id = $1
    WHERE m.attachments @> $2::jsonb
      AND m.expunged = FALSE
      AND ${mailInDomainsExpression("ARRAY[d.name]::text[]", "m.")}
    LIMIT 1`,
  values: [viewerId, JSON.stringify([{ content: { data: attachmentId } }])],
});

/** Whether a viewer may download `attachmentId`: a mail of a domain they view carries it. */
export const canViewAttachment = async (
  viewerId: string,
  attachmentId: string
): Promise<boolean> => {
  const { sql, values } = buildFindViewableAttachmentQuery(viewerId, attachmentId);
  const result = await pool.query(sql, values);
  return (result.rowCount ?? 0) > 0;
};
