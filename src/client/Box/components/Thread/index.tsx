import {
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState
} from "react";
import { useQuery, useQueryClient } from "react-query";
import { useLocation, useNavigate } from "react-router-dom";
import { MailHeaderData } from "common";
import {
  MarkMailPostBody,
  MarkMailPostResponse,
  SpamMarkPostBody,
  SpamMarkPostResponse,
  ThreadGetResponse
} from "server";
import {
  Context,
  Category,
  call,
  getDateForMailHeader,
  isSentMail,
  useFeedback,
  useIsOnline,
  useResolvedTheme
} from "client";
import {
  MailBody,
  ReplyIcon,
  ShareIcon,
  TrashIcon,
  EmptyStarIcon,
  SolidStarIcon,
  NewTabIcon,
  BanIcon,
  CircleCheckIcon
} from "../Mails/components";
import { AddressList } from "../Mails/components/MailHeader";
import { openMailInNewTab, useStartReply } from "../Mails/mailActions";
import { mailPagePath, openedFromList } from "../Mails/mailPage";
import {
  UNDO_WINDOW_MS,
  cancelDelete,
  scheduleDelete,
  sendDelete
} from "../Mails/pendingDeletes";
import {
  initialExpanded,
  markReadInCaches,
  refreshMailQueries,
  restoreCaches,
  snapshotCaches,
  threadItems,
  threadQueryUrl,
  updateMailInCaches
} from "./threadState";

import "./index.scss";

const fetchThread = async (url: string) => {
  const { status, body, message } = await call.get<ThreadGetResponse>(url);
  if (status === "success" && body) return body.map((m) => new MailHeaderData(m));
  throw new Error(message || "Couldn't load this conversation.");
};

const senderName = (mail: MailHeaderData, isSent: boolean) =>
  isSent
    ? "Me"
    : mail.from?.value?.[0]?.name || mail.from?.value?.[0]?.address || "Unknown";

const senderAddress = (mail: MailHeaderData) =>
  mail.from?.value?.[0]?.address ?? "";

/** The coloured circle with the sender's initial, as mail apps show. */
const Avatar = ({ name }: { name: string }) => {
  const letter = (name.trim()[0] ?? "?").toUpperCase();
  let hash = 0;
  for (const c of name) hash = (hash * 31 + c.charCodeAt(0)) >>> 0;
  return (
    <span
      className="thread-avatar"
      style={{ backgroundColor: `hsl(${hash % 360} 45% 42%)` }}
      aria-hidden="true"
    >
      {letter}
    </span>
  );
};

interface MessageProps {
  mail: MailHeaderData;
  expanded: boolean;
  /** The only message in the thread can't collapse. */
  canCollapse: boolean;
  onToggle: () => void;
  isSent: boolean;
  isSpamView: boolean;
  onReply: () => void;
  onForward: () => void;
  onToggleSaved: () => void;
  onSpam: () => void;
  onDelete: () => void;
  onOpenInNewTab: () => void;
  isOnline: boolean;
}

const ThreadMessage = ({
  mail,
  expanded,
  canCollapse,
  onToggle,
  isSent,
  isSpamView,
  onReply,
  onForward,
  onToggleSaved,
  onSpam,
  onDelete,
  onOpenInNewTab,
  isOnline
}: MessageProps) => {
  const { date, time, duration } = getDateForMailHeader(new Date(mail.date));
  const name = senderName(mail, isSent);
  const offlineTitle = isOnline
    ? undefined
    : "You're offline — reconnect to make changes";

  if (!expanded) {
    return (
      <li className="thread-message collapsed">
        <button
          type="button"
          className="thread-message-summary"
          aria-expanded={false}
          onClick={onToggle}
        >
          <Avatar name={name} />
          <span className="summary-text">
            <span className="summary-line">
              <span className={"summary-sender" + (mail.read ? "" : " unread")}>
                {name}
              </span>
              <span className="summary-date">{duration}</span>
            </span>
            <span className="summary-snippet">{mail.snippet || ""}</span>
          </span>
        </button>
      </li>
    );
  }

  return (
    <li className="thread-message expanded" data-mail-id={mail.id}>
      <div className="thread-message-head">
        <button
          type="button"
          className="thread-message-who"
          aria-expanded={true}
          disabled={!canCollapse}
          onClick={onToggle}
          title={canCollapse ? "Collapse" : undefined}
        >
          <Avatar name={name} />
          <span className="who-text">
            <span className="who-name">
              {name}
              {!isSent && senderAddress(mail) ? (
                <span className="who-address">&lt;{senderAddress(mail)}&gt;</span>
              ) : null}
            </span>
            <span className="who-date">
              <time dateTime={new Date(mail.date).toISOString()}>
                {date}, {time}
              </time>{" "}
              ({duration})
            </span>
          </span>
        </button>
        {mail.saved ? (
          <SolidStarIcon className="star thread-saved" aria-label="Saved" />
        ) : null}
      </div>
      <div className="thread-message-addresses">
        <AddressList label="to" address={mail.to} />
        {mail.cc?.value ? <AddressList label="cc" address={mail.cc} /> : null}
        {mail.bcc?.value ? <AddressList label="bcc" address={mail.bcc} /> : null}
      </div>
      <div className="actionRow" role="toolbar" aria-label="Message actions">
        <button type="button" className="actionButton" onClick={onReply}>
          <ReplyIcon />
          <span>Reply</span>
        </button>
        <button type="button" className="actionButton" onClick={onForward}>
          <ShareIcon />
          <span>Forward</span>
        </button>
        <button
          type="button"
          className={"actionButton" + (isOnline ? "" : " disabled")}
          title={offlineTitle}
          onClick={onToggleSaved}
        >
          {mail.saved ? <SolidStarIcon className="star" /> : <EmptyStarIcon />}
          <span>{mail.saved ? "Unsave" : "Save"}</span>
        </button>
        <button
          type="button"
          className="actionButton"
          title="Open this email body in a new tab"
          onClick={onOpenInNewTab}
        >
          <NewTabIcon />
          <span>Open in new tab</span>
        </button>
        {!isSent ? (
          <button
            type="button"
            className={"actionButton" + (isOnline ? "" : " disabled")}
            title={offlineTitle}
            onClick={onSpam}
          >
            {isSpamView ? <CircleCheckIcon /> : <BanIcon />}
            <span>{isSpamView ? "Not spam" : "Mark spam"}</span>
          </button>
        ) : null}
        <button
          type="button"
          className={"actionButton" + (isOnline ? "" : " disabled")}
          title={offlineTitle}
          onClick={onDelete}
        >
          <TrashIcon />
          <span>Delete</span>
        </button>
      </div>
      <MailBody mailId={mail.id} />
    </li>
  );
};

/**
 * A mail on its own page with the rest of its conversation, oldest first,
 * like Gmail and iOS Mail: the opened message, the newest one and any unread
 * ones open; the rest one line each, a long run of them folded into "N more".
 */
const Thread = ({ mailId }: { mailId: string }) => {
  const navigate = useNavigate();
  const location = useLocation();
  const queryClient = useQueryClient();
  const {
    domainName,
    theme = "system",
    isWriterOpen,
    isSettingsOpen,
    selectedCategory
  } = useContext(Context);
  const resolvedTheme = useResolvedTheme(theme);
  const { isOnline } = useIsOnline();
  const { showToast } = useFeedback();
  const startReply = useStartReply();

  const url = threadQueryUrl(mailId);
  const query = useQuery<MailHeaderData[]>(url, () => fetchThread(url), {
    staleTime: 30_000
  });
  const thread = useMemo(() => query.data ?? [], [query.data]);

  const [expanded, setExpanded] = useState<Set<string> | null>(null);
  useEffect(() => setExpanded(null), [mailId]);
  useEffect(() => {
    if (expanded === null && thread.length) {
      setExpanded(initialExpanded(thread, mailId));
    }
  }, [expanded, thread, mailId]);

  // Opening a message fetches its body, which marks it read on the server;
  // mirror that in the lists and counts already on screen.
  useEffect(() => {
    if (!expanded) return;
    const read = thread
      .filter((m) => !m.read && expanded.has(m.id))
      .map((m) => m.id);
    if (read.length && isOnline) markReadInCaches(queryClient, read);
  }, [expanded, thread, queryClient, isOnline]);

  // Bring the opened message into view when it isn't the first one.
  const listRef = useRef<HTMLOListElement>(null);
  const scrolledFor = useRef("");
  useEffect(() => {
    if (!expanded || scrolledFor.current === mailId) return;
    const el = listRef.current?.querySelector(
      `[data-mail-id="${CSS.escape(mailId)}"]`
    );
    if (el && thread[0]?.id !== mailId) {
      el.scrollIntoView({ block: "start" });
    }
    scrolledFor.current = mailId;
  }, [expanded, mailId, thread]);

  const goBack = useCallback(() => {
    // Back to the list the mail was opened from; a page opened directly
    // (a reload, a shared link) has no in-app history to go back to.
    if (openedFromList(location.state)) navigate(-1);
    else navigate("/", { replace: true });
  }, [location.state, navigate]);

  // Escape goes back to the list, unless something on top owns it.
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || e.defaultPrevented) return;
      if (isWriterOpen || isSettingsOpen) return;
      if (document.querySelector('[role="dialog"], [role="alertdialog"]')) return;
      const target = e.target as HTMLElement | null;
      if (
        target &&
        (target.tagName === "INPUT" ||
          target.tagName === "TEXTAREA" ||
          target.tagName === "SELECT" ||
          target.isContentEditable)
      ) {
        return;
      }
      goBack();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [goBack, isWriterOpen, isSettingsOpen]);

  const toggle = (id: string) =>
    setExpanded((current) => {
      const next = new Set(current ?? []);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const expandAll = (ids: string[]) =>
    setExpanded((current) => new Set([...(current ?? []), ...ids]));

  /** What's left of the thread after `id` leaves it; leaves the page if nothing. */
  const afterRemoval = (id: string) => {
    const remaining = thread.filter((m) => m.id !== id);
    if (!remaining.length) goBack();
  };

  const onToggleSaved = (mail: MailHeaderData) => {
    if (!isOnline) return;
    const save = !mail.saved;
    updateMailInCaches(queryClient, mail.id, (m) =>
      new MailHeaderData({ ...m, saved: save })
    );
    const body: MarkMailPostBody = { mail_id: mail.id, save };
    call
      .post<MarkMailPostResponse, MarkMailPostBody>("/api/mails/mark", body)
      .finally(() => refreshMailQueries(queryClient));
  };

  const isSpamView = selectedCategory === Category.SpamMails || !!thread[0]?.spam;

  const onSpam = (mail: MailHeaderData) => {
    if (!isOnline) return;
    const nextIsSpam = !mail.spam && !isSpamView;
    updateMailInCaches(queryClient, mail.id, () => null);
    afterRemoval(mail.id);
    const body: SpamMarkPostBody = { mail_id: mail.id, is_spam: nextIsSpam };
    call
      .post<SpamMarkPostResponse, SpamMarkPostBody>("/api/mails/spam/mark", body)
      .finally(() => refreshMailQueries(queryClient));
    showToast({ message: nextIsSpam ? "Marked as spam." : "Moved out of spam." });
  };

  const onDelete = (mail: MailHeaderData) => {
    if (!isOnline) return;
    const snapshot = snapshotCaches(queryClient);
    updateMailInCaches(queryClient, mail.id, () => null);
    const leaving = thread.length <= 1;
    afterRemoval(mail.id);
    scheduleDelete(mail.id, () => {
      sendDelete(mail.id).finally(() => refreshMailQueries(queryClient));
    });
    showToast({
      message: "Mail deleted.",
      duration: UNDO_WINDOW_MS,
      action: {
        label: "Undo",
        onClick: () => {
          if (!cancelDelete(mail.id)) return;
          restoreCaches(queryClient, snapshot);
          // The page was left when its last message went; come back to it.
          if (leaving) navigate(mailPagePath(mail.id));
        }
      }
    });
  };

  if (query.isLoading || (thread.length && !expanded)) {
    return (
      <section className="thread" aria-label="Conversation" aria-busy="true">
        <div className="thread-bar">
          <button type="button" className="thread-back" onClick={goBack}>
            <BackIcon />
            <span>Back</span>
          </button>
        </div>
        <div className="thread-loading loading_animation" role="status">
          Loading conversation…
        </div>
      </section>
    );
  }

  if (query.error || !thread.length) {
    return (
      <section className="thread" aria-label="Conversation">
        <div className="thread-bar">
          <button type="button" className="thread-back" onClick={goBack}>
            <BackIcon />
            <span>Back</span>
          </button>
        </div>
        <p className="thread-empty">
          This mail couldn&apos;t be found. It may have been deleted.
        </p>
      </section>
    );
  }

  const subject = thread[0].subject.replace(
    /^\s*((re|fwd?)\s*:\s*)+/i,
    ""
  );
  const items = threadItems(thread, expanded ?? new Set());
  const last = thread[thread.length - 1];

  return (
    <section className="thread" aria-label="Conversation">
      <div className="thread-bar">
        <button type="button" className="thread-back" onClick={goBack}>
          <BackIcon />
          <span>Back</span>
        </button>
        <span className="thread-count">
          {thread.length === 1 ? "1 message" : `${thread.length} messages`}
        </span>
      </div>
      <h2 className="thread-subject">
        {thread[0].spam ? <span className="spam-tag">Spam</span> : null}
        {subject || <span className="no-subject">(No Subject)</span>}
      </h2>
      <ol className="thread-messages" ref={listRef}>
        {items.map((item) => {
          if (item.kind === "fold") {
            return (
              <li key={`fold-${item.ids[0]}`} className="thread-fold">
                <button type="button" onClick={() => expandAll(item.ids)}>
                  {item.ids.length} more messages
                </button>
              </li>
            );
          }
          const mail = item.mail;
          const isSent = isSentMail(mail, domainName);
          return (
            <ThreadMessage
              key={mail.id}
              mail={mail}
              expanded={!!expanded?.has(mail.id)}
              canCollapse={thread.length > 1}
              onToggle={() => toggle(mail.id)}
              isSent={isSent}
              isSpamView={isSpamView}
              onReply={() => startReply(mail, "reply").catch(console.error)}
              onForward={() => startReply(mail, "forward").catch(console.error)}
              onToggleSaved={() => onToggleSaved(mail)}
              onSpam={() => onSpam(mail)}
              onDelete={() => onDelete(mail)}
              onOpenInNewTab={() =>
                openMailInNewTab(queryClient, mail.id, resolvedTheme).catch(
                  console.error
                )
              }
              isOnline={isOnline}
            />
          );
        })}
      </ol>
      <div className="thread-reply-bar">
        <button
          type="button"
          className="thread-reply-button"
          onClick={() => startReply(last, "reply").catch(console.error)}
        >
          <ReplyIcon />
          <span>Reply</span>
        </button>
        <button
          type="button"
          className="thread-reply-button"
          onClick={() => startReply(last, "forward").catch(console.error)}
        >
          <ShareIcon />
          <span>Forward</span>
        </button>
      </div>
    </section>
  );
};

const BackIcon = () => (
  <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
    <path
      d="M10 3L5 8l5 5"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
    />
  </svg>
);

export default Thread;
