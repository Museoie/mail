//! Mailbox names and other IMAP helpers (imap/util.ts).
//!
//! Also home to [`JsDate`], the JavaScript `Date` the Bun server builds from
//! SEARCH date arguments and from stored `date` columns. `Date.parse` accepts
//! what JavaScriptCore accepts, which is a good deal more than the ECMAScript
//! format, and a SEARCH date is whatever the client typed.

use base64::Engine;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::types::{js_number_string, FetchMailInput, MailAddressValue, MailAttachment};
use crate::logger;

// ---------------------------------------------------------------------------
// JavaScript string semantics
// ---------------------------------------------------------------------------

/// Whether `c` is white space to JavaScript: what `\s` matches and `trim()` strips.
pub fn js_is_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}'
            | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// `value.trim()`.
pub fn js_trim(value: &str) -> &str {
    value.trim_matches(js_is_whitespace)
}

/// `value.replace(/^\s+/, "")`.
pub fn js_trim_start(value: &str) -> &str {
    value.trim_start_matches(js_is_whitespace)
}

// ---------------------------------------------------------------------------
// JavaScript dates
// ---------------------------------------------------------------------------

const MS_PER_DAY: i64 = 86_400_000;
/// ECMAScript's time range: ±100,000,000 days around the epoch.
const MAX_TIME_MS: f64 = 8.64e15;

const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const WEEKDAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// A JavaScript `Date`: milliseconds since the epoch, or an Invalid Date.
///
/// The server's local time zone is UTC, as it is for the Bun server in its
/// container, so the local-time getters `format_internal_date` reads are the
/// UTC ones and a date string with no zone is read as UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsDate(Option<i64>);

/// A date in UTC: month 1..=12, weekday 0 for Sunday.
struct DateFields {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    ms: i64,
    weekday: usize,
}

impl JsDate {
    /// `new Date(ms)`.
    pub fn from_ms(ms: i64) -> JsDate {
        JsDate(time_clip(ms as f64))
    }

    /// An Invalid Date: `new Date(NaN)`.
    pub fn invalid() -> JsDate {
        JsDate(None)
    }

    /// `new Date()`.
    pub fn now() -> JsDate {
        JsDate::from_ms(crate::js::now_ms())
    }

    /// `new Date(input)` for a string.
    pub fn parse(input: &str) -> JsDate {
        JsDate(date_parse(input))
    }

    /// `getTime()`, or `None` where JavaScript has NaN.
    pub fn timestamp_millis(&self) -> Option<i64> {
        self.0
    }

    pub fn is_valid(&self) -> bool {
        self.0.is_some()
    }

    /// The calendar fields of a valid date.
    fn fields(&self) -> Option<DateFields> {
        let ms = self.0?;
        let days = ms.div_euclid(MS_PER_DAY);
        let in_day = ms.rem_euclid(MS_PER_DAY);
        let (year, month, day) = civil_from_days(days);
        Some(DateFields {
            year,
            month,
            day,
            hour: in_day / 3_600_000,
            minute: in_day / 60_000 % 60,
            second: in_day / 1000 % 60,
            ms: in_day % 1000,
            weekday: (days + 4).rem_euclid(7) as usize,
        })
    }

    /// `toISOString()`, or `None` where JavaScript throws a RangeError.
    pub fn to_iso_string(&self) -> Option<String> {
        let DateFields { year, month, day, hour, minute, second, ms, .. } = self.fields()?;
        let year = if (0..=9999).contains(&year) {
            format!("{year:04}")
        } else {
            format!("{}{:06}", if year < 0 { '-' } else { '+' }, year.abs())
        };
        Some(format!("{year}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{ms:03}Z"))
    }

    /// `JSON.stringify(date)`: the ISO string in quotes, or `null`.
    pub fn to_json_string(&self) -> String {
        match self.to_iso_string() {
            Some(iso) => format!("\"{iso}\""),
            None => "null".to_string(),
        }
    }

    /// `toUTCString()`, e.g. `Sat, 21 Nov 2026 05:53:08 GMT`.
    pub fn to_utc_string(&self) -> String {
        let Some(DateFields { year, month, day, hour, minute, second, weekday, .. }) = self.fields() else {
            return "Invalid Date".to_string();
        };
        format!(
            "{}, {day:02} {} {}{:04} {hour:02}:{minute:02}:{second:02} GMT",
            WEEKDAY_NAMES[weekday],
            MONTH_NAMES[(month - 1) as usize],
            if year < 0 { "-" } else { "" },
            year.abs()
        )
    }

    /// The same instant for chrono, where it can represent it.
    pub fn to_chrono(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        chrono::DateTime::from_timestamp_millis(self.0?)
    }
}

/// ECMAScript TimeClip: NaN outside the time range, truncated to an integer inside it.
fn time_clip(ms: f64) -> Option<i64> {
    if !ms.is_finite() || ms.abs() > MAX_TIME_MS {
        return None;
    }
    Some(ms.trunc() as i64)
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

// Bun's `Date.parse` reads a string the way V8 does (Node compatibility), not
// the way stock JavaScriptCore does: one tokenizer, an ECMAScript date-time
// prefix, and a lenient "legacy" grammar for whatever is left. The port below
// keeps V8's structure (`dateparser.h`, `dateparser-inl.h`, `dateparser.cc`)
// so that the two can be compared side by side.

/// "No value": a time zone hour, a named month.
const NONE: i64 = i64::MAX;
/// Digits of a numeral that count toward its value; the rest are skipped.
const MAX_SIGNIFICANT_DIGITS: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keyword {
    /// A word the parser does not know.
    Garbage,
    MonthName,
    AmPm,
    TimeZoneName,
    /// The `T` between an ECMAScript date and time.
    TimeSeparator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DateToken {
    Number { value: i64, length: usize },
    Symbol(char),
    WhiteSpace,
    Keyword { kind: Keyword, value: i64, length: usize },
    Unknown,
    EndOfInput,
}

impl DateToken {
    fn is_number(self) -> bool {
        matches!(self, DateToken::Number { .. })
    }

    fn is_fixed_number(self, digits: usize) -> bool {
        matches!(self, DateToken::Number { length, .. } if length == digits)
    }

    fn number(self) -> i64 {
        match self {
            DateToken::Number { value, .. } => value,
            _ => 0,
        }
    }

    fn is_symbol(self, symbol: char) -> bool {
        self == DateToken::Symbol(symbol)
    }

    fn is_ascii_sign(self) -> bool {
        self.is_symbol('+') || self.is_symbol('-')
    }

    fn is_keyword_z(self) -> bool {
        matches!(self, DateToken::Keyword { kind: Keyword::TimeZoneName, value: 0, length: 1 })
    }
}

/// White space between date tokens: the Unicode space separators, tab, line
/// feed, form feed, carriage return and U+FEFF. Not the vertical tab and not
/// the line and paragraph separators, which JavaScript otherwise counts.
fn is_date_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}'
            | '\u{3000}' | '\u{feff}'
    )
}

/// Names of months, time zones and am/pm, by three-letter lower-case prefix.
/// A word longer than its keyword only matches a month name.
const KEYWORDS: [(&str, Keyword, i64); 27] = [
    ("jan", Keyword::MonthName, 1),
    ("feb", Keyword::MonthName, 2),
    ("mar", Keyword::MonthName, 3),
    ("apr", Keyword::MonthName, 4),
    ("may", Keyword::MonthName, 5),
    ("jun", Keyword::MonthName, 6),
    ("jul", Keyword::MonthName, 7),
    ("aug", Keyword::MonthName, 8),
    ("sep", Keyword::MonthName, 9),
    ("oct", Keyword::MonthName, 10),
    ("nov", Keyword::MonthName, 11),
    ("dec", Keyword::MonthName, 12),
    ("am", Keyword::AmPm, 0),
    ("pm", Keyword::AmPm, 12),
    ("ut", Keyword::TimeZoneName, 0),
    ("utc", Keyword::TimeZoneName, 0),
    ("z", Keyword::TimeZoneName, 0),
    ("gmt", Keyword::TimeZoneName, 0),
    ("cdt", Keyword::TimeZoneName, -5),
    ("cst", Keyword::TimeZoneName, -6),
    ("edt", Keyword::TimeZoneName, -4),
    ("est", Keyword::TimeZoneName, -5),
    ("mdt", Keyword::TimeZoneName, -6),
    ("mst", Keyword::TimeZoneName, -7),
    ("pdt", Keyword::TimeZoneName, -7),
    ("pst", Keyword::TimeZoneName, -8),
    ("t", Keyword::TimeSeparator, 0),
];

fn lookup_keyword(prefix: [char; 3], length: usize) -> (Keyword, i64) {
    for (name, kind, value) in KEYWORDS {
        let mut padded = ['\0'; 3];
        for (slot, c) in padded.iter_mut().zip(name.chars()) {
            *slot = c;
        }
        if padded == prefix && (length <= 3 || kind == Keyword::MonthName) {
            return (kind, value);
        }
    }
    (Keyword::Garbage, 0)
}

/// Splits a date string into tokens, one ahead.
struct DateScanner {
    chars: Vec<char>,
    at: usize,
    next: DateToken,
}

impl DateScanner {
    fn new(input: &str) -> DateScanner {
        let mut scanner = DateScanner { chars: input.chars().collect(), at: 0, next: DateToken::EndOfInput };
        scanner.next = scanner.scan();
        scanner
    }

    /// The current character; NUL at the end, and a NUL in the string ends it.
    fn ch(&self) -> char {
        self.chars.get(self.at).copied().unwrap_or('\0')
    }

    fn skip(&mut self, c: char) -> bool {
        if self.ch() == c {
            self.at += 1;
            return true;
        }
        false
    }

    fn scan(&mut self) -> DateToken {
        let start = self.at;
        let c = self.ch();
        if c == '\0' {
            return DateToken::EndOfInput;
        }
        if c.is_ascii_digit() {
            // Leading zeros carry no value; digits past the ninth are skipped.
            while self.ch() == '0' {
                self.at += 1;
            }
            let mut value: i64 = 0;
            let mut significant = 0;
            while let Some(digit) = self.ch().to_digit(10).filter(|_| self.ch().is_ascii_digit()) {
                if significant < MAX_SIGNIFICANT_DIGITS {
                    value = value * 10 + digit as i64;
                }
                significant += 1;
                self.at += 1;
            }
            return DateToken::Number { value, length: self.at - start };
        }
        for symbol in [':', '-', '+', '.', ')'] {
            if self.skip(symbol) {
                return DateToken::Symbol(symbol);
            }
        }
        if c >= 'A' && !is_date_space(c) {
            // A word: every character from `A` up, letters or not.
            let mut prefix = ['\0'; 3];
            let mut length = 0;
            while self.ch() >= 'A' && !is_date_space(self.ch()) {
                if length < 3 {
                    prefix[length] = self.ch().to_ascii_lowercase();
                }
                length += 1;
                self.at += 1;
            }
            let (kind, value) = lookup_keyword(prefix, length);
            return DateToken::Keyword { kind, value, length };
        }
        if is_date_space(c) {
            self.at += 1;
            return DateToken::WhiteSpace;
        }
        if c == '(' {
            // Parenthesized text is ignored.
            let mut balance = 0;
            loop {
                match self.ch() {
                    ')' => balance -= 1,
                    '(' => balance += 1,
                    _ => {}
                }
                self.at += 1;
                if balance <= 0 || self.ch() == '\0' {
                    break;
                }
            }
            return DateToken::Unknown;
        }
        self.at += 1;
        DateToken::Unknown
    }

    fn take(&mut self) -> DateToken {
        let token = self.next;
        self.next = self.scan();
        token
    }

    fn peek(&self) -> DateToken {
        self.next
    }

    fn skip_symbol(&mut self, symbol: char) -> bool {
        if self.next.is_symbol(symbol) {
            self.next = self.scan();
            return true;
        }
        false
    }
}

fn is_minute(n: i64) -> bool {
    (0..=59).contains(&n)
}

fn is_hour(n: i64) -> bool {
    (0..=23).contains(&n)
}

fn is_month(n: i64) -> bool {
    (1..=12).contains(&n)
}

fn is_day(n: i64) -> bool {
    (1..=31).contains(&n)
}

/// A fraction of a second as milliseconds: the first three digits, truncated.
fn read_milliseconds(token: DateToken) -> i64 {
    let DateToken::Number { value, length } = token else {
        return 0;
    };
    match length {
        1 => value * 100,
        2 => value * 10,
        3 => value,
        _ => {
            let digits = length.min(MAX_SIGNIFICANT_DIGITS);
            value / 10i64.pow((digits - 3) as u32)
        }
    }
}

#[derive(Default)]
struct DayComposer {
    components: Vec<i64>,
    named_month: Option<i64>,
    /// Set for an ECMAScript date: always year-month-day, no two-digit years.
    is_iso_date: bool,
}

impl DayComposer {
    fn add(&mut self, n: i64) -> bool {
        if self.components.len() < 3 {
            self.components.push(n);
            return true;
        }
        false
    }

    /// `(year, month 1..=12, day)`.
    fn write(&self) -> Option<(i64, i64, i64)> {
        if self.components.is_empty() {
            return None;
        }
        // Day and month default to 1. So does a missing third number, which
        // is how a date with no year lands in 2001.
        let mut components = [1i64; 3];
        for (slot, value) in components.iter_mut().zip(&self.components) {
            *slot = *value;
        }
        let [first, second, third] = components;
        let (mut year, month, day) = match self.named_month {
            None if self.is_iso_date || !is_day(first) => (first, second, third),
            // MD(Y)
            None => (third, first, second),
            // YMD, MYD, or YDM
            Some(month) if !is_day(first) => (first, month, second),
            // DMY, MDY, or DYM
            Some(month) => (second, month, first),
        };
        if !self.is_iso_date {
            if (0..=49).contains(&year) {
                year += 2000;
            } else if (50..=99).contains(&year) {
                year += 1900;
            }
        }
        (is_month(month) && is_day(day)).then_some((year, month, day))
    }
}

#[derive(Default)]
struct TimeComposer {
    components: Vec<i64>,
    hour_offset: Option<i64>,
}

impl TimeComposer {
    fn is_empty(&self) -> bool {
        self.components.is_empty()
    }

    fn is_expecting(&self, n: i64) -> bool {
        match self.components.len() {
            1 | 2 => (0..=59).contains(&n),
            3 => (0..=999).contains(&n),
            _ => false,
        }
    }

    fn add(&mut self, n: i64) -> bool {
        if self.components.len() < 4 {
            self.components.push(n);
            return true;
        }
        false
    }

    fn add_final(&mut self, n: i64) -> bool {
        if !self.add(n) {
            return false;
        }
        while self.add(0) {}
        true
    }

    /// Milliseconds into the day.
    fn write(&self) -> Option<i64> {
        let mut components = [0i64; 4];
        for (slot, value) in components.iter_mut().zip(&self.components) {
            *slot = *value;
        }
        let [mut hour, minute, second, millisecond] = components;
        if let Some(offset) = self.hour_offset {
            if !(0..=12).contains(&hour) {
                return None;
            }
            hour = hour % 12 + offset;
        }
        let in_range = is_hour(hour) && is_minute(minute) && is_minute(second) && (0..=999).contains(&millisecond);
        // A 24th hour is allowed if minutes, seconds and milliseconds are 0.
        if !in_range && (hour != 24 || minute != 0 || second != 0 || millisecond != 0) {
            return None;
        }
        Some(hour * 3_600_000 + minute * 60_000 + second * 1000 + millisecond)
    }
}

struct TimeZoneComposer {
    sign: Option<i64>,
    hour: i64,
    minute: i64,
}

impl TimeZoneComposer {
    fn set(&mut self, offset_in_hours: i64) {
        let sign = if offset_in_hours < 0 { -1 } else { 1 };
        self.sign = Some(sign);
        self.hour = offset_in_hours * sign;
        self.minute = 0;
    }

    fn is_empty(&self) -> bool {
        self.hour == NONE
    }

    fn is_expecting(&self, n: i64) -> bool {
        self.hour != NONE && self.minute == NONE && is_minute(n)
    }

    fn is_utc(&self) -> bool {
        self.hour == 0 && self.minute == 0
    }

    /// The offset from UTC in seconds, or `None` for local time. The sum is
    /// taken in 32 bits, as the original takes it: a huge hour wraps around,
    /// and a result past the positive half fails the parse.
    fn write(&self) -> Result<Option<i64>, ()> {
        let Some(sign) = self.sign else {
            return Ok(None);
        };
        let hour = if self.hour == NONE { 0 } else { self.hour };
        let minute = if self.minute == NONE { 0 } else { self.minute };
        let total_seconds = hour.wrapping_mul(3600).wrapping_add(minute.wrapping_mul(60)) as u32;
        if total_seconds > i32::MAX as u32 {
            return Err(());
        }
        Ok(Some(sign * total_seconds as i64))
    }
}

/// How far the ECMAScript date-time format got.
enum Es5 {
    /// The whole string was a date-time string.
    Complete,
    /// It stopped at this token; the legacy grammar takes over from it.
    Continue(DateToken),
    /// A time part that started and went wrong: the string is no date at all.
    Invalid,
}

/// `([+-]yyyyyy|yyyy)(-MM(-DD)?)?(THH:mm(:ss(.sss)?)?(Z|[+-]HH:mm|[+-]HHmm)?)?`
fn parse_es5_date_time(
    scanner: &mut DateScanner,
    day: &mut DayComposer,
    time: &mut TimeComposer,
    tz: &mut TimeZoneComposer,
) -> Es5 {
    if scanner.peek().is_ascii_sign() {
        // The sign goes back to the legacy grammar if it is not a year's.
        let sign_token = scanner.take();
        if !scanner.peek().is_fixed_number(6) {
            return Es5::Continue(sign_token);
        }
        let sign = if sign_token.is_symbol('-') { -1 } else { 1 };
        let year = scanner.take().number();
        if sign < 0 && year == 0 {
            return Es5::Continue(sign_token);
        }
        day.add(sign * year);
    } else if scanner.peek().is_fixed_number(4) {
        day.add(scanner.take().number());
    } else {
        return Es5::Continue(scanner.take());
    }
    if scanner.skip_symbol('-') {
        if !scanner.peek().is_fixed_number(2) || !is_month(scanner.peek().number()) {
            return Es5::Continue(scanner.take());
        }
        day.add(scanner.take().number());
        if scanner.skip_symbol('-') {
            if !scanner.peek().is_fixed_number(2) || !is_day(scanner.peek().number()) {
                return Es5::Continue(scanner.take());
            }
            day.add(scanner.take().number());
        }
    }
    // The optional time.
    if !matches!(scanner.peek(), DateToken::Keyword { kind: Keyword::TimeSeparator, .. }) {
        if scanner.peek() != DateToken::EndOfInput {
            return Es5::Continue(scanner.take());
        }
    } else {
        scanner.take();
        if !scanner.peek().is_fixed_number(2) || !(0..=24).contains(&scanner.peek().number()) {
            return Es5::Invalid;
        }
        // 24:00[:00[.000]] is allowed, and no other time starting with 24.
        let hour_is_24 = scanner.peek().number() == 24;
        time.add(scanner.take().number());
        if !scanner.skip_symbol(':') {
            return Es5::Invalid;
        }
        let fits = |token: DateToken| token.is_fixed_number(2) && is_minute(token.number()) && !(hour_is_24 && token.number() > 0);
        if !fits(scanner.peek()) {
            return Es5::Invalid;
        }
        time.add(scanner.take().number());
        if scanner.skip_symbol(':') {
            if !fits(scanner.peek()) {
                return Es5::Invalid;
            }
            time.add(scanner.take().number());
            if scanner.skip_symbol('.') {
                if !scanner.peek().is_number() || (hour_is_24 && scanner.peek().number() > 0) {
                    return Es5::Invalid;
                }
                // More or fewer than the mandated three digits are allowed.
                time.add(read_milliseconds(scanner.take()));
            }
        }
        // The optional zone: `Z`, `±hh:mm`, or `±hhmm`.
        if scanner.peek().is_keyword_z() {
            scanner.take();
            tz.set(0);
        } else if scanner.peek().is_ascii_sign() {
            tz.sign = Some(if scanner.take().is_symbol('+') { 1 } else { -1 });
            if scanner.peek().is_fixed_number(4) {
                let hour_minute = scanner.take().number();
                let (hour, minute) = (hour_minute / 100, hour_minute % 100);
                if !is_hour(hour) || !is_minute(minute) {
                    return Es5::Invalid;
                }
                tz.hour = hour;
                tz.minute = minute;
            } else {
                if !scanner.peek().is_fixed_number(2) || !is_hour(scanner.peek().number()) {
                    return Es5::Invalid;
                }
                tz.hour = scanner.take().number();
                if !scanner.skip_symbol(':') {
                    return Es5::Invalid;
                }
                if !scanner.peek().is_fixed_number(2) || !is_minute(scanner.peek().number()) {
                    return Es5::Invalid;
                }
                tz.minute = scanner.take().number();
            }
        }
        if scanner.peek() != DateToken::EndOfInput {
            return Es5::Invalid;
        }
    }
    // A date-only form is UTC; a date-time form with no offset is local time.
    if tz.is_empty() && time.is_empty() {
        tz.set(0);
    }
    day.is_iso_date = true;
    Es5::Complete
}

/// The string as `(year, month, day, milliseconds into the day, UTC offset
/// in seconds)`.
///
/// What the ECMAScript format leaves over is read leniently:
/// - Any unrecognized word before the first number is ignored, and
///   parenthesized text is ignored anywhere.
/// - A number followed by `:` is a time value; one followed by `.` after a
///   time has begun is seconds, and milliseconds must follow.
/// - Any other number is a date component. A month name is recorded as the
///   month, and AM/PM, `Z`, UTC, GMT and the US zone names as what they say.
/// - After the first number, an unknown word, an extra sign or an unmatched
///   `)` makes the string no date at all.
fn parse_date_fields(input: &str) -> Option<(i64, i64, i64, i64, Option<i64>)> {
    let mut scanner = DateScanner::new(input);
    let mut day = DayComposer::default();
    let mut time = TimeComposer::default();
    let mut tz = TimeZoneComposer { sign: None, hour: NONE, minute: NONE };

    let mut token = match parse_es5_date_time(&mut scanner, &mut day, &mut time, &mut tz) {
        Es5::Invalid => return None,
        Es5::Complete => DateToken::EndOfInput,
        Es5::Continue(token) => token,
    };
    let mut has_read_number = !day.components.is_empty();

    while token != DateToken::EndOfInput {
        match token {
            DateToken::Number { value: n, .. } => {
                has_read_number = true;
                if scanner.skip_symbol(':') {
                    if scanner.skip_symbol(':') {
                        // n + "::"
                        if !time.is_empty() {
                            return None;
                        }
                        time.add(n);
                        time.add(0);
                    } else {
                        // n + ":"
                        if !time.add(n) {
                            return None;
                        }
                        if scanner.peek().is_symbol('.') {
                            scanner.take();
                        }
                    }
                } else if scanner.skip_symbol('.') && time.is_expecting(n) {
                    time.add(n);
                    if !scanner.peek().is_number() {
                        return None;
                    }
                    let milliseconds = read_milliseconds(scanner.take());
                    time.add_final(milliseconds);
                } else if tz.is_expecting(n) {
                    tz.minute = n;
                } else if time.is_expecting(n) {
                    time.add_final(n);
                    // End, white space, "Z", "+" or "-" must follow a finished time.
                    let peek = scanner.peek();
                    if !(peek == DateToken::EndOfInput
                        || peek == DateToken::WhiteSpace
                        || peek.is_keyword_z()
                        || peek.is_ascii_sign())
                    {
                        return None;
                    }
                } else {
                    if !day.add(n) {
                        return None;
                    }
                    scanner.skip_symbol('-');
                }
            }
            DateToken::Keyword { kind, value, .. } => {
                if kind == Keyword::AmPm && !time.is_empty() {
                    time.hour_offset = Some(value);
                } else if kind == Keyword::MonthName {
                    day.named_month = Some(value);
                    scanner.skip_symbol('-');
                } else if kind == Keyword::TimeZoneName && has_read_number {
                    tz.set(value);
                } else {
                    // Garbage words are illegal if a number has been read,
                    // and the first number has to be separated from them.
                    if has_read_number || scanner.peek().is_number() {
                        return None;
                    }
                }
            }
            DateToken::Symbol(sign @ ('+' | '-')) if tz.is_utc() || !time.is_empty() => {
                // A UTC offset, only after UTC or a time.
                tz.sign = Some(if sign == '-' { -1 } else { 1 });
                // The following number may be missing.
                let (n, length) = match scanner.peek() {
                    DateToken::Number { value, length } => {
                        scanner.take();
                        (value, length)
                    }
                    _ => (0, 0),
                };
                has_read_number = true;
                if scanner.peek().is_symbol(':') {
                    tz.hour = n;
                    tz.minute = NONE;
                } else if length == 2 || length == 1 {
                    // Zones like GMT-8.
                    tz.hour = n;
                    tz.minute = 0;
                } else if length == 4 || length == 3 {
                    // The hhmm format.
                    tz.hour = n / 100;
                    tz.minute = n % 100;
                } else {
                    return None;
                }
            }
            // An extra sign or ")" is illegal if a number has been read.
            DateToken::Symbol('+' | '-' | ')') if has_read_number => return None,
            _ => {}
        }
        token = scanner.take();
    }

    let (year, month, day_of_month) = day.write()?;
    let time_in_day = time.write()?;
    let offset = tz.write().ok()?;
    Some((year, month, day_of_month, time_in_day, offset))
}

/// `Date.parse(input)` as Bun implements it, with UTC as the local time
/// zone: milliseconds since the epoch, or `None` for NaN.
pub fn date_parse(input: &str) -> Option<i64> {
    let (year, month, day, time_in_day, offset) = parse_date_fields(input)?;
    if year.abs() > 1_000_000 {
        return None;
    }
    let days = days_from_civil(year, month, 1) + day - 1;
    let local = days as f64 * MS_PER_DAY as f64 + time_in_day as f64;
    time_clip(local - offset.unwrap_or(0) as f64 * 1000.0)
}

// ---------------------------------------------------------------------------
// Message ids, headers and quoting
// ---------------------------------------------------------------------------

/// The Message-ID a COPY gives its destination row: stable for one
/// (source, destination) pair, so a retried COPY lands on the same row.
pub fn derive_copy_message_id(source_message_id: Option<&str>, dest_mailbox: &str) -> String {
    // A missing source Message-ID is a broken source row. Hashing an empty
    // seed would make every such copy collide on one derived id, so each
    // gets a random seed instead.
    let seed = match source_message_id {
        Some(id) => id.to_string(),
        None => hex::encode(rand::random::<[u8; 8]>()),
    };
    let digest = Sha256::digest(format!("{seed}\0{dest_mailbox}").as_bytes());
    format!("{}.copy@server", &hex::encode(digest)[..16])
}

/// Base64 of the UTF-8 bytes, unfolded.
pub fn encode_text(text: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
}

/// Replaces each run of the characters in `breaks` with one space.
fn collapse_runs(value: &str, breaks: &[char]) -> String {
    let mut out = String::with_capacity(value.len());
    let mut in_run = false;
    for c in value.chars() {
        if breaks.contains(&c) {
            if !in_run {
                out.push(' ');
            }
            in_run = true;
        } else {
            out.push(c);
            in_run = false;
        }
    }
    out
}

/// Backslash-escapes `"` and `\`.
fn escape_quoted_specials(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    for c in value.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A stored value made safe as a header field body: no stored value may
/// start a line, so every run of CR, LF, NUL, U+2028 and U+2029 becomes one
/// space.
pub fn header_field_value(value: &str) -> String {
    collapse_runs(value, &['\r', '\n', '\0', '\u{2028}', '\u{2029}'])
}

/// A stored value as a quoted header parameter.
pub fn header_quoted_param(value: &str) -> String {
    format!("\"{}\"", escape_quoted_specials(&header_field_value(value)))
}

/// The `type` and `subtype` halves of a stored `contentType`.
///
/// Each half falls back when it is missing or empty: `application` has no
/// subtype, `/pdf` an empty type, `application/` an empty subtype. A media
/// type missing either half is unparseable, so a client falls back to
/// `text/plain` and renders an attachment inline. BODYSTRUCTURE and the
/// emitted part headers describe the same part, so both derive it here.
pub fn media_type_parts(content_type: Option<&str>) -> (String, String) {
    let content_type = match content_type {
        Some(value) if !value.is_empty() => value,
        _ => "application/octet-stream",
    };
    let mut halves = content_type.split('/');
    let raw_type = halves.next().unwrap_or("");
    let raw_subtype = halves.next().unwrap_or("");
    (
        if raw_type.is_empty() { "application" } else { raw_type }.to_string(),
        if raw_subtype.is_empty() { "octet-stream" } else { raw_subtype }.to_string(),
    )
}

/// The `Content-Type` value and quoted `filename` parameter for one
/// attachment part header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentPartHeaderFields {
    pub content_type: String,
    pub filename_param: String,
}

fn attachment_filename(attachment: &MailAttachment) -> &str {
    match attachment.filename.as_deref() {
        Some(name) if !name.is_empty() => name,
        _ => "unnamed",
    }
}

pub fn attachment_part_header_fields(attachment: &MailAttachment) -> AttachmentPartHeaderFields {
    let (media_type, subtype) = media_type_parts(attachment.content_type.as_deref());
    AttachmentPartHeaderFields {
        content_type: header_field_value(&format!("{media_type}/{subtype}")),
        filename_param: header_quoted_param(attachment_filename(attachment)),
    }
}

/// The MIME multipart boundary token derived from a mail's stable id.
///
/// The id falls back to the Message-ID when there is no document id, which
/// puts a sender-controlled string inside `boundary="…"` and every delimiter.
/// RFC 2046 §5.1.1 admits far less than a Message-ID, so anything outside
/// `[A-Za-z0-9_.-]` maps to `_` — one `_` per UTF-16 code unit, as the Bun
/// server's regular expression replaces them.
pub fn boundary_token(stable_id: &str) -> String {
    let mut out = String::with_capacity(stable_id.len());
    for c in stable_id.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
            out.push(c);
        } else {
            for _ in 0..c.len_utf16() {
                out.push('_');
            }
        }
    }
    out
}

/// RFC 3501 §9 quoted string. `"` and `\` are backslash-escaped, and each run
/// of CR, LF and NUL collapses to one space: either one unescaped would make
/// the client lose the string boundary and mis-frame the rest of the response.
/// 8-bit characters pass through.
pub fn quote_string(value: &str) -> String {
    format!("\"{}\"", escape_quoted_specials(&collapse_runs(value, &['\r', '\n', '\0'])))
}

/// RFC 3501 §9 `nstring`: a quoted string, or the bare atom `NIL` when absent or empty.
pub fn format_n_string(value: Option<&str>) -> String {
    match value {
        Some(value) if !value.is_empty() => quote_string(value),
        _ => "NIL".to_string(),
    }
}

/// An ENVELOPE address list without its outer parentheses, or `NIL`.
pub fn format_address_list(value: Option<&[MailAddressValue]>) -> String {
    let Some(value) = value else {
        return "NIL".to_string();
    };
    let formatted: Vec<String> = value
        .iter()
        .filter_map(|entry| {
            let address = entry.address.as_deref().unwrap_or("");
            let mut halves = address.split('@');
            let local = halves.next().unwrap_or("");
            let domain = halves.next().unwrap_or("");
            if local.is_empty() || domain.is_empty() {
                return None;
            }
            // RFC 3501 §9: `addr-name = nstring`. A header with no display
            // name has no personal name at all: NIL, not `""`.
            let name = format_n_string(entry.name.as_deref());
            Some(format!("({name} NIL {} {})", quote_string(local), quote_string(domain)))
        })
        .collect();
    if formatted.is_empty() {
        "NIL".to_string()
    } else {
        formatted.join(" ")
    }
}

/// Whether a body column has content: the loaded string when there is one,
/// or its `octet_length()` when the row was projected without the string.
fn has_materialized_or_lazy_body(raw: Option<&str>, octets: Option<f64>) -> bool {
    raw.is_some_and(|text| !js_trim(text).is_empty()) || octets.is_some_and(|count| count > 0.0)
}

fn has_attachments(mail: &FetchMailInput) -> bool {
    mail.attachments.as_ref().is_some_and(|list| !list.is_empty())
}

fn non_empty(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|text| !text.is_empty())
}

/// The RFC 5322 header block of a stored mail, lines joined by CRLF with no
/// trailing CRLF.
pub fn format_headers(mail: &FetchMailInput, doc_id: Option<&str>) -> String {
    let mut headers: Vec<String> = Vec::new();

    if let Some(message_id) = non_empty(&mail.message_id) {
        headers.push(format!("Message-ID: {}", header_field_value(message_id)));
    }
    if let Some(date) = non_empty(&mail.date) {
        headers.push(format!("Date: {}", JsDate::parse(date).to_utc_string()));
    }
    let address_headers = [
        ("From", &mail.from),
        ("To", &mail.to),
        ("Cc", &mail.cc),
        ("Bcc", &mail.bcc),
        ("Reply-To", &mail.reply_to),
    ];
    for (name, address) in address_headers {
        if let Some(text) = address.as_ref().and_then(|a| non_empty(&a.text)) {
            headers.push(format!("{name}: {}", header_field_value(text)));
        }
    }
    if let Some(subject) = non_empty(&mail.subject) {
        headers.push(format!("Subject: {}", header_field_value(subject)));
    }

    headers.push("MIME-Version: 1.0".to_string());

    let has_text = has_materialized_or_lazy_body(mail.text.as_deref(), mail.text_octets);
    let has_html = has_materialized_or_lazy_body(mail.html.as_deref(), mail.html_octets);

    // The boundary is stable: derived from the document id, which should always exist.
    let doc_id = doc_id.filter(|id| !id.is_empty());
    if doc_id.is_none() {
        logger::warn(
            "docId is missing, falling back to messageId",
            json!({ "component": "imap", "messageId": mail.message_id }),
        );
    }
    let stable_id = doc_id.or(non_empty(&mail.message_id)).unwrap_or("default");

    if has_attachments(mail) {
        let boundary = format!("boundary_{}", boundary_token(stable_id));
        headers.push(format!("Content-Type: multipart/mixed; boundary=\"{boundary}\""));
    } else if has_text && has_html {
        let boundary = format!("boundary_{}", boundary_token(stable_id));
        headers.push(format!("Content-Type: multipart/alternative; boundary=\"{boundary}\""));
    } else if has_html {
        headers.push("Content-Type: text/html; charset=utf-8".to_string());
        headers.push("Content-Transfer-Encoding: base64".to_string());
    } else {
        headers.push("Content-Type: text/plain; charset=utf-8".to_string());
        headers.push("Content-Transfer-Encoding: base64".to_string());
    }

    headers.join("\r\n")
}

/// The ENVELOPE of a stored mail (RFC 3501 §7.4.2).
pub fn format_envelope(mail: &FetchMailInput) -> String {
    let date = match non_empty(&mail.date) {
        Some(date) => format!("\"{}\"", JsDate::parse(date).to_utc_string()),
        None => "NIL".to_string(),
    };
    let subject = format_n_string(mail.subject.as_deref());
    let list = |address: &Option<crate::imap::types::MailAddress>| {
        format_address_list(address.as_ref().and_then(|a| a.value.as_deref()))
    };
    let from = list(&mail.from);
    // The sender is usually the same as From.
    let sender = from.clone();
    let reply_to = match &mail.reply_to {
        Some(_) => list(&mail.reply_to),
        None => "NIL".to_string(),
    };
    let to = list(&mail.to);
    let cc = list(&mail.cc);
    let bcc = list(&mail.bcc);
    let in_reply_to = "NIL";
    let message_id = format_n_string(mail.message_id.as_deref());

    // RFC 3501 §7.4.2 / §9: an address-list member is the bare atom NIL or a
    // parenthesized `1*address`, never `(NIL)`.
    let address_list = |list: &str| if list == "NIL" { "NIL".to_string() } else { format!("({list})") };

    format!(
        "({date} {subject} {} {} {} {} {} {} {in_reply_to} {message_id})",
        address_list(&from),
        address_list(&sender),
        address_list(&reply_to),
        address_list(&to),
        address_list(&cc),
        address_list(&bcc),
    )
}

/// `Math.ceil(raw / 3) * 4`: the length of unfolded base64 of `raw` octets.
fn base64_len(raw: f64) -> f64 {
    (raw / 3.0).ceil() * 4.0
}

/// BODYSTRUCTURE of a stored mail (RFC 3501 §7.4.2).
///
/// `extensible: false` is the bare `BODY` data item (§6.4.5): it drops the
/// extension data, md5/disposition/language/location on single parts and
/// param-list/disposition/language/location on multiparts.
///
/// A text part's size comes from `text_octets` / `html_octets` when the
/// caller projected them, so a bare `FETCH BODYSTRUCTURE` loads no body, and
/// from the loaded string otherwise. Every body this server serves is base64
/// with no line folding, so `body-fld-lines` is one, or zero for no bytes.
pub fn format_body_structure(mail: &FetchMailInput, extensible: bool) -> String {
    let encoded_line_count = |size: f64| if size == 0.0 { 0.0 } else { 1.0 };

    let build_text_part = |subtype: &str, content: Option<&str>, octets: Option<f64>| -> String {
        let size = match octets {
            Some(octets) => base64_len(octets),
            None => base64_len(content.unwrap_or("").len() as f64),
        };
        // RFC 3501 §9: media type, subtype and encoding are strings, not atoms.
        let mut parts = vec![
            "\"TEXT\"".to_string(),
            format!("\"{}\"", subtype.to_uppercase()),
            "(\"CHARSET\" \"UTF-8\")".to_string(),
            "NIL".to_string(),
            "NIL".to_string(),
            "\"BASE64\"".to_string(),
            js_number_string(size),
            js_number_string(encoded_line_count(size)),
        ];
        if extensible {
            // MD5, disposition, language, location.
            parts.extend(["NIL".to_string(), "NIL".to_string(), "NIL".to_string(), "NIL".to_string()]);
        }
        format!("({})", parts.join(" "))
    };
    let text_part = || build_text_part("plain", mail.text.as_deref(), mail.text_octets);
    let html_part = || build_text_part("html", mail.html.as_deref(), mail.html_octets);

    let build_attachment_part = |attachment: &MailAttachment| -> String {
        let (media_type, subtype) = media_type_parts(attachment.content_type.as_deref());
        let filename = attachment_filename(attachment);
        // The base64 length, without encoding anything.
        let size = match attachment.size {
            Some(size) if size != 0.0 && !size.is_nan() => base64_len(size),
            _ => 0.0,
        };
        let mut parts = vec![
            quote_string(&media_type),
            quote_string(&subtype),
            format!("({} {})", quote_string("NAME"), quote_string(filename)),
            "NIL".to_string(),
            "NIL".to_string(),
            "\"BASE64\"".to_string(),
            js_number_string(size),
        ];
        // RFC 3501 §9: a text/* part carries one field more than a basic part.
        if media_type.to_lowercase() == "text" {
            parts.push(js_number_string(encoded_line_count(size)));
        }
        if extensible {
            parts.push("NIL".to_string());
            parts.push(format!(
                "({} ({} {}))",
                quote_string("ATTACHMENT"),
                quote_string("FILENAME"),
                quote_string(filename)
            ));
            parts.push("NIL".to_string());
            parts.push("NIL".to_string());
        }
        format!("({})", parts.join(" "))
    };

    // Multipart tail: the subtype alone, or the subtype plus the extension data.
    let multipart_tail = |subtype: &str| {
        if extensible {
            format!("\"{subtype}\" NIL NIL NIL NIL")
        } else {
            format!("\"{subtype}\"")
        }
    };

    // The same has-body decision `format_headers` makes: the two must take
    // the same branch or the response contradicts itself.
    let has_text = has_materialized_or_lazy_body(mail.text.as_deref(), mail.text_octets);
    let has_html = has_materialized_or_lazy_body(mail.html.as_deref(), mail.html_octets);
    let has_attachments = has_attachments(mail);

    if has_text && !has_html && !has_attachments {
        return text_part();
    }
    if !has_text && has_html && !has_attachments {
        return html_part();
    }
    // RFC 3501 §9: `body-type-mpart = 1*body SP media-subtype`. Sibling parts
    // concatenate with no separator.
    if has_text && has_html && !has_attachments {
        return format!("({}{} {})", text_part(), html_part(), multipart_tail("alternative"));
    }
    if has_attachments {
        let mut body_parts: Vec<String> = Vec::new();
        if has_text && has_html {
            body_parts.push(format!("({}{} {})", text_part(), html_part(), multipart_tail("alternative")));
        } else if has_text {
            body_parts.push(text_part());
        } else if has_html {
            body_parts.push(html_part());
        }
        for attachment in mail.attachments.iter().flatten() {
            body_parts.push(build_attachment_part(attachment));
        }
        return format!("({} {})", body_parts.join(""), multipart_tail("mixed"));
    }

    // No content at all: an empty text part.
    build_text_part("plain", Some(""), None)
}

/// The system flags a stored mail carries.
pub fn format_flags(mail: &FetchMailInput) -> Vec<String> {
    let mut flags = Vec::new();
    let mut add = |set: Option<bool>, flag: &str| {
        if set == Some(true) {
            flags.push(flag.to_string());
        }
    };
    add(mail.read, "\\Seen");
    add(mail.saved, "\\Flagged");
    add(mail.deleted, "\\Deleted");
    add(mail.draft, "\\Draft");
    add(mail.answered, "\\Answered");
    flags
}

// ---------------------------------------------------------------------------
// Mailbox names
// ---------------------------------------------------------------------------

pub const ACCOUNTS_FOLDER: &str = "INBOX/accounts";
pub const SENT_MESSAGES_FOLDER: &str = "Sent Messages";
pub const SENT_MESSAGES_ACCOUNTS_FOLDER: &str = "Sent Messages/accounts";

/// The flags a mail must carry to be in a utility folder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UtilityPlacement {
    pub draft: Option<bool>,
    pub is_spam: Option<bool>,
    pub saved: Option<bool>,
    pub deleted: Option<bool>,
}

/// Which UID space a utility folder reads: `mails.uid_domain`, or a pivot row
/// per membership like the per-account boxes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UtilityUidSpace {
    Domain,
    Mapped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UtilityFolder {
    pub name: &'static str,
    pub special_use: &'static str,
    pub placement: UtilityPlacement,
    pub uid_space: UtilityUidSpace,
}

const NO_PLACEMENT: UtilityPlacement = UtilityPlacement { draft: None, is_spam: None, saved: None, deleted: None };

pub const UTILITY_FOLDERS: [UtilityFolder; 4] = [
    UtilityFolder {
        name: "Drafts",
        special_use: "\\Drafts",
        placement: UtilityPlacement { draft: Some(true), ..NO_PLACEMENT },
        uid_space: UtilityUidSpace::Domain,
    },
    UtilityFolder {
        name: "Junk",
        special_use: "\\Junk",
        placement: UtilityPlacement { is_spam: Some(true), ..NO_PLACEMENT },
        uid_space: UtilityUidSpace::Domain,
    },
    UtilityFolder {
        name: "Starred",
        special_use: "\\Flagged",
        placement: UtilityPlacement { saved: Some(true), ..NO_PLACEMENT },
        uid_space: UtilityUidSpace::Mapped,
    },
    UtilityFolder {
        name: "Trash",
        special_use: "\\Trash",
        placement: UtilityPlacement { deleted: Some(true), ..NO_PLACEMENT },
        uid_space: UtilityUidSpace::Mapped,
    },
];

/// The utility folder `mailbox` names, in any casing, or `None` for any other box.
pub fn utility_folder(mailbox: &str) -> Option<&'static UtilityFolder> {
    let wanted = mailbox.to_lowercase();
    UTILITY_FOLDERS.iter().find(|folder| folder.name.to_lowercase() == wanted)
}

/// True for a server-defined utility mailbox (`Drafts`, `Junk`, `Starred`, `Trash`).
pub fn is_utility_folder(mailbox: &str) -> bool {
    utility_folder(mailbox).is_some()
}

/// True for a utility folder whose UID space is the mapped one: `Starred` and `Trash`.
pub fn is_mapped_utility_folder(mailbox: &str) -> bool {
    utility_folder(mailbox).is_some_and(|folder| folder.uid_space == UtilityUidSpace::Mapped)
}

/// The flags a mail must carry to land in `mailbox`, or `None` for any other box.
pub fn utility_placement(mailbox: &str) -> Option<UtilityPlacement> {
    utility_folder(mailbox).map(|folder| folder.placement)
}

/// The canonical spelling of a server-defined mailbox name, unchanged for any
/// other box. Every entry point that takes a mailbox name off the wire runs
/// it through here.
pub fn canonical_mailbox(mailbox: &str) -> String {
    if is_inbox(mailbox) {
        return "INBOX".to_string();
    }
    match utility_folder(mailbox) {
        Some(folder) => folder.name.to_string(),
        None => mailbox.to_string(),
    }
}

fn local_part(account_name: &str) -> &str {
    account_name.split('@').next().unwrap_or("")
}

/// Maps an email address to its received-mail virtual mailbox name.
pub fn account_to_box(account_name: &str) -> String {
    format!("{ACCOUNTS_FOLDER}/{}", local_part(account_name))
}

/// Maps an email address to its sent-mail virtual mailbox name.
pub fn account_to_sent_box(account_name: &str) -> String {
    format!("{SENT_MESSAGES_ACCOUNTS_FOLDER}/{}", local_part(account_name))
}

/// True for any sent mailbox: `Sent Messages` or `Sent Messages/accounts/{name}`.
pub fn is_sent_box(mailbox: &str) -> bool {
    mailbox == SENT_MESSAGES_FOLDER || mailbox.starts_with(&format!("{SENT_MESSAGES_ACCOUNTS_FOLDER}/"))
}

/// True for the special INBOX mailbox name, which RFC 3501 §5.1 matches
/// case-insensitively. All other mailbox names are case-sensitive.
pub fn is_inbox(mailbox: &str) -> bool {
    mailbox.to_uppercase() == "INBOX"
}

pub fn is_domain_scoped(mailbox: &str) -> bool {
    if is_inbox(mailbox) || mailbox == SENT_MESSAGES_FOLDER {
        return true;
    }
    utility_folder(mailbox).is_some_and(|folder| folder.uid_space == UtilityUidSpace::Domain)
}

/// True for the accounts/ parent folder itself (non-selectable).
pub fn is_accounts_folder(mailbox: &str) -> bool {
    mailbox == ACCOUNTS_FOLDER
}

/// True for the Sent Messages/accounts/ parent folder itself (non-selectable).
pub fn is_sent_messages_accounts_folder(mailbox: &str) -> bool {
    mailbox == SENT_MESSAGES_ACCOUNTS_FOLDER
}

/// The address an account folder names. Folders are named by local part, so
/// the domain is the session's primary one: the domain a scoped login names,
/// or the user's default domain.
pub fn box_to_account(domain: &str, mailbox: &str) -> String {
    let sent_prefix = format!("{SENT_MESSAGES_ACCOUNTS_FOLDER}/");
    let accounts_prefix = format!("{ACCOUNTS_FOLDER}/");
    let local = mailbox
        .strip_prefix(&sent_prefix)
        .or_else(|| mailbox.strip_prefix(&accounts_prefix))
        .unwrap_or(mailbox);
    format!("{local}@{domain}")
}

/// INTERNALDATE (RFC 3501 §9 `date-time`): `DD-Mon-YYYY HH:MM:SS +ZZZZ`, the
/// day space-padded. An Invalid Date prints what the Bun server prints.
pub fn format_internal_date(date: &JsDate) -> String {
    let Some(DateFields { year, month, day, hour, minute, second, .. }) = date.fields() else {
        return "NaN-undefined-NaN NaN:NaN:NaN -NaNNaN".to_string();
    };
    format!(
        "{day:>2}-{}-{year} {hour:02}:{minute:02}:{second:02} +0000",
        MONTH_NAMES[(month - 1) as usize]
    )
}
