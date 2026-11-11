import { MailDataToSend, MailDataToSendType } from "common";
import {
  sendMail,
  MailValidationError,
  MailSendingError,
  refuseReadOnly,
} from "server";
import { MAX_ATTACHMENTS_PER_MAIL, toUploadList } from "../../upload";
import { Route } from "../route";

export type SendMailPostResponse =
  | { status: "success" }
  | { status: "failed"; message: string };

/** `references` is the server's to work out from the stored conversation. */
export type SendMailPostBody = Omit<MailDataToSendType, "references">;

export const postSendMailRoute = new Route<SendMailPostResponse>(
  "POST",
  "/send",
  async (req) => {
    const user = req.session.user!;

    const guard = refuseReadOnly(user, "Sending mail");
    if (!guard.ok) return { status: "failed", message: guard.message };

    const body = req.body as SendMailPostBody;
    const attachments = req.files?.attachments;

    if (toUploadList(attachments).length > MAX_ATTACHMENTS_PER_MAIL) {
      return {
        status: "failed",
        message: `A mail may carry at most ${MAX_ATTACHMENTS_PER_MAIL} attachments`
      };
    }

    try {
      // Drop any client-supplied References: sendMail derives them from the
      // mail being answered, so a client can't graft a reply onto another
      // conversation's thread.
      const { references: _ignored, ...fields } = body as SendMailPostBody & {
        references?: unknown;
      };
      await sendMail(user, new MailDataToSend({ ...fields }), attachments);
      return { status: "success" };
    } catch (error) {
      if (error instanceof MailValidationError || error instanceof MailSendingError) {
        return { status: "failed", message: error.message };
      }
      throw error;
    }
  }
);
