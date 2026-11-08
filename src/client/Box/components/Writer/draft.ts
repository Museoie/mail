/**
 * Whether the compose form currently holds a draft — the fields, the editor
 * content, or a quoted original. Read by the rail Compose button's dot
 * (design doc §7) and driven by the Writer, which owns the fields.
 */
export interface DraftFields {
  name: string;
  to: string;
  cc: string;
  bcc: string;
  subject: string;
  sender: string;
  /** The editor's HTML (`initialContent`). */
  initialContent: string;
  /** Whether a quoted original is attached to the draft. */
  hasQuote: boolean;
}

/** Whether editor HTML holds visible text — an empty document is `<p></p>`. */
export const editorHtmlHasText = (html: string): boolean => {
  const text = html
    .replace(/<[^>]*>/g, "")
    .replace(/&nbsp;/g, " ")
    .trim();
  return text.length > 0;
};

export const hasDraftFields = (fields: DraftFields): boolean =>
  fields.hasQuote ||
  [
    fields.name,
    fields.to,
    fields.cc,
    fields.bcc,
    fields.subject,
    fields.sender
  ].some((v) => v.trim().length > 0) ||
  editorHtmlHasText(fields.initialContent);
