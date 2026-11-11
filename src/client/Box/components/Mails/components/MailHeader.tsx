import { ComponentProps, useState } from "react";
import { getDateForMailHeader, onKeyboardActivate, Density } from "client";
import {
  MailAddressType,
  MailHeaderData
} from "common";

export interface MailHeaderProps extends Omit<
  ComponentProps<"div">,
  "onClick"
> {
  mail: MailHeaderData;
  density: Density;
  isSent: boolean;
  onClick?: () => void;
}

// Long address lists collapse after the first two (design doc §6), with a
// "show all" toggle. The toggle lives inside the header's clickable row, so
// it stops propagation on click AND keydown: without the keydown stop, Enter
// or Space on the button would bubble to the header's onKeyboardActivate and
// collapse the message instead of expanding the list.
export const AddressList = ({
  label,
  address
}: {
  label: string;
  address?: MailAddressType;
}) => {
  const [expanded, setExpanded] = useState(false);
  const raw = address?.value;
  const values = (Array.isArray(raw) ? raw : raw ? [raw] : [])
    .map((e) =>
      // Show the address next to the name: a display name alone can't tell
      // the reader who actually sent the mail, or which mailbox it went to.
      e?.name && e?.address && e.name !== e.address
        ? `${e.name} <${e.address}>`
        : e?.name || e?.address
    )
    .filter((e): e is string => !!e);
  if (!values.length) return <></>;
  const shown =
    expanded || values.length <= 2 ? values : values.slice(0, 2);
  return (
    <div className="mailcard-small content address-list">
      {label + ": " + shown.join(", ")}
      {values.length > 2 && (
        <button
          type="button"
          className="address-toggle"
          aria-expanded={expanded}
          onClick={(e) => {
            e.stopPropagation();
            setExpanded((v) => !v);
          }}
          onKeyDown={(e) => e.stopPropagation()}
        >
          {expanded ? "show fewer" : `show all ${values.length}`}
        </button>
      )}
    </div>
  );
};

const MailHeader = (props: MailHeaderProps) => {
  const { mail, density, isSent, onClick, onMouseLeave } = props;
  const { duration } = getDateForMailHeader(new Date(mail.date));

  // List row (design doc §5): sender + relative date on the first line,
  // subject on the second, snippet on the third. The snippet stays out of
  // search results (the highlight is shown instead). Opening a mail goes to
  // its own page (see Thread).
  const sender = isSent
    ? "me"
    : mail?.from?.value?.map((e) => e?.name || e?.address).join(", ");
  const hasHighlight =
    !!mail.highlight &&
    (mail.highlight.subject?.length ?? 0) +
      (mail.highlight.text?.length ?? 0) >
      0;
  const showSnippet =
    density === "comfortable" && !!mail.snippet && !hasHighlight;
  return (
    <div
      className="header cursor"
      role="button"
      tabIndex={0}
      onClick={onClick}
      onKeyDown={onClick ? onKeyboardActivate(onClick) : undefined}
      onMouseLeave={onMouseLeave}
    >
      <div className="mailcard-line1 content">
        {!mail.read && <span className="unread-dot" aria-hidden="true" />}
        <span className="sender">{sender}</span>
        <span className="mailcard-date">{duration}</span>
      </div>
      <div className="mailcard-subject content">
        {mail.spam ? <span className="spam-tag">Spam</span> : null}
        {mail.subject || <span className="no-subject">(No Subject)</span>}
      </div>
      {showSnippet && (
        <div className="mailcard-snippet content">{mail.snippet}</div>
      )}
    </div>
  );
};

export default MailHeader;
