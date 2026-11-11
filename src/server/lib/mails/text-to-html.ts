/**
 * Plain-text bodies are stored in the `html` column as-is (receive stores
 * `parsed.html || parsed.text`), and the viewer renders that column as HTML.
 * Rendered raw, a plain-text mail loses every line break, decodes entity-like
 * runs (`?a=1&params=2` shows as `?a=1¶ms=2`) and treats a literal `<` as
 * markup. These helpers turn such a body into HTML that shows the text as
 * written.
 */

const escapeHtml = (text: string): string =>
  text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");

// Stops before trailing punctuation that usually ends a sentence rather than
// the URL ("see https://x.com/a." links "https://x.com/a").
const URL_PATTERN = /\bhttps?:\/\/[^\s<>"]+[^\s<>".,;:!?)\]'}]/g;

/** Escape plain text and turn bare http(s) URLs into links. */
const linkify = (line: string): string => {
  let out = "";
  let last = 0;
  for (const match of line.matchAll(URL_PATTERN)) {
    const index = match.index ?? 0;
    out += escapeHtml(line.slice(last, index));
    const url = escapeHtml(match[0]);
    out += `<a href="${url}">${url}</a>`;
    last = index + match[0].length;
  }
  return out + escapeHtml(line.slice(last));
};

/**
 * Convert a plain-text body to HTML: escaped, URLs linked, line breaks and
 * runs of spaces kept (`white-space: pre-wrap`), and `>`-quoted lines
 * wrapped in a `<blockquote>` per quote level so the viewer folds them like
 * an HTML reply.
 */
export const plainTextToHtml = (text: string): string => {
  const lines = text.replace(/\r\n?/g, "\n").split("\n");
  let depth = 0;
  let out = "";
  for (const raw of lines) {
    const quote = /^((?:>\s?)+)/.exec(raw);
    const level = quote ? (quote[1].match(/>/g) ?? []).length : 0;
    const line = quote ? raw.slice(quote[1].length) : raw;
    while (depth < level) {
      out += "<blockquote>";
      depth++;
    }
    while (depth > level) {
      out += "</blockquote>";
      depth--;
    }
    out += linkify(line) + "\n";
  }
  while (depth > 0) {
    out += "</blockquote>";
    depth--;
  }
  // A blockquote is a block of its own, so a line break against its edge
  // would show as an extra blank line under pre-wrap.
  const body = out.replace(/\n$/, "").replace(/\n(<\/?blockquote>)/g, "$1");
  return `<div style="white-space: pre-wrap">${body}</div>`;
};

/**
 * The HTML to show for a stored mail. A body whose `html` column is empty or
 * holds exactly the plain text was never HTML, so it is converted; any other
 * value is the mail's own HTML and is returned untouched.
 */
export const bodyHtml = (
  html: string | null | undefined,
  text: string | null | undefined
): string => {
  const h = html ?? "";
  const t = text ?? "";
  if (t && (!h || h === t)) return plainTextToHtml(t);
  return h;
};
