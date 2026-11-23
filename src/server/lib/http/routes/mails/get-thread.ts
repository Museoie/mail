import { MailHeaderDataType, isUuid } from "common";
import { getThread, toMailHeaderData } from "server";
import { Route } from "../route";
import { addressOnDomains, ownAddressDomains } from "../../../domains";
import { ownAddressesOf } from "../../../mails/thread";
import {
  findViewableMail,
  getMailHeadersByIds,
  MailHeaderResult
} from "../../../postgres/repositories/mails";

/** The conversation a mail belongs to, oldest message first. */
export type ThreadGetResponse = MailHeaderDataType[];

export const getThreadRoute = new Route<ThreadGetResponse>(
  "GET",
  "/thread/:id",
  async (req) => {
    const user = req.session.user!;
    if (!isUuid(req.params.id)) {
      return { status: "failed", message: "No email is found." };
    }
    const own = await ownAddressDomains(user.id);
    const thread = await getThread(user, req.params.id, ownAddressesOf(own));
    if (thread.length) return { status: "success", body: thread };

    // A mail of a domain the user views is shown on its own: following its
    // conversation through the owner's rows would reach the owner's other
    // domains, which the role does not cover.
    const viewable = await findViewableMail(user.id, req.params.id);
    if (viewable) {
      const rows: MailHeaderResult[] = [];
      for await (const batch of getMailHeadersByIds(viewable.ownerId, [req.params.id])) {
        rows.push(...batch);
      }
      const isReaders = addressOnDomains(viewable.domains);
      if (rows.length) {
        return { status: "success", body: rows.map((row) => toMailHeaderData(row, isReaders)) };
      }
    }
    return { status: "failed", message: "No email is found." };
  }
);
