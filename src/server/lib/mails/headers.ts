import {
  MailHeaderData,
  MaskedUser,
  Pagination,
  SignedUser,
  MailAddressType,
  MailAddressValueType,
  Insight,
} from "common";
import {
  getMailHeaderIds as pgGetMailHeaderIds,
  getMailHeadersByIds as pgGetMailHeadersByIds,
  getMailHeadersDelta as pgGetMailHeadersDelta,
  getUnifiedMailHeaderIds as pgGetUnifiedMailHeaderIds,
  getMailHeaderIdPage as pgGetMailHeaderIdPage,
  getUnifiedMailHeaderIdPage as pgGetUnifiedMailHeaderIdPage,
  ListCursor,
  getUnifiedMailHeadersDelta as pgGetUnifiedMailHeadersDelta,
  GetMailHeadersOptions,
  MailHeaderResult,
} from "../postgres/repositories/mails";
import { buildSnippet } from "./snippet";
import type { AddressPredicate } from "../domains";

export interface GetMailsOptions {
  sent: boolean;
  saved: boolean;
  spam: boolean;
  pagination?: Pagination;
  /**
   * Only the mail of these domains of the owner's: a viewer of one domain
   * reads its owner's rows filtered to it. Omitted, the whole store.
   */
  domains?: string[];
}

/**
 * Options for the unified list. The unified view is the All Mails view
 * only, so there are no sent/saved/spam variants to pass — only paging.
 * `addresses` is the user's receiving-address set, resolved by the caller
 * from the accounts payload (the route's `received` keys).
 */
export interface GetUnifiedMailsOptions {
  pagination?: Pagination;
}

export interface MailHeadersDelta {
  as_of: string;
  headers: MailHeaderData[];
  expunged_ids: string[];
}

interface UserWithId {
  user_id?: string;
}

const resolveUserId = (user: MaskedUser | Pick<SignedUser, "id">): string | undefined =>
  "id" in user ? user.id : (user as UserWithId).user_id;

async function* noBatches(): AsyncGenerator<MailHeaderResult[]> {}

const isRecipientIn = (entry: unknown, isReaders: AddressPredicate) => {
  const address = (entry as { address?: unknown } | null)?.address;
  return typeof address === "string" && isReaders(address);
};

/**
 * A received row's `envelopeTo` as its reader may see it. Every owner a
 * message is delivered to stores the whole envelope, Bcc recipients on other
 * people's domains included, so only the recipients on the reader's domains
 * are shown: `isReaders` is the owner's own domains, or for a viewer the
 * domain they view.
 */
export const envelopeToOf = (
  m: { envelope_to?: unknown; sent?: unknown },
  isReaders: AddressPredicate
): MailAddressType | undefined => {
  if (!m.envelope_to) return undefined;
  if (m.sent) return { value: m.envelope_to as MailAddressValueType[], text: "" };
  const entries = Array.isArray(m.envelope_to) ? m.envelope_to : [m.envelope_to];
  return {
    value: entries.filter((entry) => isRecipientIn(entry, isReaders)),
    text: "",
  };
};

/** A list row as its reader sees it; `isReaders` is as for {@link envelopeToOf}. */
export const toMailHeaderData = (m: MailHeaderResult, isReaders: AddressPredicate): MailHeaderData => {
  const envelopeTo = envelopeToOf(m, isReaders);
  return new MailHeaderData({
    id: m.mail_id,
    subject: m.subject,
    date: m.date,
    from: m.from_address
      ? { value: m.from_address as MailAddressValueType[], text: m.from_text || "" }
      : undefined,
    to: m.to_address
      ? { value: m.to_address as MailAddressValueType[], text: m.to_text || "" }
      : undefined,
    cc: m.cc_address
      ? { value: m.cc_address as MailAddressValueType[], text: m.cc_text || "" }
      : undefined,
    bcc: m.bcc_address
      ? { value: m.bcc_address as MailAddressValueType[], text: m.bcc_text || "" }
      : undefined,
    ...(envelopeTo ? { envelopeTo } : {}),
    read: m.read,
    saved: m.saved,
    sent: m.sent,
    insight: m.insight as Insight | undefined,
    snippet: buildSnippet(m.text_prefix),
    ...(m.message_id ? { messageId: m.message_id } : {}),
    ...(m.in_reply_to ? { inReplyTo: m.in_reply_to } : {}),
    ...(m.reference_ids?.length ? { references: m.reference_ids } : {}),
  });
};

export const getMailHeaders = async (
  user: MaskedUser | Pick<SignedUser, "id">,
  address: string,
  options: GetMailsOptions
): Promise<AsyncIterable<MailHeaderResult[]>> => {
  const userId = resolveUserId(user);
  if (!userId) return noBatches();

  const { from, size } = options.pagination || new Pagination();

  const pgOptions: GetMailHeadersOptions = {
    sent: options.sent,
    saved: options.saved,
    spam: options.spam,
    domains: options.domains,
    from,
    size,
  };

  const ids = await pgGetMailHeaderIds(userId, address, pgOptions);
  return pgGetMailHeadersByIds(userId, ids);
};

export interface MailHeaderPage {
  batches: AsyncIterable<MailHeaderResult[]>;
  next: ListCursor | null;
}

/** Up to `limit` rows of the list, starting after `after` (or at the top). */
export const getMailHeaderPage = async (
  user: MaskedUser | Pick<SignedUser, "id">,
  address: string,
  options: GetMailsOptions,
  limit: number,
  after?: ListCursor
): Promise<MailHeaderPage> => {
  const userId = resolveUserId(user);
  if (!userId) return { batches: noBatches(), next: null };

  const { ids, next } = await pgGetMailHeaderIdPage(
    userId,
    address,
    { sent: options.sent, saved: options.saved, spam: options.spam, domains: options.domains, after },
    limit
  );
  return { batches: pgGetMailHeadersByIds(userId, ids), next };
};

/** The unified list's counterpart of getMailHeaderPage. */
export const getUnifiedMailHeaderPage = async (
  user: MaskedUser | Pick<SignedUser, "id">,
  addresses: string[],
  limit: number,
  after?: ListCursor
): Promise<MailHeaderPage> => {
  const userId = resolveUserId(user);
  if (!userId) return { batches: noBatches(), next: null };

  const { ids, next } = await pgGetUnifiedMailHeaderIdPage(
    userId,
    addresses,
    { after },
    limit
  );
  return { batches: pgGetMailHeadersByIds(userId, ids), next };
};

export const getMailHeadersDelta = async (
  user: MaskedUser | Pick<SignedUser, "id">,
  address: string,
  options: GetMailsOptions,
  since: string,
  isReaders: AddressPredicate
): Promise<MailHeadersDelta> => {
  const userId = resolveUserId(user);
  if (!userId) return { as_of: since, headers: [], expunged_ids: [] };

  const pgOptions: GetMailHeadersOptions = {
    sent: options.sent,
    saved: options.saved,
    spam: options.spam,
    domains: options.domains,
  };

  const { as_of, headers, expunged_ids } = await pgGetMailHeadersDelta(
    userId,
    address,
    pgOptions,
    since
  );

  return {
    as_of,
    headers: headers.map((row) => toMailHeaderData(row, isReaders)),
    expunged_ids,
  };
};

export const getUnifiedMailHeaders = async (
  user: MaskedUser | Pick<SignedUser, "id">,
  addresses: string[],
  options: GetUnifiedMailsOptions
): Promise<AsyncIterable<MailHeaderResult[]>> => {
  const userId = resolveUserId(user);
  if (!userId) return noBatches();

  const { from, size } = options.pagination || new Pagination();

  const ids = await pgGetUnifiedMailHeaderIds(userId, addresses, { from, size });
  return pgGetMailHeadersByIds(userId, ids);
};

export const getUnifiedMailHeadersDelta = async (
  user: MaskedUser | Pick<SignedUser, "id">,
  addresses: string[],
  since: string,
  isReaders: AddressPredicate
): Promise<MailHeadersDelta> => {
  const userId = resolveUserId(user);
  if (!userId) return { as_of: since, headers: [], expunged_ids: [] };

  const { as_of, headers, expunged_ids } =
    await pgGetUnifiedMailHeadersDelta(userId, addresses, since);

  return {
    as_of,
    headers: headers.map((row) => toMailHeaderData(row, isReaders)),
    expunged_ids,
  };
};
