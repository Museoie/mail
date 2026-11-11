const MINUTE = 60 * 1000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

const plural = (n: number, unit: string) =>
  `${n} ${unit}${n === 1 ? "" : "s"} ago`;

/**
 * How long ago `then` was, in the largest unit that fits: minutes under an
 * hour, hours under a day, days under a week, and so on. Each unit starts
 * where the one below ends, so neighbouring rows never read "40 hours ago"
 * next to "2 days ago".
 */
export const getRelativeTime = (then: Date, now: number = Date.now()) => {
  const elapsed = now - +then;
  if (elapsed < MINUTE) return "just now";
  if (elapsed < HOUR) return plural(Math.floor(elapsed / MINUTE), "minute");
  if (elapsed < DAY) return plural(Math.floor(elapsed / HOUR), "hour");
  if (elapsed < 7 * DAY) return plural(Math.floor(elapsed / DAY), "day");
  if (elapsed < 30 * DAY) return plural(Math.floor(elapsed / (7 * DAY)), "week");
  if (elapsed < 365 * DAY) {
    return plural(Math.max(1, Math.floor(elapsed / (30.42 * DAY))), "month");
  }
  return plural(Math.floor(elapsed / (365 * DAY)), "year");
};

export const getDateForMailHeader = (dateObject: Date) => {
  const date = dateObject.toLocaleDateString("en-US", {
    weekday: "long",
    year: "numeric",
    month: "long",
    day: "numeric"
  });

  const time = dateObject.toLocaleTimeString("en-US", {
    hour: "2-digit",
    minute: "2-digit"
  });

  return { date, time, duration: getRelativeTime(dateObject) };
};

/** A byte count as a short human-readable size: "512 B", "21 KB", "3.4 MB". */
export const formatFileSize = (bytes: number): string => {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  const rounded = value < 10 ? Math.round(value * 10) / 10 : Math.round(value);
  return `${rounded} ${units[unit]}`;
};
