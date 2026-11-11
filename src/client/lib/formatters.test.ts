import { describe, test, expect } from "bun:test";
import { formatFileSize, getRelativeTime } from "./formatters";

const NOW = Date.UTC(2026, 10, 11, 12, 0, 0);
const ago = (ms: number) => getRelativeTime(new Date(NOW - ms), NOW);
const MIN = 60_000;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;

describe("getRelativeTime", () => {
  test("under a minute is just now", () => {
    expect(ago(30_000)).toBe("just now");
  });

  test("uses singular units for one", () => {
    expect(ago(MIN)).toBe("1 minute ago");
    expect(ago(HOUR)).toBe("1 hour ago");
    expect(ago(DAY)).toBe("1 day ago");
    expect(ago(7 * DAY)).toBe("1 week ago");
  });

  test("switches from hours to days at one day", () => {
    expect(ago(23 * HOUR)).toBe("23 hours ago");
    expect(ago(40 * HOUR)).toBe("1 day ago");
    expect(ago(50 * HOUR)).toBe("2 days ago");
  });

  test("covers weeks, months and years", () => {
    expect(ago(20 * DAY)).toBe("2 weeks ago");
    expect(ago(31 * DAY)).toBe("1 month ago");
    expect(ago(200 * DAY)).toBe("6 months ago");
    expect(ago(800 * DAY)).toBe("2 years ago");
  });

  test("a future date reads as just now", () => {
    expect(ago(-5 * MIN)).toBe("just now");
  });
});

describe("formatFileSize", () => {
  test("bytes, kilobytes and megabytes", () => {
    expect(formatFileSize(21)).toBe("21 B");
    expect(formatFileSize(2000)).toBe("2 KB");
    expect(formatFileSize(1536)).toBe("1.5 KB");
    expect(formatFileSize(25 * 1024)).toBe("25 KB");
    expect(formatFileSize(3.4 * 1024 * 1024)).toBe("3.4 MB");
  });
});
