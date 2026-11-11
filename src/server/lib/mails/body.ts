import { MailBodyData, AttachmentType, Insight } from "common";
import { getMailById } from "../postgres/repositories/mails";
import { bodyHtml } from "./text-to-html";

export const getMailBody = async (
  userId: string,
  mailId: string
): Promise<MailBodyData | undefined> => {
  const mailModel = await getMailById(userId, mailId);

  if (!mailModel) return undefined;

  return new MailBodyData({
    id: mailModel.mail_id,
    // A plain-text mail is stored with its text in `html`; convert it so the
    // viewer keeps its line breaks and shows `&`/`<` literally.
    html: bodyHtml(mailModel.html, mailModel.text),
    attachments: Array.isArray(mailModel.attachments)
      ? (mailModel.attachments as AttachmentType[])
      : undefined,
    messageId: mailModel.message_id,
    insight: mailModel.insight as Insight | undefined,
  });
};
