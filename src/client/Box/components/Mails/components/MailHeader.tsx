import { ComponentProps } from "react";
import { getDateForMailHeader, onKeyboardActivate, Density } from "client";
import { MailAddressValueType, MailHeaderData } from "common";

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
      <div className="mailcard-small content">
        {"from: " +
          mail?.from?.value?.map((e) => e?.name || e?.address).join(", ")}
      </div>
      <div className="mailcard-small content">
        {"to: " + mail?.to?.value?.map((e) => e?.name || e?.address).join(", ")}
      </div>
      {cc && cc.value ? (
        <div className="mailcard-small content">
          {"cc: " +
            (cc.value as MailAddressValueType[])
              .map((e) => e?.name || e?.address)
              .join(", ")}
        </div>
      ) : (
        <></>
      )}
      {bcc && bcc.value ? (
        <div className="mailcard-small content">
          {"bcc: " +
            (bcc.value as MailAddressValueType[])
              .map((e) => e?.name || e?.address)
              .join(", ")}
        </div>
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
