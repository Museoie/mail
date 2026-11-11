/**
 * A loose shape check for one address as typed in To/Cc/Bcc: a local part,
 * an @, and a domain with a dot. It catches typos such as a missing @ or
 * domain before the mail goes out; the server's own check stays the
 * authority. A "Name <address>" entry is checked by its address.
 */
const ADDRESS = /^[^\s@<>,;]+@[^\s@<>,;]+\.[^\s@<>,;]+$/;

const addressOf = (entry: string): string => {
  const angled = /<([^>]*)>\s*$/.exec(entry);
  return (angled ? angled[1] : entry).trim();
};

/** The entries of a comma- or semicolon-separated list that aren't addresses. */
export const invalidAddresses = (list: string): string[] =>
  list
    .split(/[,;]/)
    .map((e) => e.trim())
    .filter((e) => e && !ADDRESS.test(addressOf(e)));
