//! Plain-English schedules for zoomies ("every 30 min", "mondays 10am",
//! "daily 21:00") parsed into the engine's [`AgentTrigger`]s, plus the live
//! description the composer's When row shows under the field.
//!
//! Pure: the caller supplies the IANA zone and the clock. Every parsed
//! trigger is checked with the engine's own validation
//! ([`zeron_engine::agents::preview_next_fire`]), so a schedule that reads
//! fine here is one `CreateAgent` accepts.

use chrono::{
    DateTime, Datelike as _, Duration, NaiveDate, NaiveTime, TimeZone as _, Timelike as _, Utc,
    Weekday,
};
use chrono_tz::Tz;
use zeron_proto::agents::AgentTrigger;

/// Shown when nothing matched.
pub const GENERIC_HINT: &str =
    "Couldn't read that. Try \"every 30 min\", \"weekdays at 6pm\" or \"daily 21:00\".";
/// Shown for an empty field.
pub const EMPTY_HINT: &str = "Say when it runs, like \"every morning at 9\".";

/// Default time of day when a schedule names days but no time.
const DEFAULT_TIME: (u32, u32) = (9, 0);

/// A schedule the user typed (or picked), ready to save.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedSchedule {
    pub trigger: AgentTrigger,
    /// Lowercase description that reads after "runs": "every weekday at 18:00".
    pub phrase: String,
    /// The zone a clock schedule runs in (cron and one-shots).
    pub zone: Option<String>,
    /// The first scheduled fire after the parse clock; `None` for event
    /// triggers.
    pub next: Option<DateTime<Utc>>,
}

impl ParsedSchedule {
    /// "Runs every weekday at 18:00 IST — next: tomorrow 18:00".
    pub fn summary(&self, now: DateTime<Utc>) -> String {
        let mut text = format!("Runs {}", self.phrase);
        let tz = self.zone.as_deref().and_then(find_zone);
        if let (Some(tz), Some(next)) = (tz, self.next) {
            text.push(' ');
            text.push_str(&next.with_timezone(&tz).format("%Z").to_string());
        }
        if matches!(self.trigger, AgentTrigger::Webhook) {
            text.push_str(" (its URL is in Settings → Zoomies)");
        }
        if let Some(next) = self.next {
            text.push_str(" — next: ");
            text.push_str(&next_label(next, now, tz.unwrap_or(Tz::UTC)));
        }
        text
    }
}

/// The When row's one-click choices. Each fills the free-text field with a
/// phrase the parser reads, so the field stays the single source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    EveryMorning,
    EveryHour,
    WeekdayEvenings,
    FilesChange,
    Webhook,
    Once,
}

impl Preset {
    pub const ALL: [Preset; 6] = [
        Self::EveryMorning,
        Self::EveryHour,
        Self::WeekdayEvenings,
        Self::FilesChange,
        Self::Webhook,
        Self::Once,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::EveryMorning => "Every morning at 9",
            Self::EveryHour => "Every hour",
            Self::WeekdayEvenings => "Weekdays at 6pm",
            Self::FilesChange => "When files change in this project",
            Self::Webhook => "When a webhook fires",
            Self::Once => "Once at…",
        }
    }

    /// What the preset writes into the field. "Once at…" leaves the time
    /// for the user to finish.
    pub fn text(self) -> &'static str {
        match self {
            Self::EveryMorning => "every morning at 9",
            Self::EveryHour => "every hour",
            Self::WeekdayEvenings => "weekdays at 6pm",
            Self::FilesChange => "when files change in this project",
            Self::Webhook => "when a webhook fires",
            Self::Once => "once at ",
        }
    }

    /// The preset whose phrase the field currently holds (highlights it).
    pub fn matching(text: &str) -> Option<Preset> {
        let text = text.trim().to_lowercase();
        Self::ALL
            .into_iter()
            .find(|preset| preset.text().trim() == text)
    }
}

/// Parse `input` in `local_tz` (an IANA name; an unknown one falls back to
/// UTC). `Err` is a short hint the composer shows inline.
pub fn parse_schedule(
    input: &str,
    local_tz: &str,
    now: DateTime<Utc>,
) -> Result<ParsedSchedule, String> {
    let text = normalize(input);
    if text.is_empty() {
        return Err(EMPTY_HINT.into());
    }
    let mut toks = Toks::new(&text);
    let zone_name = toks
        .take_zone()
        .or_else(|| find_zone(local_tz).map(|tz| tz.name().to_string()))
        .unwrap_or_else(|| "UTC".into());
    let tz = find_zone(&zone_name).unwrap_or(Tz::UTC);
    toks.drop_filler();
    if toks.remaining() == 0 {
        return Err(EMPTY_HINT.into());
    }

    let kind = if let Some(parsed) = webhook(&toks) {
        parsed
    } else if let Some(parsed) = file_watch(&toks) {
        parsed
    } else if let Some(parsed) = raw_cron(&toks, &zone_name) {
        parsed
    } else if let Some(parsed) = interval(&mut toks.clone(), &zone_name)? {
        parsed
    } else {
        // A one-shot cue that doesn't read as one may still be a calendar
        // schedule ("weekly on friday at 4pm"); its own hint wins otherwise.
        match once(&mut toks.clone(), tz, now) {
            Ok(Some(parsed)) => parsed,
            once_result => match recurring(&mut toks.clone(), &zone_name) {
                Ok(Some(parsed)) => parsed,
                recurring_result => {
                    once_result?;
                    recurring_result?;
                    return Err(GENERIC_HINT.into());
                }
            },
        }
    };
    finish(kind, now)
}

/// A trigger plus its phrase, before the engine check.
struct Kind {
    trigger: AgentTrigger,
    phrase: String,
    zone: Option<String>,
}

fn finish(kind: Kind, now: DateTime<Utc>) -> Result<ParsedSchedule, String> {
    let next = zeron_engine::agents::preview_next_fire(&kind.trigger, now).map_err(|err| {
        if err.contains("at least") {
            "Zoomies run at most once a minute.".to_string()
        } else {
            err
        }
    })?;
    if matches!(kind.trigger, AgentTrigger::Once { .. }) && next.is_none() {
        return Err("That time has already passed.".into());
    }
    Ok(ParsedSchedule {
        trigger: kind.trigger,
        phrase: kind.phrase,
        zone: kind.zone,
        next,
    })
}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

fn normalize(input: &str) -> String {
    let mut text = input.trim().to_lowercase();
    for (from, to) in [
        ("a.m.", "am"),
        ("p.m.", "pm"),
        ("o'clock", ""),
        ("o’clock", ""),
        ("’", "'"),
        (",", " , "),
        ("&", " and "),
        ("+", " and "),
    ] {
        text = text.replace(from, to);
    }
    let text = text.trim_end_matches(['.', '!', '?']);
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Words that never change the meaning of a schedule.
const FILLER: &[&str] = &[
    "run",
    "runs",
    "running",
    "it",
    "this",
    "please",
    "should",
    "start",
    "starting",
    "trigger",
    "triggered",
    "fire",
    ",",
    "and",
    "the",
    "around",
    "about",
    "exactly",
    "sharp",
    "o",
];

#[derive(Clone)]
struct Toks {
    items: Vec<String>,
    used: Vec<bool>,
}

impl Toks {
    fn new(text: &str) -> Self {
        let items: Vec<String> = text.split_whitespace().map(str::to_string).collect();
        let used = vec![false; items.len()];
        Self { items, used }
    }

    fn remaining(&self) -> usize {
        self.used.iter().filter(|used| !**used).count()
    }

    fn free(&self) -> impl Iterator<Item = (usize, &str)> {
        self.items
            .iter()
            .enumerate()
            .filter(|(ix, _)| !self.used[*ix])
            .map(|(ix, item)| (ix, item.as_str()))
    }

    fn words(&self) -> Vec<&str> {
        self.free().map(|(_, word)| word).collect()
    }

    fn has(&self, word: &str) -> bool {
        self.free().any(|(_, w)| w == word)
    }

    fn has_any(&self, words: &[&str]) -> bool {
        self.free().any(|(_, w)| words.contains(&w))
    }

    fn take(&mut self, word: &str) -> bool {
        let found = self.free().find(|(_, w)| *w == word).map(|(ix, _)| ix);
        if let Some(ix) = found {
            self.used[ix] = true;
        }
        found.is_some()
    }

    fn take_all(&mut self, words: &[&str]) {
        for ix in 0..self.items.len() {
            if !self.used[ix] && words.contains(&self.items[ix].as_str()) {
                self.used[ix] = true;
            }
        }
    }

    /// The next unused token after `ix`.
    fn next_free(&self, ix: usize) -> Option<(usize, &str)> {
        self.free().find(|(at, _)| *at > ix)
    }

    /// The previous unused token before `ix`.
    fn prev_free(&self, ix: usize) -> Option<(usize, &str)> {
        self.free().filter(|(at, _)| *at < ix).last()
    }

    fn drop_filler(&mut self) {
        self.take_all(FILLER);
    }

    /// Pull a time zone ("utc", "ist", "in Europe/Berlin") out of the text.
    fn take_zone(&mut self) -> Option<String> {
        let found = self.free().find_map(|(ix, word)| {
            zone_abbreviation(word)
                .map(str::to_string)
                .or_else(|| {
                    word.contains('/')
                        .then(|| find_zone(word))
                        .flatten()
                        .map(|tz| tz.name().to_string())
                })
                .map(|zone| (ix, zone))
        });
        let (ix, zone) = found?;
        self.used[ix] = true;
        if let Some((prev, "in" | "at")) = self.prev_free(ix) {
            self.used[prev] = true;
        }
        Some(zone)
    }

    /// Consumed everything meaningful?
    fn done(&self) -> bool {
        self.remaining() == 0
    }
}

/// An IANA zone by name, case-insensitively.
pub fn find_zone(name: &str) -> Option<Tz> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    name.parse::<Tz>().ok().or_else(|| {
        chrono_tz::TZ_VARIANTS
            .iter()
            .copied()
            .find(|tz| tz.name().eq_ignore_ascii_case(name))
    })
}

/// Common abbreviations people type, mapped to one representative zone.
fn zone_abbreviation(word: &str) -> Option<&'static str> {
    Some(match word {
        "utc" | "gmt" => "UTC",
        "ist" => "Asia/Kolkata",
        "pst" | "pdt" => "America/Los_Angeles",
        "mst" | "mdt" => "America/Denver",
        "cst" | "cdt" => "America/Chicago",
        "est" | "edt" => "America/New_York",
        "bst" => "Europe/London",
        "cet" | "cest" => "Europe/Berlin",
        "eet" | "eest" => "Europe/Athens",
        "jst" => "Asia/Tokyo",
        "sgt" => "Asia/Singapore",
        "aest" | "aedt" => "Australia/Sydney",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Event triggers
// ---------------------------------------------------------------------------

fn webhook(toks: &Toks) -> Option<Kind> {
    let words = toks.words();
    let hit = words.iter().any(|w| w.starts_with("webhook"))
        || words
            .windows(2)
            .any(|pair| pair == ["web", "hook"] || pair == ["web", "hooks"]);
    hit.then(|| Kind {
        trigger: AgentTrigger::Webhook,
        phrase: "when its webhook is called".into(),
        zone: None,
    })
}

const CHANGE_WORDS: &[&str] = &[
    "change", "changes", "changed", "save", "saved", "saves", "edit", "edits", "edited",
    "modified", "modify", "touched",
];

fn file_watch(toks: &Toks) -> Option<Kind> {
    let words = toks.words();
    let changes = words.iter().any(|w| CHANGE_WORDS.contains(w));
    let about_files = words.iter().any(|w| {
        matches!(
            *w,
            "file"
                | "files"
                | "folder"
                | "folders"
                | "code"
                | "anything"
                | "something"
                | "save"
                | "saved"
                | "directory"
        )
    }) || words.first().is_some_and(|w| *w == "on") && changes;
    if !(changes && about_files) {
        return None;
    }
    // Paths follow "in"/"under"/"inside": "when files change in src and docs".
    let mut paths = Vec::new();
    if let Some(at) = words
        .iter()
        .position(|w| matches!(*w, "in" | "under" | "inside" | "within"))
    {
        for word in &words[at + 1..] {
            if matches!(*word, "this" | "the" | "my" | "our" | "and" | "or" | ",") {
                continue;
            }
            if matches!(
                *word,
                "project" | "workspace" | "repo" | "repository" | "folder" | "codebase" | "here"
            ) {
                break;
            }
            paths.push(
                word.trim_matches(['"', '\'', '`'])
                    .trim_end_matches('/')
                    .to_string(),
            );
        }
    }
    paths.retain(|path| !path.is_empty() && path != "." && path != "./");
    let phrase = if paths.is_empty() {
        "whenever files change in this project".to_string()
    } else {
        format!("whenever files change in {}", list_phrase(&paths))
    };
    Some(Kind {
        trigger: AgentTrigger::FileWatch {
            paths,
            debounce_ms: None,
        },
        phrase,
        zone: None,
    })
}

/// A literal five-field cron expression.
fn raw_cron(toks: &Toks, zone: &str) -> Option<Kind> {
    let words = toks.words();
    let fields: Vec<&str> = match words.as_slice() {
        ["cron", rest @ ..] => rest.to_vec(),
        all => all.to_vec(),
    };
    let cronish = |field: &&str| {
        !field.is_empty()
            && field
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '*' | '/' | ',' | '-' | '?'))
    };
    if fields.len() != 5 || !fields.iter().all(cronish) {
        return None;
    }
    // Five bare numbers could just as well be a mistake; require cron syntax
    // or the "cron" keyword.
    if words[0] != "cron" && !fields.iter().any(|f| f.contains(['*', '/', ',', '-'])) {
        return None;
    }
    let expr = fields.join(" ");
    Some(Kind {
        phrase: format!("on the cron schedule {expr}"),
        trigger: AgentTrigger::Cron {
            expr,
            timezone: zone.into(),
        },
        zone: Some(zone.into()),
    })
}

// ---------------------------------------------------------------------------
// Durations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Second,
    Minute,
    Hour,
    Day,
    Week,
}

impl Unit {
    fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "s" | "sec" | "secs" | "second" | "seconds" => Self::Second,
            "m" | "min" | "mins" | "minute" | "minutes" | "minits" => Self::Minute,
            "h" | "hr" | "hrs" | "hour" | "hours" => Self::Hour,
            "d" | "day" | "days" => Self::Day,
            "w" | "wk" | "wks" | "week" | "weeks" => Self::Week,
            _ => return None,
        })
    }

    fn secs(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3600,
            Self::Day => 86_400,
            Self::Week => 604_800,
        }
    }
}

fn number_word(word: &str) -> Option<u64> {
    Some(match word {
        "a" | "an" | "one" => 1,
        "two" => 2,
        "three" => 3,
        "four" => 4,
        "five" => 5,
        "six" => 6,
        "seven" => 7,
        "eight" => 8,
        "nine" => 9,
        "ten" => 10,
        "eleven" => 11,
        "twelve" => 12,
        "fifteen" => 15,
        "twenty" => 20,
        "thirty" => 30,
        "forty" | "fourty" => 40,
        "fortyfive" | "forty-five" => 45,
        "sixty" => 60,
        "ninety" => 90,
        _ => return word.parse().ok(),
    })
}

/// "30m", "2h", "1h30m", "90min" → seconds.
fn compact_duration(word: &str) -> Option<u64> {
    let mut total = 0u64;
    let mut digits = String::new();
    let mut rest = word;
    let mut any = false;
    while !rest.is_empty() {
        let split = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let (number, tail) = rest.split_at(split);
        if number.is_empty() {
            return None;
        }
        digits.clear();
        digits.push_str(number);
        let unit_len = tail
            .find(|c: char| c.is_ascii_digit())
            .unwrap_or(tail.len());
        let (unit, tail) = tail.split_at(unit_len);
        let unit = Unit::parse(unit)?;
        total = total.checked_add(digits.parse::<u64>().ok()?.checked_mul(unit.secs())?)?;
        any = true;
        rest = tail;
    }
    any.then_some(total)
}

/// A duration starting at token `ix`: "30 min", "half an hour", "2h",
/// "hour". Returns seconds, the unit named last, and the indexes it used.
fn duration_at(toks: &Toks, ix: usize) -> Option<(u64, Unit, Vec<usize>)> {
    let word = toks.items.get(ix)?.as_str();
    if toks.used[ix] {
        return None;
    }
    if let Some(unit) = Unit::parse(word).filter(|_| word.len() > 1) {
        return Some((unit.secs(), unit, vec![ix]));
    }
    if let Some(secs) = compact_duration(word) {
        let unit = Unit::parse(word.trim_start_matches(|c: char| c.is_ascii_digit()))
            .unwrap_or(Unit::Minute);
        return Some((secs, unit, vec![ix]));
    }
    if matches!(word, "half" | "quarter") {
        // "half hour", "half an hour", "quarter of an hour"
        let mut used = vec![ix];
        let mut at = ix;
        while let Some((next, w)) = toks.next_free(at) {
            used.push(next);
            at = next;
            if matches!(w, "an" | "a" | "of") {
                continue;
            }
            if matches!(w, "hour" | "hr") {
                let secs = if word == "half" { 1800 } else { 900 };
                return Some((secs, Unit::Minute, used));
            }
            return None;
        }
        return None;
    }
    let count = number_word(word)?;
    let (unit_ix, unit_word) = toks.next_free(ix)?;
    let unit = Unit::parse(unit_word)?;
    let mut secs = count.checked_mul(unit.secs())?;
    let mut used = vec![ix, unit_ix];
    // "1 hour 30 min", "an hour and a half"
    if let Some((more_ix, more)) = toks.next_free(unit_ix) {
        if unit == Unit::Hour
            && let Some(minutes) = number_word(more)
            && let Some((min_ix, min_word)) = toks.next_free(more_ix)
            && Unit::parse(min_word) == Some(Unit::Minute)
        {
            secs += minutes * 60;
            used.extend([more_ix, min_ix]);
        }
    }
    Some((secs, unit, used))
}

fn duration_phrase(secs: u64) -> String {
    let plural = |n: u64, unit: &str| {
        if n == 1 {
            unit.to_string()
        } else {
            format!("{n} {unit}s")
        }
    };
    if secs % 604_800 == 0 {
        plural(secs / 604_800, "week")
    } else if secs % 86_400 == 0 {
        plural(secs / 86_400, "day")
    } else if secs % 3600 == 0 {
        plural(secs / 3600, "hour")
    } else if secs % 60 == 0 {
        plural(secs / 60, "minute")
    } else {
        plural(secs, "second")
    }
}

/// "every 30 min", "every hour", "hourly", "every 2 days" and, with days,
/// "every 15 minutes on weekdays" (a cron step).
fn interval(toks: &mut Toks, zone: &str) -> Result<Option<Kind>, String> {
    let words = toks.words();
    let hourly = words.iter().position(|w| *w == "hourly");
    let every_ix = toks
        .free()
        .find(|(_, w)| matches!(*w, "every" | "each" | "per"))
        .map(|(ix, _)| ix);
    let (secs, unit) = if hourly.is_some() {
        toks.take("hourly");
        (3600, Unit::Hour)
    } else if let Some(every) = every_ix {
        let Some((next_ix, next)) = toks.next_free(every) else {
            return Ok(None);
        };
        let other = next == "other";
        let start = if other {
            match toks.next_free(next_ix) {
                Some((ix, _)) => ix,
                None => return Ok(None),
            }
        } else {
            next_ix
        };
        let Some((secs, unit, used)) = duration_at(toks, start) else {
            return Ok(None);
        };
        // "every day" / "every week" read as calendar schedules.
        if matches!(unit, Unit::Day | Unit::Week) && secs == unit.secs() && !other {
            return Ok(None);
        }
        toks.used[every] = true;
        if other {
            toks.used[next_ix] = true;
        }
        for ix in used {
            toks.used[ix] = true;
        }
        (if other { secs * 2 } else { secs }, unit)
    } else {
        return Ok(None);
    };

    // "every hour at :15" → on that minute of each hour.
    if secs == 3600 {
        if let Some(minute) = take_minute_past(toks) {
            if !toks.done() {
                return Err(GENERIC_HINT.into());
            }
            return Ok(Some(Kind {
                trigger: AgentTrigger::Cron {
                    expr: format!("{minute} * * * *"),
                    timezone: zone.into(),
                },
                phrase: format!("every hour at :{minute:02}"),
                zone: Some(zone.into()),
            }));
        }
    }
    if toks.done() {
        return Ok(Some(Kind {
            trigger: AgentTrigger::Interval { every_secs: secs },
            phrase: format!("every {}", duration_phrase(secs)),
            zone: None,
        }));
    }
    // Leftovers may only be a day set: "every 30 min on weekdays".
    toks.take_all(&["on", "during"]);
    let days = take_days(toks);
    if !toks.done() || days.is_none() {
        if matches!(unit, Unit::Day | Unit::Week)
            && parse_times(&mut toks.clone(), None).is_ok_and(|times| !times.is_empty())
        {
            return Err(
                "Pick a daily time or an interval, like \"every day at 9\" or \"every 2 days\"."
                    .into(),
            );
        }
        return Err(GENERIC_HINT.into());
    }
    let days = days.unwrap_or_default();
    let (minute, hour) = match secs {
        s if s % 60 == 0 && s < 3600 && 3600 % s == 0 => (format!("*/{}", s / 60), "*".to_string()),
        s if s % 3600 == 0 && s < 86_400 && 86_400 % s == 0 => (
            "0".to_string(),
            if s == 3600 {
                "*".into()
            } else {
                format!("*/{}", s / 3600)
            },
        ),
        _ => {
            return Err(
                "On chosen days, the interval has to divide an hour or a day (like 15 min or 2 hours)."
                    .into(),
            );
        }
    };
    Ok(Some(Kind {
        trigger: AgentTrigger::Cron {
            expr: format!("{minute} {hour} * * {}", days.cron()),
            timezone: zone.into(),
        },
        phrase: format!("every {} {}", duration_phrase(secs), days.on_phrase()),
        zone: Some(zone.into()),
    }))
}

/// ":15", "at 15 past", "half past", "quarter past".
fn take_minute_past(toks: &mut Toks) -> Option<u32> {
    let found = toks.free().find_map(|(ix, word)| {
        if let Some(min) = word.strip_prefix(':').and_then(|m| m.parse::<u32>().ok()) {
            return Some((vec![ix], min));
        }
        let (next_ix, next) = toks.next_free(ix)?;
        if next != "past" {
            return None;
        }
        let min = match word {
            "half" => 30,
            "quarter" => 15,
            other => number_word(other).and_then(|n| u32::try_from(n).ok())?,
        };
        let mut used = vec![ix, next_ix];
        if let Some((after, "the" | "each" | "every")) = toks.next_free(next_ix) {
            used.push(after);
        }
        if let Some((after, "hour")) = toks.next_free(*used.last().unwrap()) {
            used.push(after);
        }
        Some((used, min))
    })?;
    let (used, minute) = found;
    if minute > 59 {
        return None;
    }
    for ix in used {
        toks.used[ix] = true;
    }
    toks.take_all(&["at", "past"]);
    Some(minute)
}

// ---------------------------------------------------------------------------
// Times of day
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Period {
    Morning,
    Afternoon,
    Evening,
    Night,
}

impl Period {
    fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "morning" | "mornings" => Self::Morning,
            "afternoon" | "afternoons" => Self::Afternoon,
            "evening" | "evenings" => Self::Evening,
            "night" | "nights" | "tonight" | "nightly" => Self::Night,
            _ => return None,
        })
    }

    fn default_time(self) -> (u32, u32) {
        match self {
            Self::Morning => (9, 0),
            Self::Afternoon => (14, 0),
            Self::Evening => (18, 0),
            Self::Night => (21, 0),
        }
    }
}

fn take_period(toks: &mut Toks) -> Option<Period> {
    let (ix, period) = toks
        .free()
        .find_map(|(ix, word)| Period::parse(word).map(|p| (ix, p)))?;
    toks.used[ix] = true;
    Some(period)
}

/// One clock token: "9", "9am", "9:30", "21:00", "9.30pm". `meridiem` is an
/// am/pm in the following token. Returns (hour, minute, had_meridiem,
/// had_minutes).
fn clock(word: &str, meridiem: Option<&str>) -> Option<Result<(u32, u32, bool, bool), String>> {
    let (body, suffix) = if let Some(body) = word.strip_suffix("am") {
        (body, Some("am"))
    } else if let Some(body) = word.strip_suffix("pm") {
        (body, Some("pm"))
    } else if let Some(body) = word.strip_suffix('a') {
        (body, Some("am"))
    } else if let Some(body) = word.strip_suffix('p') {
        (body, Some("pm"))
    } else {
        (word, meridiem)
    };
    if body.is_empty() || !body.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let (hour, minute, had_minutes) = match body.split_once([':', '.']) {
        Some((h, m)) => {
            if h.is_empty() || h.len() > 2 || m.len() != 2 {
                return None;
            }
            (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?, true)
        }
        None if body.len() <= 2 => (body.parse::<u32>().ok()?, 0, false),
        // "0930", "2100"
        None if body.len() == 4 && suffix.is_none() => (
            body[..2].parse::<u32>().ok()?,
            body[2..].parse::<u32>().ok()?,
            true,
        ),
        None => return None,
    };
    if minute > 59 {
        return Some(Err(format!("{word} isn't a time.")));
    }
    let hour = match suffix {
        Some(m) => {
            if !(1..=12).contains(&hour) {
                return Some(Err(format!("{word} isn't a time.")));
            }
            match (m, hour) {
                ("am", 12) => 0,
                ("am", h) => h,
                ("pm", 12) => 12,
                (_, h) => h + 12,
            }
        }
        None => {
            if hour > 23 {
                return Some(Err(format!("{word} isn't a time.")));
            }
            hour
        }
    };
    Some(Ok((hour, minute, suffix.is_some(), had_minutes)))
}

/// Every time of day in the text ("9am", "at 9 and 17:00", "noon").
/// `period` lifts bare hours: "every evening at 7" is 19:00. Bare numbers
/// count as hours (the caller has already consumed counts and dates).
fn parse_times(toks: &mut Toks, period: Option<Period>) -> Result<Vec<(u32, u32)>, String> {
    let mut times = Vec::new();
    let indexes: Vec<usize> = toks.free().map(|(ix, _)| ix).collect();
    for ix in indexes {
        if toks.used[ix] {
            continue;
        }
        let word = toks.items[ix].clone();
        match word.as_str() {
            "noon" | "midday" | "lunchtime" | "lunch" => {
                toks.used[ix] = true;
                times.push((12, 0));
                continue;
            }
            "midnight" => {
                toks.used[ix] = true;
                times.push((0, 0));
                continue;
            }
            _ => {}
        }
        let next = toks
            .next_free(ix)
            .filter(|(_, w)| matches!(*w, "am" | "pm"))
            .map(|(at, w)| (at, w.to_string()));
        let Some(parsed) = clock(&word, next.as_ref().map(|(_, w)| w.as_str())) else {
            continue;
        };
        let (mut hour, minute, meridiem, _) = parsed?;
        toks.used[ix] = true;
        if let Some((at, _)) = next.filter(|_| meridiem && !word.ends_with(['m', 'a', 'p'])) {
            toks.used[at] = true;
        }
        if !meridiem {
            hour = match period {
                Some(Period::Afternoon | Period::Evening) if (1..12).contains(&hour) => hour + 12,
                Some(Period::Night) if (5..12).contains(&hour) => hour + 12,
                Some(Period::Night) if hour == 12 => 0,
                _ => hour,
            };
        }
        times.push((hour, minute));
    }
    toks.take_all(&["at", "@", "by", "from"]);
    times.sort();
    times.dedup();
    Ok(times)
}

fn hhmm((hour, minute): (u32, u32)) -> String {
    format!("{hour:02}:{minute:02}")
}

fn times_phrase(times: &[(u32, u32)]) -> String {
    let labels: Vec<String> = times.iter().copied().map(hhmm).collect();
    format!("at {}", list_phrase(&labels))
}

fn list_phrase(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

// ---------------------------------------------------------------------------
// Days
// ---------------------------------------------------------------------------

/// Cron day-of-week numbers (0 = Sunday), sorted; empty = every day.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Days(Vec<u8>);

impl Days {
    fn every(&self) -> bool {
        self.0.is_empty() || self.0.len() == 7
    }

    fn cron(&self) -> String {
        if self.every() {
            return "*".into();
        }
        let days = &self.0;
        let contiguous = days.windows(2).all(|pair| pair[1] == pair[0] + 1);
        if contiguous && days.len() > 2 {
            format!("{}-{}", days[0], days[days.len() - 1])
        } else {
            days.iter().map(u8::to_string).collect::<Vec<_>>().join(",")
        }
    }

    /// "every weekday", "weekends", "every Monday", "every Mon, Wed and Fri".
    fn phrase(&self) -> String {
        match self.0.as_slice() {
            _ if self.every() => "every day".into(),
            [1, 2, 3, 4, 5] => "every weekday".into(),
            [0, 6] => "on weekends".into(),
            [one] => format!("every {}", day_name(*one, false)),
            many => {
                let names: Vec<String> = many
                    .iter()
                    .map(|d| day_name(*d, true).to_string())
                    .collect();
                format!("every {}", list_phrase(&names))
            }
        }
    }

    /// After an interval: "on weekdays", "on Mondays", "on Mon and Thu".
    fn on_phrase(&self) -> String {
        match self.0.as_slice() {
            _ if self.every() => "every day".into(),
            [1, 2, 3, 4, 5] => "on weekdays".into(),
            [0, 6] => "on weekends".into(),
            [one] => format!("on {}s", day_name(*one, false)),
            many => {
                let names: Vec<String> = many
                    .iter()
                    .map(|d| day_name(*d, true).to_string())
                    .collect();
                format!("on {}", list_phrase(&names))
            }
        }
    }
}

fn day_name(day: u8, short: bool) -> &'static str {
    const LONG: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    const SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    if short {
        SHORT[usize::from(day % 7)]
    } else {
        LONG[usize::from(day % 7)]
    }
}

/// A weekday word (singular or plural) → (cron day, plural?).
fn weekday(word: &str) -> Option<(u8, bool)> {
    let plural = word.len() > 3 && word.ends_with('s') && !word.ends_with("ss");
    let stem = if plural {
        &word[..word.len() - 1]
    } else {
        word
    };
    let day = match stem {
        "sun" | "sunday" => 0,
        "mon" | "monday" => 1,
        "tue" | "tues" | "tuesday" => 2,
        "wed" | "weds" | "wednesday" => 3,
        "thu" | "thur" | "thurs" | "thursday" => 4,
        "fri" | "friday" => 5,
        "sat" | "saturday" => 6,
        _ => return None,
    };
    Some((day, plural))
}

fn range(from: u8, to: u8) -> Vec<u8> {
    if from <= to {
        (from..=to).collect()
    } else {
        // "fri-mon" wraps through the weekend.
        (from..=6).chain(0..=to).collect()
    }
}

/// Consume a day set. `None` when the text names no days at all.
fn take_days(toks: &mut Toks) -> Option<Days> {
    let mut days: Vec<u8> = Vec::new();
    let mut every = false;
    let mut found = false;
    let indexes: Vec<usize> = toks.free().map(|(ix, _)| ix).collect();
    for ix in indexes {
        if toks.used[ix] {
            continue;
        }
        let word = toks.items[ix].clone();
        match word.as_str() {
            "weekday" | "weekdays" | "workday" | "workdays" | "weekdays." => {
                days.extend(1..=5);
            }
            "business" | "working" | "work" => {
                if let Some((next, "day" | "days")) = toks.next_free(ix) {
                    toks.used[next] = true;
                    days.extend(1..=5);
                } else {
                    continue;
                }
            }
            "weekend" | "weekends" => days.extend([0, 6]),
            "daily" | "everyday" | "day" | "days" => every = true,
            _ => {
                if let Some((from, to)) = word.split_once('-')
                    && let (Some((a, _)), Some((b, _))) = (weekday(from), weekday(to))
                {
                    days.extend(range(a, b));
                } else if let Some((day, _)) = weekday(&word) {
                    // "monday to friday", "mon through fri"
                    if let Some((link, "to" | "through" | "thru" | "until" | "till" | "-")) =
                        toks.next_free(ix)
                        && let Some((end_ix, end)) = toks.next_free(link)
                        && let Some((end_day, _)) = weekday(end)
                    {
                        toks.used[link] = true;
                        toks.used[end_ix] = true;
                        days.extend(range(day, end_day));
                    } else {
                        days.push(day);
                    }
                } else {
                    continue;
                }
            }
        }
        toks.used[ix] = true;
        found = true;
    }
    if !found {
        return None;
    }
    toks.take_all(&["every", "each", "on", "of", "week", "a", "per", "in"]);
    if every && days.is_empty() {
        return Some(Days::default());
    }
    days.sort_unstable();
    days.dedup();
    Some(Days(days))
}

// ---------------------------------------------------------------------------
// Calendar schedules
// ---------------------------------------------------------------------------

/// "1st", "2nd", "15th", "31" → day of month.
fn day_of_month(word: &str) -> Option<u32> {
    let digits = word
        .strip_suffix("st")
        .or_else(|| word.strip_suffix("nd"))
        .or_else(|| word.strip_suffix("rd"))
        .or_else(|| word.strip_suffix("th"))?;
    digits.parse().ok().filter(|d| (1..=31).contains(d))
}

fn ordinal(day: u32) -> String {
    let suffix = match (day % 10, day % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{day}{suffix}")
}

/// Daily, weekly, by weekday, monthly.
fn recurring(toks: &mut Toks, zone: &str) -> Result<Option<Kind>, String> {
    // "once a week" reached here because it is not a one-shot.
    toks.take_all(&["once"]);
    let monthly = toks.has_any(&["monthly", "month", "months"]);
    let weekly = toks.has_any(&["weekly", "week"]) && !monthly;
    let mut dom = None;
    if monthly {
        toks.take_all(&[
            "monthly", "month", "months", "every", "each", "of", "on", "a", "per",
        ]);
        let found = toks
            .free()
            .find_map(|(ix, word)| day_of_month(word).map(|d| (ix, d)));
        if let Some((ix, day)) = found {
            toks.used[ix] = true;
            dom = Some(day);
        }
    }
    if weekly {
        toks.take_all(&["weekly", "week", "every", "each", "a", "per", "once"]);
    }
    let period = take_period(toks);
    let days = take_days(toks);
    toks.take_all(&["every", "each", "on", "in"]);
    let times = parse_times(toks, period)?;
    if !toks.done() {
        return Ok(None);
    }
    if !(monthly || weekly || period.is_some() || days.is_some() || !times.is_empty()) {
        return Ok(None);
    }
    let times = if times.is_empty() {
        vec![period.map_or(DEFAULT_TIME, Period::default_time)]
    } else {
        times
    };
    let minute = times[0].1;
    if times.iter().any(|t| t.1 != minute) {
        return Err(
            "Several times need the same minutes, like 9:00 and 17:00 — or make two zoomies."
                .into(),
        );
    }
    let hours = times
        .iter()
        .map(|t| t.0.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let at = times_phrase(&times);
    let (dom_field, dow_field, phrase) = if monthly {
        let day = dom.unwrap_or(1);
        if days.as_ref().is_some_and(|d| !d.every()) {
            return Err(GENERIC_HINT.into());
        }
        (
            day.to_string(),
            "*".to_string(),
            format!("on the {} of every month {at}", ordinal(day)),
        )
    } else {
        let days = match days {
            Some(days) => days,
            None if weekly => Days(vec![1]),
            None => Days::default(),
        };
        (
            "*".to_string(),
            days.cron(),
            format!("{} {at}", days.phrase()),
        )
    };
    Ok(Some(Kind {
        trigger: AgentTrigger::Cron {
            expr: format!("{minute} {hours} {dom_field} * {dow_field}"),
            timezone: zone.into(),
        },
        phrase,
        zone: Some(zone.into()),
    }))
}

// ---------------------------------------------------------------------------
// One-shots
// ---------------------------------------------------------------------------

fn month(word: &str) -> Option<u32> {
    const NAMES: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let word = word.trim_end_matches('.');
    if word.len() < 3 {
        return None;
    }
    let prefix = &word[..3];
    let ix = NAMES.iter().position(|name| *name == prefix)?;
    const FULL: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    (FULL[ix].starts_with(word) || word == "sept").then_some(ix as u32 + 1)
}

/// What a one-shot does when its time on the named day already passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Roll {
    /// "today at 9" after 9: say so.
    Today,
    /// An explicit date: say so.
    Strict,
    /// No date, just a time: tomorrow.
    Day,
    /// A weekday: the same day next week.
    Week,
}

/// The date a one-shot names, consuming its words.
fn take_date(toks: &mut Toks, today: NaiveDate) -> Result<Option<(NaiveDate, Roll)>, String> {
    if toks.take("today") || toks.take("tonight") {
        return Ok(Some((today, Roll::Today)));
    }
    if toks.take("tomorrow") || toks.take("tmrw") || toks.take("tmr") {
        return Ok(Some((today + Duration::days(1), Roll::Strict)));
    }
    // ISO date
    let iso = toks.free().find_map(|(ix, w)| {
        NaiveDate::parse_from_str(w, "%Y-%m-%d")
            .ok()
            .map(|d| (ix, d))
    });
    if let Some((ix, date)) = iso {
        toks.used[ix] = true;
        return Ok(Some((date, Roll::Strict)));
    }
    // "nov 1", "1 nov", "november 1st"
    let named = toks.free().find_map(|(ix, w)| month(w).map(|m| (ix, m)));
    if let Some((ix, m)) = named {
        let day = toks
            .next_free(ix)
            .and_then(|(at, w)| {
                day_of_month(w)
                    .or_else(|| w.parse().ok().filter(|d: &u32| (1..=31).contains(d)))
                    .map(|d| (at, d))
            })
            .or_else(|| {
                toks.prev_free(ix).and_then(|(at, w)| {
                    day_of_month(w)
                        .or_else(|| w.parse().ok().filter(|d: &u32| (1..=31).contains(d)))
                        .map(|d| (at, d))
                })
            });
        let Some((day_ix, day)) = day else {
            return Err("Which day of the month?".into());
        };
        toks.used[ix] = true;
        toks.used[day_ix] = true;
        toks.take_all(&["of", "on"]);
        let mut date = NaiveDate::from_ymd_opt(today.year(), m, day)
            .ok_or_else(|| "That date doesn't exist.".to_string())?;
        if date < today {
            date = NaiveDate::from_ymd_opt(today.year() + 1, m, day)
                .ok_or_else(|| "That date doesn't exist.".to_string())?;
        }
        return Ok(Some((date, Roll::Strict)));
    }
    // "on friday", "next friday", "this friday"
    let weekday_hit = toks
        .free()
        .find_map(|(ix, w)| weekday(w).map(|(d, plural)| (ix, d, plural)));
    if let Some((ix, day, false)) = weekday_hit {
        toks.used[ix] = true;
        toks.take_all(&["on", "next", "coming"]);
        let target = Weekday::try_from(((day + 6) % 7) as u8).unwrap_or(Weekday::Mon);
        let ahead = (7 + target.num_days_from_monday() as i64
            - today.weekday().num_days_from_monday() as i64)
            % 7;
        return Ok(Some((today + Duration::days(ahead), Roll::Week)));
    }
    Ok(None)
}

fn once(toks: &mut Toks, tz: Tz, now: DateTime<Utc>) -> Result<Option<Kind>, String> {
    let words = toks.words();
    let relative = toks
        .free()
        .find(|(ix, w)| {
            *w == "in"
                && toks
                    .next_free(*ix)
                    .is_some_and(|(at, _)| duration_at(toks, at).is_some())
        })
        .map(|(ix, _)| ix);
    let has_named_weekday = toks.free().any(|(ix, w)| {
        weekday(w).is_some_and(|(_, plural)| !plural)
            && toks
                .prev_free(ix)
                .is_some_and(|(_, prev)| matches!(prev, "on" | "next" | "this" | "coming"))
    });
    let once_word = toks.free().any(|(ix, w)| {
        w == "once"
            && !matches!(
                toks.next_free(ix).map(|(_, next)| next),
                Some("a" | "an" | "every" | "per" | "each")
            )
    });
    let cue = once_word
        || relative.is_some()
        || toks.has_any(&["today", "tonight", "tomorrow", "tmrw", "tmr"])
        || words
            .iter()
            .any(|w| NaiveDate::parse_from_str(w, "%Y-%m-%d").is_ok())
        || (words.iter().any(|w| month(w).is_some())
            && !toks.has_any(&["every", "each", "monthly"]))
        || (has_named_weekday && !toks.has_any(&["every", "each"]));
    if !cue {
        return Ok(None);
    }
    toks.take_all(&["once", "only", "single"]);
    if let Some(ix) = relative {
        let (dur_ix, _) = toks.next_free(ix).expect("checked above");
        let (secs, _, used) = duration_at(toks, dur_ix).expect("checked above");
        toks.used[ix] = true;
        for at in used {
            toks.used[at] = true;
        }
        toks.take_all(&["from", "now"]);
        if !toks.done() {
            return Err(GENERIC_HINT.into());
        }
        if secs < 60 {
            return Err("Give it at least a minute.".into());
        }
        let at = now + Duration::seconds(i64::try_from(secs).unwrap_or(i64::MAX / 2));
        // Whole minutes, like every other schedule.
        let at = at
            .with_second(0)
            .and_then(|at| at.with_nanosecond(0))
            .unwrap_or(at);
        return Ok(Some(once_kind(at, tz, now)));
    }
    let local_now = now.with_timezone(&tz);
    let today = local_now.date_naive();
    let tonight = toks.has("tonight");
    let date = take_date(toks, today)?;
    let period = take_period(toks).or(tonight.then_some(Period::Night));
    toks.take_all(&["on", "next", "this", "coming"]);
    let times = parse_times(toks, period)?;
    if !toks.done() {
        return Err(GENERIC_HINT.into());
    }
    if times.len() > 1 {
        return Err("A one-shot runs at one time.".into());
    }
    let explicit_time = !times.is_empty();
    let (hour, minute) = times
        .first()
        .copied()
        .unwrap_or_else(|| period.map_or(DEFAULT_TIME, Period::default_time));
    let time = NaiveTime::from_hms_opt(hour, minute, 0).expect("validated");
    let (mut date, roll) = match date {
        Some(found) => found,
        None if explicit_time => (today, Roll::Day),
        None => {
            return Err("Once at what time? Like \"once at 15:00\" or \"tomorrow 9am\".".into());
        }
    };
    let local = |date: NaiveDate| {
        tz.from_local_datetime(&date.and_time(time))
            .earliest()
            .map(|at| at.with_timezone(&Utc))
    };
    let mut at =
        local(date).ok_or_else(|| "That time is skipped by a clock change.".to_string())?;
    if at <= now {
        date += match roll {
            Roll::Today => return Err("That time has already passed today.".into()),
            Roll::Strict => return Err("That time has already passed.".into()),
            // "once at 9am" after 9am means tomorrow.
            Roll::Day => Duration::days(1),
            Roll::Week => Duration::days(7),
        };
        at = local(date).ok_or_else(|| "That time is skipped by a clock change.".to_string())?;
    }
    Ok(Some(once_kind(at, tz, now)))
}

fn once_kind(at: DateTime<Utc>, tz: Tz, now: DateTime<Utc>) -> Kind {
    Kind {
        trigger: AgentTrigger::Once { at },
        phrase: format!("once, {}", moment_label(at, now, tz)),
        zone: Some(tz.name().to_string()),
    }
}

// ---------------------------------------------------------------------------
// Labels
// ---------------------------------------------------------------------------

/// "today 18:00", "tomorrow 09:00", "Mon 10:00", "Nov 3 09:00".
pub fn moment_label(at: DateTime<Utc>, now: DateTime<Utc>, tz: Tz) -> String {
    let local = at.with_timezone(&tz);
    let today = now.with_timezone(&tz).date_naive();
    let date = local.date_naive();
    let time = local.format("%H:%M");
    let days = (date - today).num_days();
    match days {
        0 => format!("today {time}"),
        1 => format!("tomorrow {time}"),
        2..=6 => format!("{} {time}", local.format("%a")),
        _ => format!("{} {time}", local.format("%b %-d")),
    }
}

/// The When row's "next:" — minutes when it is close, else a moment.
pub fn next_label(at: DateTime<Utc>, now: DateTime<Utc>, tz: Tz) -> String {
    let minutes = (at - now).num_minutes();
    if minutes < 1 {
        "in under a minute".into()
    } else if minutes < 60 {
        format!("in {minutes} min")
    } else {
        moment_label(at, now, tz)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KOLKATA: &str = "Asia/Kolkata";

    /// Wednesday 2026-10-28 10:00 in Kolkata (04:30 UTC).
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 28, 4, 30, 0).unwrap()
    }

    fn parse(text: &str) -> ParsedSchedule {
        parse_schedule(text, KOLKATA, now()).unwrap_or_else(|err| panic!("{text:?}: {err}"))
    }

    fn cron(text: &str) -> (String, String) {
        match parse(text).trigger {
            AgentTrigger::Cron { expr, timezone } => (expr, timezone),
            other => panic!("{text:?} parsed to {other:?}"),
        }
    }

    fn expr(text: &str) -> String {
        cron(text).0
    }

    fn every(text: &str) -> u64 {
        match parse(text).trigger {
            AgentTrigger::Interval { every_secs } => every_secs,
            other => panic!("{text:?} parsed to {other:?}"),
        }
    }

    fn once_at(text: &str) -> DateTime<Utc> {
        match parse(text).trigger {
            AgentTrigger::Once { at } => at,
            other => panic!("{text:?} parsed to {other:?}"),
        }
    }

    fn kolkata(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        find_zone(KOLKATA)
            .unwrap()
            .with_ymd_and_hms(y, m, d, h, min, 0)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn err(text: &str) -> String {
        parse_schedule(text, KOLKATA, now())
            .err()
            .unwrap_or_else(|| panic!("{text:?} should not parse"))
    }

    #[test]
    fn intervals() {
        for (text, secs) in [
            ("every 30 min", 1800),
            ("every 30 minutes", 1800),
            ("Every 30 mins.", 1800),
            ("every 30m", 1800),
            ("every thirty minutes", 1800),
            ("every minute", 60),
            ("every 1 minute", 60),
            ("every hour", 3600),
            ("hourly", 3600),
            ("every 2 hours", 7200),
            ("every 2h", 7200),
            ("every 1h30m", 5400),
            ("every 1 hour 30 minutes", 5400),
            ("every 90 min", 5400),
            ("every half hour", 1800),
            ("every half an hour", 1800),
            ("every quarter hour", 900),
            ("every other hour", 7200),
            ("every 2 days", 172_800),
            ("every 2 weeks", 1_209_600),
            ("run every 15 minutes please", 900),
            ("each 45 min", 2700),
        ] {
            assert_eq!(every(text), secs, "{text}");
        }
        assert_eq!(parse("every 30 min").phrase, "every 30 minutes");
        assert_eq!(parse("every hour").phrase, "every hour");
        assert_eq!(parse("every 2 days").phrase, "every 2 days");
    }

    #[test]
    fn interval_floor_is_a_minute() {
        assert_eq!(
            err("every 30 seconds"),
            "Zoomies run at most once a minute."
        );
        assert_eq!(err("every 10s"), "Zoomies run at most once a minute.");
    }

    #[test]
    fn intervals_on_days_become_cron_steps() {
        assert_eq!(expr("every 15 minutes on weekdays"), "*/15 * * * 1-5");
        assert_eq!(expr("every 2 hours on weekends"), "0 */2 * * 0,6");
        assert_eq!(expr("every hour on mondays"), "0 * * * 1");
        assert_eq!(
            parse("every 15 minutes on weekdays").phrase,
            "every 15 minutes on weekdays"
        );
        assert!(err("every 7 minutes on weekdays").contains("divide"));
    }

    #[test]
    fn hourly_on_a_minute() {
        assert_eq!(expr("every hour at :15"), "15 * * * *");
        assert_eq!(expr("hourly at :30"), "30 * * * *");
        assert_eq!(expr("every hour at half past"), "30 * * * *");
        assert_eq!(expr("every hour at quarter past"), "15 * * * *");
        assert_eq!(parse("every hour at :05").phrase, "every hour at :05");
    }

    #[test]
    fn daily() {
        for text in [
            "daily 21:00",
            "daily at 21:00",
            "every day at 9pm",
            "everyday 9 pm",
            "every day at 9 p.m.",
            "9pm daily",
            "at 21:00 every day",
            "every night at 9",
            "nightly at 9",
            "every evening at 9",
            "every day at 2100",
            "every day 21.00",
        ] {
            assert_eq!(expr(text), "0 21 * * *", "{text}");
        }
        assert_eq!(expr("every morning at 9"), "0 9 * * *");
        assert_eq!(expr("every morning"), "0 9 * * *");
        assert_eq!(expr("every evening"), "0 18 * * *");
        assert_eq!(expr("every afternoon at 3"), "0 15 * * *");
        assert_eq!(expr("daily"), "0 9 * * *");
        assert_eq!(expr("every day"), "0 9 * * *");
        assert_eq!(expr("daily at noon"), "0 12 * * *");
        assert_eq!(expr("every day at midnight"), "0 0 * * *");
        assert_eq!(expr("every day at 12am"), "0 0 * * *");
        assert_eq!(expr("every day at 12pm"), "0 12 * * *");
        assert_eq!(expr("every day at 7:45am"), "45 7 * * *");
        assert_eq!(expr("at 9am"), "0 9 * * *");
        assert_eq!(expr("9:30"), "30 9 * * *");
        assert_eq!(parse("daily 21:00").phrase, "every day at 21:00");
    }

    #[test]
    fn several_times_a_day() {
        assert_eq!(expr("every day at 9am and 5pm"), "0 9,17 * * *");
        assert_eq!(expr("weekdays at 9 and 17:00"), "0 9,17 * * 1-5");
        assert_eq!(
            parse("daily at 9am and 5pm").phrase,
            "every day at 09:00 and 17:00"
        );
        assert!(err("daily at 9am and 5:30pm").contains("same minutes"));
    }

    #[test]
    fn weekdays_and_named_days() {
        for text in [
            "weekdays at 6pm",
            "every weekday at 18:00",
            "weekdays 18:00",
            "mon-fri 6pm",
            "monday to friday at 6pm",
            "monday through friday 18:00",
            "on workdays at 6 pm",
            "every business day at 6pm",
            "weekday evenings at 6",
        ] {
            assert_eq!(expr(text), "0 18 * * 1-5", "{text}");
        }
        assert_eq!(expr("mondays 10am"), "0 10 * * 1");
        assert_eq!(expr("every monday at 10"), "0 10 * * 1");
        assert_eq!(expr("monday 10am"), "0 10 * * 1");
        assert_eq!(expr("mon, wed, fri at 9"), "0 9 * * 1,3,5");
        assert_eq!(expr("mon,wed,fri 9am"), "0 9 * * 1,3,5");
        assert_eq!(expr("tuesdays and thursdays at 14:30"), "30 14 * * 2,4");
        assert_eq!(expr("weekends at 10"), "0 10 * * 0,6");
        assert_eq!(expr("every saturday"), "0 9 * * 6");
        assert_eq!(expr("sundays"), "0 9 * * 0");
        assert_eq!(expr("fri-mon at 8"), "0 8 * * 0,1,5,6");
        assert_eq!(expr("weekly"), "0 9 * * 1");
        assert_eq!(expr("weekly on friday at 4pm"), "0 16 * * 5");
        assert_eq!(expr("once a week on sunday evening"), "0 18 * * 0");
        assert_eq!(parse("weekdays at 6pm").phrase, "every weekday at 18:00");
        assert_eq!(parse("mondays 10am").phrase, "every Monday at 10:00");
        assert_eq!(
            parse("mon,wed,fri 9am").phrase,
            "every Mon, Wed and Fri at 09:00"
        );
        assert_eq!(parse("weekends at 10").phrase, "on weekends at 10:00");
    }

    #[test]
    fn monthly() {
        assert_eq!(expr("monthly"), "0 9 1 * *");
        assert_eq!(expr("every month on the 15th at 8am"), "0 8 15 * *");
        assert_eq!(expr("on the 1st of every month"), "0 9 1 * *");
        assert_eq!(expr("the 3rd of each month at 17:00"), "0 17 3 * *");
        assert_eq!(
            parse("every month on the 15th at 8am").phrase,
            "on the 15th of every month at 08:00"
        );
    }

    #[test]
    fn raw_cron_passes_through() {
        assert_eq!(expr("0 9 * * 1-5"), "0 9 * * 1-5");
        assert_eq!(expr("cron 0 9 * * 1-5"), "0 9 * * 1-5");
        assert_eq!(expr("*/10 * * * *"), "*/10 * * * *");
        assert_eq!(
            parse("0 9 * * 1-5").phrase,
            "on the cron schedule 0 9 * * 1-5"
        );
        assert!(err("99 9 * * *").contains("cron"));
    }

    #[test]
    fn time_zones() {
        assert_eq!(cron("weekdays at 6pm").1, KOLKATA);
        assert_eq!(cron("daily 21:00 utc").1, "UTC");
        assert_eq!(cron("daily at 9am in Europe/Berlin").1, "Europe/Berlin");
        assert_eq!(cron("daily at 9am europe/berlin").1, "Europe/Berlin");
        assert_eq!(cron("mondays 10am pst").1, "America/Los_Angeles");
        assert_eq!(cron("9am ist every day").1, KOLKATA);
        // An unknown local zone falls back to UTC rather than failing.
        let parsed = parse_schedule("daily 9am", "Nowhere/Land", now()).unwrap();
        assert_eq!(parsed.zone.as_deref(), Some("UTC"));
        // The zone does not leak into interval triggers.
        assert_eq!(every("every 30 min utc"), 1800);
    }

    #[test]
    fn one_shots() {
        assert_eq!(once_at("once at 15:00"), kolkata(2026, 10, 28, 15, 0));
        // Already past today → tomorrow.
        assert_eq!(once_at("once at 9am"), kolkata(2026, 10, 29, 9, 0));
        assert_eq!(once_at("tomorrow 9am"), kolkata(2026, 10, 29, 9, 0));
        assert_eq!(once_at("tomorrow at 9"), kolkata(2026, 10, 29, 9, 0));
        assert_eq!(once_at("tomorrow"), kolkata(2026, 10, 29, 9, 0));
        assert_eq!(once_at("tomorrow morning"), kolkata(2026, 10, 29, 9, 0));
        assert_eq!(once_at("today at 5pm"), kolkata(2026, 10, 28, 17, 0));
        assert_eq!(once_at("tonight at 11"), kolkata(2026, 10, 28, 23, 0));
        assert_eq!(once_at("tonight"), kolkata(2026, 10, 28, 21, 0));
        assert_eq!(once_at("in 2 hours"), now() + Duration::hours(2));
        assert_eq!(once_at("in 30 min"), now() + Duration::minutes(30));
        assert_eq!(once_at("in an hour"), now() + Duration::hours(1));
        assert_eq!(once_at("in half an hour"), now() + Duration::minutes(30));
        assert_eq!(once_at("once in 3 days"), now() + Duration::days(3));
        assert_eq!(once_at("on friday at 10"), kolkata(2026, 10, 30, 10, 0));
        assert_eq!(once_at("next friday 10am"), kolkata(2026, 10, 30, 10, 0));
        assert_eq!(once_at("once on monday"), kolkata(2026, 11, 2, 9, 0));
        // Today's weekday after its time: next week, not an error.
        assert_eq!(once_at("on wednesday at 9am"), kolkata(2026, 11, 4, 9, 0));
        assert_eq!(
            once_at("on wednesday at 11am"),
            kolkata(2026, 10, 28, 11, 0)
        );
        assert_eq!(once_at("2026-11-01 09:00"), kolkata(2026, 11, 1, 9, 0));
        assert_eq!(once_at("on 2026-11-01 at 6pm"), kolkata(2026, 11, 1, 18, 0));
        assert_eq!(once_at("nov 3 at 14:00"), kolkata(2026, 11, 3, 14, 0));
        assert_eq!(once_at("3rd november 2pm"), kolkata(2026, 11, 3, 14, 0));
        // A date already behind us this year means next year.
        assert_eq!(once_at("jan 5 at 9"), kolkata(2027, 1, 5, 9, 0));
        assert_eq!(
            once_at("once at 15:00 utc"),
            Utc.with_ymd_and_hms(2026, 10, 28, 15, 0, 0).unwrap()
        );
        assert_eq!(parse("tomorrow 9am").phrase, "once, tomorrow 09:00");
    }

    #[test]
    fn one_shot_errors() {
        assert_eq!(err("today at 9am"), "That time has already passed today.");
        assert_eq!(err("2026-10-01 09:00"), "That time has already passed.");
        assert!(err("once").contains("what time"));
        assert!(err("once at ").contains("what time"));
        assert_eq!(err("tomorrow at 9 and 5"), "A one-shot runs at one time.");
        assert_eq!(err("in 10 seconds"), "Give it at least a minute.");
        assert_eq!(err("feb 30 at 9"), "That date doesn't exist.");
    }

    #[test]
    fn event_triggers() {
        for text in [
            "when files change",
            "when files change in this project",
            "on file change",
            "whenever a file is saved",
            "on save",
            "when anything changes",
        ] {
            assert_eq!(
                parse(text).trigger,
                AgentTrigger::FileWatch {
                    paths: vec![],
                    debounce_ms: None
                },
                "{text}"
            );
        }
        assert_eq!(
            parse("when files change in src/ and docs").trigger,
            AgentTrigger::FileWatch {
                paths: vec!["src".into(), "docs".into()],
                debounce_ms: None
            }
        );
        assert_eq!(
            parse("when files change in src").phrase,
            "whenever files change in src"
        );
        for text in [
            "when a webhook fires",
            "webhook",
            "on webhook",
            "when its web hook is called",
        ] {
            assert_eq!(parse(text).trigger, AgentTrigger::Webhook, "{text}");
        }
        assert_eq!(parse("webhook").next, None);
    }

    #[test]
    fn garbage_and_edge_cases_are_rejected_with_a_hint() {
        assert_eq!(err(""), EMPTY_HINT);
        assert_eq!(err("   "), EMPTY_HINT);
        assert_eq!(err("please"), EMPTY_HINT);
        for text in [
            "whenever",
            "sometimes",
            "every blue moon",
            "every second tuesday of the month",
            "twice a day",
            "every 30 min on my birthday",
        ] {
            assert_eq!(err(text), GENERIC_HINT, "{text}");
        }
        assert!(err("every day at 25:00").contains("isn't a time"));
        assert!(err("daily at 13pm").contains("isn't a time"));
        assert!(err("daily at 9:75").contains("isn't a time"));
        assert!(err("every 2 days at 9am").contains("daily time or an interval"));
    }

    #[test]
    fn presets_parse_and_round_trip() {
        for preset in Preset::ALL {
            assert_eq!(Preset::matching(preset.text()), Some(preset));
            if preset != Preset::Once {
                parse(preset.text());
            }
        }
        assert_eq!(expr(Preset::EveryMorning.text()), "0 9 * * *");
        assert_eq!(every(Preset::EveryHour.text()), 3600);
        assert_eq!(expr(Preset::WeekdayEvenings.text()), "0 18 * * 1-5");
        assert_eq!(
            Preset::matching("  Weekdays at 6PM "),
            Some(Preset::WeekdayEvenings)
        );
        assert_eq!(Preset::matching("weekdays at 7pm"), None);
    }

    #[test]
    fn live_summary_names_zone_and_next_run() {
        // Wednesday 10:00 IST: the next weekday 18:00 is today.
        assert_eq!(
            parse("weekdays at 6pm").summary(now()),
            "Runs every weekday at 18:00 IST — next: today 18:00"
        );
        let evening = kolkata(2026, 10, 28, 19, 0);
        let parsed = parse_schedule("weekdays at 6pm", KOLKATA, evening).unwrap();
        assert_eq!(
            parsed.summary(evening),
            "Runs every weekday at 18:00 IST — next: tomorrow 18:00"
        );
        // Friday evening → Monday.
        let friday = kolkata(2026, 10, 30, 19, 0);
        let parsed = parse_schedule("weekdays at 6pm", KOLKATA, friday).unwrap();
        assert_eq!(
            parsed.summary(friday),
            "Runs every weekday at 18:00 IST — next: Mon 18:00"
        );
        assert_eq!(
            parse("every 30 min").summary(now()),
            "Runs every 30 minutes — next: in 30 min"
        );
        assert_eq!(
            parse("when files change").summary(now()),
            "Runs whenever files change in this project"
        );
        assert!(parse("webhook").summary(now()).contains("Settings"));
        let berlin = parse("daily at 9am in Europe/Berlin").summary(now());
        assert!(
            berlin.starts_with("Runs every day at 09:00 CET"),
            "{berlin}"
        );
        assert_eq!(
            parse("monthly").summary(now()),
            "Runs on the 1st of every month at 09:00 IST — next: Sun 09:00"
        );
    }

    #[test]
    fn dst_aware_next_run() {
        // New York leaves DST on 2026-11-01: 09:00 local is 13:00 UTC before
        // and 14:00 UTC after.
        let saturday = Utc.with_ymd_and_hms(2026, 10, 31, 20, 0, 0).unwrap();
        let parsed = parse_schedule("daily 9am", "America/New_York", saturday).unwrap();
        assert_eq!(
            parsed.next,
            Some(Utc.with_ymd_and_hms(2026, 11, 1, 14, 0, 0).unwrap())
        );
        assert!(parsed.summary(saturday).contains("EST"));
    }
}
