import { SignedUser } from "common";
import { addressOnDomains, domainOfAddress, ownAddressDomains, type AddressPredicate } from "../../../domains";
import { getDomainAccess } from "../../../postgres/repositories/domains";

/** Whose mail rows a request reads, and which of their domains it may see. */
export interface MailReader {
  /** The store the rows are keyed by: the user, or the owner of a viewed domain. */
  store: Pick<SignedUser, "id">;
  /** Undefined reads the whole store (the user's own); set, only those domains. */
  domains?: string[];
  /** The addresses the reader may see as a received mail's envelope recipients. */
  isReaders: AddressPredicate;
}

/**
 * The reader for an account (an address): its owner's rows, filtered to the
 * domain, for a domain the user views; otherwise the user's own rows, which
 * hold whatever mail they have for the address — a domain they own, or one
 * they used to own and whose mail stayed with them. The role is read in the
 * same request, so a revoked one stops the next read.
 */
export const resolveMailReader = async (
  user: SignedUser,
  account: string
): Promise<MailReader | null> => {
  const domain = domainOfAddress(account);
  if (!domain) return null;
  const access = await getDomainAccess(user.id, domain);
  if (access?.role === "viewer") {
    return {
      store: { id: access.ownerId },
      domains: [access.name],
      isReaders: addressOnDomains([access.name]),
    };
  }
  return { store: user, isReaders: addressOnDomains(await ownAddressDomains(user.id)) };
};
