import { MailHeaderDataType, isUuid } from "common";
import { getThread } from "server";
import { Route } from "../route";

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
    const thread = await getThread(user, req.params.id);
    if (!thread.length) {
      return { status: "failed", message: "No email is found." };
    }
    return { status: "success", body: thread };
  }
);
