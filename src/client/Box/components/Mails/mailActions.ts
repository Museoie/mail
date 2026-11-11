import { useCallback, useContext } from "react";
import { QueryClient, useQueryClient } from "react-query";
import { MailBodyData, MailHeaderData, ReplyData } from "common";
import { BodyGetResponse } from "server";
import {
  Category,
  Context,
  Theme,
  call,
  isSentMail,
  processHtmlForViewer
} from "client";

export const bodyQueryUrl = (mailId: string) => `/api/mails/body/${mailId}`;

/** A mail's body, from the cache the viewer fills, or fetched. */
export const fetchMailBody = (queryClient: QueryClient, mailId: string) => {
  const url = bodyQueryUrl(mailId);
  return queryClient.fetchQuery<MailBodyData>(
    url,
    () =>
      call.get<BodyGetResponse>(url).then(({ status, body, message }) => {
        if (status === "success") return new MailBodyData(body);
        throw new Error(message);
      }),
    { staleTime: Infinity }
  );
};

const addressesOf = (mail: MailHeaderData): string[] =>
  [mail.to, mail.cc, mail.bcc].flatMap((field) => {
    const value = field?.value;
    const list = Array.isArray(value) ? value : value ? [value] : [];
    return list.map((e) => e.address).filter((a): a is string => !!a);
  });

/**
 * The user's own address a received mail came in on — the mailbox a reply
 * goes out from. Falls back to `fallback` (the selected mailbox) when no
 * recipient is on this domain, e.g. a Bcc delivery.
 */
export const ownAddressFor = (
  mail: MailHeaderData,
  domainName: string,
  fallback: string
): string => {
  const domain = domainName.toLowerCase();
  const own = addressesOf(mail).find((address) => {
    const host = address.toLowerCase().split("@")[1] ?? "";
    return !!domain && (host === domain || host.endsWith("." + domain));
  });
  return own ?? fallback;
};

/**
 * What the composer needs to answer or forward `mail`. A reply to the user's
 * own sent mail goes back to that mail's recipients, from the same address.
 */
export const buildReplyData = (
  mail: MailHeaderData,
  kind: "reply" | "forward",
  domainName: string,
  fallbackAccount: string
): ReplyData => {
  if (kind === "forward") return { ...mail, to: { address: "" } };
  if (isSentMail(mail, domainName)) {
    const ownAddress = mail.from?.value?.[0]?.address || fallbackAccount;
    return { ...mail, from: mail.to, to: { address: ownAddress } };
  }
  return {
    ...mail,
    to: { address: ownAddressFor(mail, domainName, fallbackAccount) }
  };
};

/**
 * Opens the composer answering or forwarding `mail`. The composer quotes the
 * original, so its body is fetched first (from cache when already open).
 */
export const useStartReply = () => {
  const queryClient = useQueryClient();
  const {
    domainName,
    selectedAccount,
    selectedCategory,
    setReplyData,
    setIsWriterOpen
  } = useContext(Context);
  // While searching, `selectedAccount` holds the search term.
  const fallbackAccount =
    selectedCategory === Category.Search ? "" : selectedAccount;

  return useCallback(
    async (mail: MailHeaderData, kind: "reply" | "forward") => {
      const body = await fetchMailBody(queryClient, mail.id);
      setReplyData({
        ...buildReplyData(mail, kind, domainName, fallbackAccount),
        html: body.html,
        messageId: body.messageId
      });
      setIsWriterOpen(true);
    },
    [queryClient, domainName, fallbackAccount, setReplyData, setIsWriterOpen]
  );
};

/**
 * Opens a mail's body in a new tab, inside a sandboxed iframe — NOT directly
 * in the top document. A `blob:` document inherits this page's origin, so any
 * script in the email would otherwise run with the app's origin (cookies,
 * session). The shell carries no script of its own and confines the email
 * to an iframe whose sandbox matches the inline viewer (no `allow-scripts`,
 * no top-navigation), so email scripts stay inert and `<meta refresh>` can't
 * redirect the tab. The sanitizer stays defense-in-depth. The shell carries
 * its own `<meta name="referrer">`, and matches the viewer's color scheme
 * and panel, or theme-colored text would sit on a default white page.
 */
export const openMailInNewTab = async (
  queryClient: QueryClient,
  mailId: string,
  theme: Theme
) => {
  const data = await fetchMailBody(queryClient, mailId);
  const processed = processHtmlForViewer(data.html, { theme });
  const panel = theme === "light" ? "#ffffff" : "#222222";
  // Escape for embedding in the double-quoted `srcdoc` attribute.
  const srcdoc = processed.replace(/&/g, "&amp;").replace(/"/g, "&quot;");
  const shell =
    '<!doctype html><html><head><meta charset="utf-8" />' +
    '<meta name="referrer" content="no-referrer" />' +
    `<style>:root{color-scheme:${theme}}` +
    `html,body{margin:0;padding:0;height:100%;background:${panel}}` +
    "iframe{border:0;width:100%;height:100%}</style></head>" +
    '<body><iframe sandbox="allow-same-origin allow-popups allow-popups-to-escape-sandbox" ' +
    `srcdoc="${srcdoc}"></iframe></body></html>`;
  const blob = new Blob([shell], { type: "text/html;charset=utf-8" });
  const url = URL.createObjectURL(blob);
  window.open(url, "_blank", "noopener,noreferrer");
  // Revoke after a delay so the new tab has a chance to load.
  setTimeout(() => URL.revokeObjectURL(url), 60_000);
};
