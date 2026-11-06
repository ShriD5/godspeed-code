//! Folding usage records into a [`UsageHistory`]: local-day buckets in the
//! viewer's zone, the Godspeed-vs-log dedupe, attribution to chats and
//! projects, and the cost estimate. Pure — the store hands it rows.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use zeron_proto::pricing::{PRICES_CHECKED, canonical_model, price_for};
use zeron_proto::{
    UsageAmount, UsageBucket, UsageBucketSize, UsageChatSlice, UsageHistory, UsagePoint,
    UsageRange, UsageSlice, provider_label,
};

use super::{Source, UsageRecord};

/// A log record this close before a Godspeed turn's prompt, or after its
/// end, still belongs to that turn (clock skew, the transcript write lag).
const WINDOW_LEAD_MS: i64 = 5_000;
const WINDOW_TRAIL_MS: i64 = 30_000;
/// `All` past this many days buckets by week.
const MAX_DAILY_BUCKETS: i64 = 92;
/// Chats listed in the top list.
const TOP_CHATS: usize = 10;

/// What the registry knows about a chat.
#[derive(Debug, Clone, Default)]
pub(crate) struct ChatInfo {
    pub title: Option<String>,
    /// `(key, label)` of its project.
    pub project: Option<(String, String)>,
    /// Kebab-case harness id of its config.
    pub harness: Option<String>,
    /// The harness session the chat resumes (imported chats: the source's).
    pub session_id: Option<String>,
}

/// The registry side of a query: chats by id, and project folders (path,
/// label) to place CLI sessions by their working directory.
#[derive(Debug, Clone, Default)]
pub(crate) struct Registry {
    pub chats: HashMap<String, ChatInfo>,
    pub projects: Vec<(String, String)>,
}

/// The range resolved in a zone: its bucket starts, in order, and its end.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Bounds {
    pub size: UsageBucketSize,
    /// `(start, local day, local hour)` per bucket.
    pub buckets: Vec<(DateTime<Utc>, NaiveDate, Option<u8>)>,
    pub end: DateTime<Utc>,
}

impl Bounds {
    pub fn start(&self) -> DateTime<Utc> {
        self.buckets.first().map_or(self.end, |b| b.0)
    }
}

/// The first instant of `date` at `hour` local time, or `None` when that
/// hour doesn't exist (a DST gap). A repeated hour starts at its first pass.
fn local_hour(tz: &Tz, date: NaiveDate, hour: u32) -> Option<DateTime<Utc>> {
    tz.from_local_datetime(&date.and_hms_opt(hour, 0, 0)?)
        .earliest()
        .map(|at| at.with_timezone(&Utc))
}

/// Local midnight of `date` — or, where midnight falls in a DST gap, the
/// day's first existing hour.
pub(crate) fn local_day_start(tz: &Tz, date: NaiveDate) -> DateTime<Utc> {
    (0..24)
        .find_map(|hour| local_hour(tz, date, hour))
        .unwrap_or_else(|| Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).expect("midnight")))
}

pub(crate) fn local_date(tz: &Tz, at_ms: i64) -> NaiveDate {
    Utc.timestamp_millis_opt(at_ms)
        .single()
        .unwrap_or_default()
        .with_timezone(tz)
        .date_naive()
}

/// Resolve `range` in `tz` at `now`. `first_at` (the oldest record) sizes
/// `All`; with nothing indexed, `All` is just today.
pub(crate) fn bounds(range: UsageRange, tz: &Tz, now_ms: i64, first_at: Option<i64>) -> Bounds {
    let today = local_date(tz, now_ms);
    let end = local_day_start(tz, today + Duration::days(1));
    if range == UsageRange::Today {
        let buckets = (0..24u32)
            .filter_map(|hour| local_hour(tz, today, hour).map(|at| (at, today, Some(hour as u8))))
            .collect();
        return Bounds {
            size: UsageBucketSize::Hour,
            buckets,
            end,
        };
    }
    let first_day = match range.days() {
        Some(days) => today - Duration::days(i64::from(days) - 1),
        None => first_at.map_or(today, |at| local_date(tz, at).min(today)),
    };
    let span = (today - first_day).num_days() + 1;
    let (size, mut day, step) = if range == UsageRange::All && span > MAX_DAILY_BUCKETS {
        let monday =
            first_day - Duration::days(i64::from(first_day.weekday().num_days_from_monday()));
        (UsageBucketSize::Week, monday, 7)
    } else {
        (UsageBucketSize::Day, first_day, 1)
    };
    let mut buckets = Vec::new();
    while day <= today {
        buckets.push((local_day_start(tz, day), day, None));
        day += Duration::days(step);
    }
    Bounds { size, buckets, end }
}

/// A record's cost at list prices: `(usd, unpriced tokens, cache savings)`.
fn price(record: &UsageRecord) -> (f64, u64, f64) {
    let t = &record.tokens;
    match record.model.as_deref().and_then(price_for) {
        Some(p) => (
            p.cost(t.input, t.output, t.cache_read, t.cache_write),
            0,
            p.cache_savings(t.cache_read),
        ),
        None => (0.0, t.total(), 0.0),
    }
}

fn amount_of(record: &UsageRecord) -> UsageAmount {
    let (cost_usd, unpriced_tokens, cache_savings_usd) = price(record);
    let godspeed = record.source == Source::Godspeed;
    UsageAmount {
        tokens: record.tokens,
        cost_usd,
        unpriced_tokens,
        cache_savings_usd,
        turns: u64::from(godspeed),
        calls: u64::from(!godspeed),
    }
}

/// Godspeed's own turns among `records`, folded for the telemetry ping
/// (`crate::telemetry`): turns per harness, tokens and the estimated cost at
/// list prices. Agent-log records (history outside Godspeed) are left out.
/// Pure.
pub(crate) fn godspeed_totals(records: &[UsageRecord]) -> super::DayUsage {
    let mut day = super::DayUsage::default();
    for record in records.iter().filter(|r| r.source == Source::Godspeed) {
        let amount = amount_of(record);
        day.turns += 1;
        *day.by_harness.entry(record.provider.clone()).or_default() += 1;
        day.tokens.add(&amount.tokens);
        day.cost_usd += amount.cost_usd;
    }
    day
}

fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(trimmed)
}

/// The project a working directory sits in: the deepest registered project
/// folder containing it, else the folder itself.
fn project_for_cwd(registry: &Registry, cwd: &str) -> (String, String) {
    let cwd = cwd.trim_end_matches('/');
    registry
        .projects
        .iter()
        .filter(|(path, _)| {
            let path = path.trim_end_matches('/');
            cwd == path
                || cwd
                    .strip_prefix(path)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
        .max_by_key(|(path, _)| path.len())
        .cloned()
        .unwrap_or_else(|| (cwd.to_owned(), basename(cwd).to_owned()))
}

/// Rank most expensive first, then most tokens, then by key (stable).
fn sort_slices(slices: &mut [UsageSlice]) {
    slices.sort_by(|a, b| {
        b.amount
            .cost_usd
            .total_cmp(&a.amount.cost_usd)
            .then(b.amount.tokens.total().cmp(&a.amount.tokens.total()))
            .then(a.key.cmp(&b.key))
    });
}

struct ChatAcc {
    title: String,
    provider: String,
    project: Option<String>,
    last_at: i64,
    amount: UsageAmount,
}

/// Fold `records` (anything near the range; out-of-range rows only feed the
/// dedupe) into the page's answer.
pub(crate) fn aggregate(
    range: UsageRange,
    tz: &Tz,
    bounds: &Bounds,
    records: &[UsageRecord],
    registry: &Registry,
) -> UsageHistory {
    let start_ms = bounds.start().timestamp_millis();
    let end_ms = bounds.end.timestamp_millis();
    let chat_session = |record: &UsageRecord| -> Option<String> {
        record.session_id.clone().or_else(|| {
            registry
                .chats
                .get(record.chat_id.as_deref()?)?
                .session_id
                .clone()
        })
    };

    // Godspeed turns claim their session's log lines inside their window;
    // a session any chat resumes is that chat's.
    let mut windows: HashMap<(String, String), Vec<(i64, i64)>> = HashMap::new();
    let mut session_chat: HashMap<(String, String), String> = HashMap::new();
    for (id, chat) in &registry.chats {
        if let (Some(harness), Some(session)) = (&chat.harness, &chat.session_id)
            && !session.is_empty()
        {
            session_chat.insert((harness.clone(), session.clone()), id.clone());
        }
    }
    for record in records.iter().filter(|r| r.source == Source::Godspeed) {
        let Some(session) = chat_session(record).filter(|s| !s.is_empty()) else {
            continue;
        };
        let key = (record.provider.clone(), session);
        let started = record.started_ms.unwrap_or(record.at_ms);
        windows
            .entry(key.clone())
            .or_default()
            .push((started - WINDOW_LEAD_MS, record.at_ms + WINDOW_TRAIL_MS));
        if let Some(chat) = &record.chat_id {
            session_chat.entry(key).or_insert_with(|| chat.clone());
        }
    }

    let mut out = UsageHistory {
        range,
        bucket: bounds.size,
        time_zone: tz.name().to_owned(),
        start: Some(bounds.start()),
        end: Some(bounds.end),
        prices_checked: PRICES_CHECKED.to_owned(),
        ..Default::default()
    };
    let mut points: Vec<BTreeMap<(String, String), UsageAmount>> =
        vec![BTreeMap::new(); bounds.buckets.len()];
    let mut by_model: HashMap<String, UsageAmount> = HashMap::new();
    let mut by_provider: HashMap<String, UsageAmount> = HashMap::new();
    let mut by_project: HashMap<String, (String, UsageAmount)> = HashMap::new();
    let mut chats: HashMap<String, ChatAcc> = HashMap::new();
    let mut sessions: HashSet<String> = HashSet::new();

    for record in records {
        if record.at_ms < start_ms || record.at_ms >= end_ms {
            continue;
        }
        let session = if record.source == Source::Godspeed {
            None
        } else {
            record.session_id.clone().filter(|s| !s.is_empty())
        };
        if let Some(session) = &session {
            let key = (record.provider.clone(), session.clone());
            let claimed = windows.get(&key).is_some_and(|spans| {
                spans
                    .iter()
                    .any(|(from, to)| (*from..=*to).contains(&record.at_ms))
            });
            if claimed {
                out.sources.deduplicated += 1;
                continue;
            }
        }
        let chat_id = record.chat_id.clone().or_else(|| {
            session
                .as_ref()
                .and_then(|s| session_chat.get(&(record.provider.clone(), s.clone())))
                .cloned()
        });
        let chat = chat_id.as_deref().and_then(|id| registry.chats.get(id));
        let project = match (chat, &record.cwd) {
            (Some(chat), _) => chat.project.clone(),
            (None, Some(cwd)) => Some(project_for_cwd(registry, cwd)),
            (None, None) => None,
        };
        let amount = amount_of(record);
        let model = record
            .model
            .as_deref()
            .map(canonical_model)
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "unknown".into());

        let bucket = bounds
            .buckets
            .partition_point(|(at, _, _)| at.timestamp_millis() <= record.at_ms)
            .saturating_sub(1);
        points[bucket]
            .entry((record.provider.clone(), model.clone()))
            .or_default()
            .add(&amount);
        out.total.add(&amount);
        by_model.entry(model).or_default().add(&amount);
        by_provider
            .entry(record.provider.clone())
            .or_default()
            .add(&amount);
        let (project_key, project_label) = project
            .clone()
            .unwrap_or_else(|| (String::new(), "No project".into()));
        by_project
            .entry(project_key)
            .or_insert_with(|| (project_label, UsageAmount::default()))
            .1
            .add(&amount);

        let chat_key = match (&chat_id, &session) {
            (Some(id), _) => id.clone(),
            (None, Some(s)) => format!("session:{}:{s}", record.provider),
            (None, None) => format!("session:{}:", record.provider),
        };
        sessions.insert(chat_key.clone());
        let acc = chats.entry(chat_key).or_insert_with(|| ChatAcc {
            title: match (&chat_id, chat) {
                (Some(_), Some(chat)) => {
                    chat.title.clone().unwrap_or_else(|| "Untitled chat".into())
                }
                (Some(_), None) => "Deleted chat".into(),
                (None, _) => format!("{} session", provider_label(&record.provider)),
            },
            provider: record.provider.clone(),
            project: project.map(|(_, label)| label),
            last_at: record.at_ms,
            amount: UsageAmount::default(),
        });
        acc.last_at = acc.last_at.max(record.at_ms);
        acc.amount.add(&amount);
    }

    out.buckets = bounds
        .buckets
        .iter()
        .zip(points)
        .map(|((start, day, hour), points)| UsageBucket {
            start: *start,
            day: *day,
            hour: *hour,
            points: points
                .into_iter()
                .map(|((provider, model), amount)| UsagePoint {
                    provider,
                    model,
                    amount,
                })
                .collect(),
        })
        .collect();
    out.sessions = sessions.len() as u64;
    out.by_model = by_model
        .into_iter()
        .map(|(key, amount)| UsageSlice {
            label: key.clone(),
            key,
            amount,
        })
        .collect();
    out.by_provider = by_provider
        .into_iter()
        .map(|(key, amount)| UsageSlice {
            label: provider_label(&key),
            key,
            amount,
        })
        .collect();
    out.by_project = by_project
        .into_iter()
        .map(|(key, (label, amount))| UsageSlice { key, label, amount })
        .collect();
    sort_slices(&mut out.by_model);
    sort_slices(&mut out.by_provider);
    sort_slices(&mut out.by_project);
    let mut top: Vec<(String, ChatAcc)> = chats.into_iter().collect();
    top.sort_by(|(ka, a), (kb, b)| {
        b.amount
            .cost_usd
            .total_cmp(&a.amount.cost_usd)
            .then(b.amount.tokens.total().cmp(&a.amount.tokens.total()))
            .then(ka.cmp(kb))
    });
    out.top_chats = top
        .into_iter()
        .take(TOP_CHATS)
        .map(|(key, acc)| UsageChatSlice {
            chat_id: (!key.starts_with("session:")).then_some(key),
            title: acc.title,
            provider: acc.provider,
            project: acc.project,
            last_at: Utc
                .timestamp_millis_opt(acc.last_at)
                .single()
                .unwrap_or_default(),
            amount: acc.amount,
        })
        .collect();
    out
}
