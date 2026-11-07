/**
 * One-line body preview for list rows (design doc §5).
 *
 * Built from the stored plain-text body — no new column, no new endpoint:
 * the headers query selects a bounded prefix of the existing `text` column
 * and this strips quoted history from it. Returns `undefined` when there is
 * no unquoted content to show, so the row simply renders without a snippet.
 */
export const SNIPPET_MAX_LENGTH = 160;

const QUOTED_LINE = /^\s*>/;
const WROTE_LINE = /^\s*On\b.+wrote:\s*$/;
const FORWARD_SEPARATOR =
  /^(-{2,}\s*Original Message|_{2,}|Begin forwarded message)/i;

export const buildSnippet = (
  text: string | null | undefined
): string | undefined => {
  if (!text) return undefined;
  const lines: string[] = [];
  for (const line of text.split(/\r?\n/)) {
    if (FORWARD_SEPARATOR.test(line)) break;
    if (QUOTED_LINE.test(line)) continue;
    if (WROTE_LINE.test(line)) continue;
    lines.push(line);
  }
  const collapsed = lines.join(" ").replace(/\s+/g, " ").trim();
  if (!collapsed) return undefined;
  if (collapsed.length <= SNIPPET_MAX_LENGTH) return collapsed;
  const cut = collapsed.slice(0, SNIPPET_MAX_LENGTH);
  const lastSpace = cut.lastIndexOf(" ");
  return (lastSpace > 0 ? cut.slice(0, lastSpace) : cut) + "…";
};
