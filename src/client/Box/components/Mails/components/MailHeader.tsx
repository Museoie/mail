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
  isActive: boolean;
  density: Density;
  isSent: boolean;
  onClick?: () => void;
}

// Long address lists collapse after the first two (design doc §6), with a
// "show all" toggle. The toggle lives inside the header's clickable row, so
// it stops propagation on click AND keydown: without the keydown stop, Enter
// or Space on the button would bubble to the header's onKeyboardActivate and
// collapse the message instead of expanding the list.
const AddressList = ({
  label,
  address
}: {
  label: string;
  address?: MailAddressType;
}) => {
  const [expanded, setExpanded] = useState(false);
  const raw = address?.value;
  const values = (Array.isArray(raw) ? raw : raw ? [raw] : [])
    .map((e) => e?.name || e?.address)
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
  const { mail, isActive, density, isSent, onClick, onMouseLeave } = props;
  const { date, time, duration } = getDateForMailHeader(new Date(mail.date));

  const cc = "cc" in mail ? mail.cc : undefined;
  const bcc = "bcc" in mail ? mail.bcc : undefined;

  if (cc?.value && !Array.isArray(cc.value)) cc.value = [cc.value];
  if (bcc?.value && !Array.isArray(bcc.value)) bcc.value = [bcc.value];

  // Collapsed row (design doc §5): sender + relative date on the first line,
  // subject on the second, snippet on the third. The snippet stays out of the
  // expanded header (the body is shown there) and out of search results (the
  // highlight is shown instead).
  if (!isActive) {
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
          {mail.subject || <span className="no-subject">(No Subject)</span>}
        </div>
        {showSnippet && (
          <div className="mailcard-snippet content">{mail.snippet}</div>
        )}
      </div>
    );
  }

  return (
    <div
      className="header cursor"
      role="button"
      tabIndex={0}
      onClick={onClick}
      onKeyDown={onClick ? onKeyboardActivate(onClick) : undefined}
      onMouseLeave={onMouseLeave}
    >
      <div className="mailcard-small content">{duration}</div>
      <div className="mailcard-small content">
        {date}, {time}
      </div>
      <AddressList label="from" address={mail.from} />
      <AddressList label="to" address={mail.to} />
      {cc && cc.value ? <AddressList label="cc" address={cc} /> : <></>}
      {bcc && bcc.value ? (
        <AddressList label="bcc" address={bcc} />
      ) : (
        <></>
      )}
      <div className="mailcard-subject content">
        {mail.subject || <span className="no-subject">(No Subject)</span>}
      </div>
    </div>
  );
};

export default MailHeader;
