//! Account-scoped statistics for actual playback on this PC.
//!
//! The UI supplies engine observations, not Spotify's remote or optimistic
//! playback state. Only monotonic, short observation intervals are credited.
//! SQLite, including opening the file, runs on a lazy background worker.
//!
//! Cumulative buckets coalesce by process run, account, local day and track.
//! Their stable randomized identities and absolute counters make retrying a
//! committed write harmless. A failed disk never discards pending listening.
//! The mailbox has one read request and one response, not a queue per frame.
//! The compact run map grows with distinct track/day/account combinations,
//! not with observations, saves or repeat listens.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use jiff::{Timestamp, civil::Date, tz::TimeZone};
use rusqlite::{Connection, params};

use crate::api::models::Track;
use crate::backend::Waker;

const SAVE_INTERVAL: Duration = Duration::from_secs(5);
const MAX_GAP: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Period {
    AllTime,
    /// The first day of a local calendar month. Other days are normalized.
    Month(Date),
}

impl Default for Period {
    fn default() -> Self {
        Self::Month(jiff::Zoned::now().date().first_of_month())
    }
}

impl Period {
    fn normalized(self) -> Self {
        match self {
            Self::Month(date) => Self::Month(date.first_of_month()),
            Self::AllTime => Self::AllTime,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Sample {
    pub track: Track,
    pub sequence: u64,
    pub playing: bool,
    pub position_ms: u32,
    pub seek_sequence: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub plays: u64,
    pub listened_ms: u64,
    pub unique_tracks: u64,
    pub unique_albums: u64,
    pub unique_artists: u64,
    pub tracks: Vec<Rank>,
    pub albums: Vec<Rank>,
    /// Each credited artist receives the track's full listening time and plays.
    /// Consequently, summing artist rows can exceed the report's totals.
    pub artists: Vec<Rank>,
    /// Ascending local calendar date. Days without activity are omitted.
    pub days: Vec<Day>,
}

#[derive(Clone, Debug, Default)]
pub struct Rank {
    /// Empty when there is no known navigable URI.
    pub uri: String,
    pub name: String,
    pub subtitle: String,
    pub image: Option<String>,
    pub plays: u64,
    pub listened_ms: u64,
}

#[derive(Clone, Debug)]
pub struct Day {
    pub date: Date,
    pub listened_ms: u64,
    pub plays: u64,
}

pub struct Stats {
    pub period: Period,
    pub report: Option<Report>,
    pub error: Option<String>,
    pub loading: bool,
    path: PathBuf,
    waker: Waker,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    account: Option<String>,
    request: u64,
    selected_period: Period,
    active: Option<Active>,
    last_save: Option<Instant>,
    run: u128,
    zone: TimeZone,
}

struct Active {
    sample: Sample,
    now: Instant,
    at: Timestamp,
    started: Timestamp,
    start_date: Date,
    listened: Duration,
    counted: bool,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Bucket {
    account: String,
    date: Date,
    track: String,
}

#[derive(Clone)]
struct Pending {
    id: String,
    track: Track,
    listened_ms: u64,
    plays: u64,
    started_ms: i64,
    updated_ms: i64,
    revision: u64,
    acknowledged: u64,
}

#[derive(Clone)]
struct Query {
    id: u64,
    account: String,
    period: Period,
}

struct Response {
    id: u64,
    result: Result<Report, String>,
}

#[derive(Default)]
struct Mailbox {
    pending: HashMap<Bucket, Pending>,
    generation: u64,
    committed: u64,
    write_requested: bool,
    read_requested: bool,
    query: Option<Query>,
    response: Option<Response>,
    stop: bool,
}

#[derive(Default)]
struct Shared {
    mail: Mutex<Mailbox>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Mailbox> {
        self.mail.lock().unwrap_or_else(|error| error.into_inner())
    }
}

impl Stats {
    /// No statistics file is opened and no thread is started here.
    pub fn new(path: PathBuf, waker: &Waker) -> Self {
        let local = jiff::Zoned::now();
        let period = Period::Month(local.date().first_of_month());
        Self {
            period,
            report: None,
            error: None,
            loading: false,
            path,
            waker: waker.clone(),
            shared: Arc::new(Shared::default()),
            worker: None,
            account: None,
            request: 0,
            selected_period: period,
            active: None,
            last_save: None,
            run: rand::random(),
            zone: local.time_zone().clone(),
        }
    }

    /// Observe actual engine state. A paused/loading sample keeps the same
    /// listen; `None` ends it. Only a new engine sequence or track starts a
    /// new listen. Seeking never resets the threshold or creates another play.
    pub fn observe(
        &mut self,
        account: Option<&str>,
        mut sample: Option<Sample>,
        now: Instant,
        at: Timestamp,
    ) {
        let account = account.filter(|account| !account.is_empty());
        let account_changed = self.account.as_deref() != account;
        let mut keep = None;
        let mut edge = account_changed;

        if let Some(mut active) = self.active.take() {
            let same = !account_changed
                && sample.as_ref().is_some_and(|sample| {
                    sample.sequence == active.sample.sequence
                        && track_key(&sample.track) == track_key(&active.sample.track)
                });
            if same
                && let Some(current) = sample.as_ref()
                && active.sample.track != current.track
            {
                // Cache enrichment can arrive on the final pause observation.
                // Use it for this interval too, retaining the previous engine
                // position/playing state for the elapsed-time calculation.
                active.sample.track = current.track.clone();
            }
            // Credit the last brief interval even when its ending observation
            // is paused, loading, a different track, or an account switch.
            // It belongs exclusively to the previously selected account.
            let elapsed = now.checked_duration_since(active.now).unwrap_or_default();
            if active.sample.playing && elapsed <= MAX_GAP {
                let duration = active.sample.track.duration_ms;
                let remaining = if duration == 0 {
                    MAX_GAP
                } else {
                    Duration::from_millis(u64::from(
                        duration.saturating_sub(active.sample.position_ms),
                    ))
                };
                let mut credit = elapsed.min(remaining);
                if same
                    && let Some(current) = sample.as_ref()
                    && current.seek_sequence == active.sample.seek_sequence
                    && duration > 0
                    && current.position_ms >= duration.saturating_sub(1_000)
                {
                    // End positions can be rounded just below the duration.
                    // Require progress in the final second, so a late/repeated
                    // end event never keeps adding the same remaining tail.
                    credit = credit.min(Duration::from_millis(u64::from(
                        current
                            .position_ms
                            .saturating_sub(active.sample.position_ms),
                    )));
                }
                if let Some(previous_account) = self.account.clone() {
                    self.credit(&previous_account, &mut active, credit);
                }
            }
            edge |= !same
                || sample
                    .as_ref()
                    .is_some_and(|sample| active.sample.playing && !sample.playing);
            if same && let Some(sample) = sample.take() {
                active.sample = sample;
                active.now = now;
                active.at = at;
                keep = Some(active);
            }
        }

        if account_changed {
            self.account = account.map(str::to_owned);
            self.invalidate();
        }
        if keep.is_none()
            && account.is_some()
            && let Some(sample) = sample
            && sample.playing
        {
            keep = Some(Active {
                sample,
                now,
                at,
                started: at,
                start_date: at.to_zoned(self.zone.clone()).date(),
                listened: Duration::ZERO,
                counted: false,
            });
        }
        self.active = keep;

        if account.is_some() {
            self.start_worker();
        }
        if edge
            || self
                .last_save
                .is_none_or(|last| now.saturating_duration_since(last) >= SAVE_INTERVAL)
        {
            self.request_write();
            self.last_save = Some(now);
        }
        self.poll();
    }

    pub fn set_period(&mut self, period: Period) {
        let period = period.normalized();
        if self.selected_period != period {
            self.period = period;
            self.invalidate();
        } else {
            self.period = period;
        }
    }

    /// Refreshes the selected account and period after pending writes.
    pub fn refresh(&mut self) {
        self.request = self.request.wrapping_add(1);
        self.error = None;
        self.schedule_read();
    }

    /// Drains the coalesced worker response. Stale request identities are
    /// ignored, including errors from an account or period no longer shown.
    pub fn poll(&mut self) {
        if self.period.normalized() != self.selected_period {
            self.invalidate();
        }
        let response = self.shared.lock().response.take();
        if let Some(response) = response
            && response.id == self.request
            && self.account.is_some()
        {
            self.loading = false;
            match response.result {
                Ok(report) => {
                    self.report = Some(report);
                    self.error = None;
                }
                Err(error) => self.error = Some(error),
            }
        }
        if self.account.is_some() {
            self.start_worker();
        }
    }

    /// Persists all observed listening up to this call. It does not invent a
    /// further interval after the last observation. Only call this blocking
    /// method for shutdown/suspend, not on a normal UI frame.
    pub fn flush(&mut self, timeout: Duration) -> bool {
        let saved = {
            let mail = self.shared.lock();
            mail.committed >= mail.generation
        };
        if saved {
            self.poll();
            return true;
        }
        self.start_worker();
        let deadline = Instant::now().checked_add(timeout);
        let shared = self.shared.clone();
        let mut mail = shared.lock();
        let target = mail.generation;
        mail.write_requested = true;
        shared.changed.notify_one();
        while mail.committed < target {
            let remaining = deadline
                .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
                .unwrap_or_default();
            if remaining.is_zero() || self.worker.is_none() {
                break;
            }
            let (next, timed) = shared
                .changed
                .wait_timeout(mail, remaining)
                .unwrap_or_else(|error| error.into_inner());
            mail = next;
            if timed.timed_out() {
                break;
            }
        }
        let saved = mail.committed >= target;
        drop(mail);
        self.poll();
        saved
    }

    /// Seeds the selected account through the same coalesced write path.
    /// Each tuple describes a listen starting at the UTC timestamp, with its
    /// actual listened milliseconds. Calls add listens; they do not replace
    /// existing statistics.
    #[cfg(any(test, feature = "demo"))]
    pub fn seed(&mut self, account: &str, entries: Vec<(Track, Timestamp, u64)>) {
        if account.is_empty() {
            return;
        }
        self.observe(Some(account), None, Instant::now(), Timestamp::now());
        for (track, at, listened_ms) in entries {
            let mut active = Active {
                sample: Sample {
                    track,
                    sequence: 0,
                    playing: true,
                    position_ms: 0,
                    seek_sequence: 0,
                },
                now: Instant::now(),
                at,
                started: at,
                start_date: at.to_zoned(self.zone.clone()).date(),
                listened: Duration::ZERO,
                counted: false,
            };
            self.credit(account, &mut active, Duration::from_millis(listened_ms));
        }
        self.request_write();
        self.refresh();
    }

    fn invalidate(&mut self) {
        self.period = self.period.normalized();
        self.selected_period = self.period;
        self.report = None;
        self.error = None;
        self.request = self.request.wrapping_add(1);
        self.schedule_read();
    }

    fn schedule_read(&mut self) {
        self.loading = self.account.is_some();
        let mut mail = self.shared.lock();
        mail.query = self.account.as_ref().map(|account| Query {
            id: self.request,
            account: account.clone(),
            period: self.period.normalized(),
        });
        mail.response = None;
        mail.read_requested = self.account.is_some();
        self.shared.changed.notify_one();
    }

    fn start_worker(&mut self) {
        if let Some(worker) = self.worker.as_ref() {
            if !worker.is_finished() {
                return;
            }
            // Joining a finished worker cannot wait for disk I/O. If it
            // panicked, the snapshots still belong to the shared mailbox.
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
        let shared = self.shared.clone();
        let path = self.path.clone();
        let waker = self.waker.clone();
        match thread::Builder::new()
            .name("listening-statistics".into())
            .spawn(move || run_worker(path, shared, waker))
        {
            Ok(worker) => self.worker = Some(worker),
            Err(error) => {
                self.error = Some(format!("Could not start listening statistics: {error}"));
                self.loading = false;
            }
        }
    }

    fn request_write(&self) {
        let mut mail = self.shared.lock();
        if mail.committed < mail.generation {
            mail.write_requested = true;
            self.shared.changed.notify_one();
        }
    }

    fn credit(&mut self, account: &str, active: &mut Active, elapsed: Duration) {
        let previous_ms = active.listened.as_millis();
        active.listened = active.listened.saturating_add(elapsed);
        let milliseconds = (active.listened.as_millis() - previous_ms).min(u64::MAX as u128) as u64;
        let mut start = active.at.as_millisecond();
        let mut left = milliseconds;
        while left > 0 {
            let Ok(timestamp) = Timestamp::from_millisecond(start) else {
                self.error = Some("Listening timestamp is outside the supported range".into());
                break;
            };
            let local = timestamp.to_zoned(self.zone.clone());
            let next = local
                .date()
                .tomorrow()
                .and_then(|date| date.to_zoned(self.zone.clone()))
                .map(|next| next.timestamp().as_millisecond())
                .unwrap_or(i64::MAX);
            let part = left.min(next.saturating_sub(start).max(1) as u64);
            self.add(account, active, local.date(), part, 0, start);
            left -= part;
            start = start.saturating_add(part.min(i64::MAX as u64) as i64);
        }
        if !active.counted
            && active.listened >= crate::history::counts_after(active.sample.track.duration_ms)
        {
            self.add(
                account,
                active,
                active.start_date,
                0,
                1,
                active.started.as_millisecond(),
            );
            active.counted = true;
        }
    }

    fn add(
        &self,
        account: &str,
        active: &Active,
        date: Date,
        listened_ms: u64,
        plays: u64,
        at_ms: i64,
    ) {
        let key = Bucket {
            account: account.to_owned(),
            date,
            track: track_key(&active.sample.track),
        };
        let mut mail = self.shared.lock();
        mail.generation = mail.generation.saturating_add(1);
        let revision = mail.generation;
        let row = mail.pending.entry(key).or_insert_with(|| Pending {
            // Keep this identity and its cumulative counters after saving.
            // Engine sequences and wall timestamps may repeat after restart;
            // the random process-run identity must not.
            id: format!("{:032x}:{:032x}", self.run, rand::random::<u128>()),
            track: active.sample.track.clone(),
            listened_ms: 0,
            plays: 0,
            started_ms: active.started.as_millisecond(),
            updated_ms: at_ms,
            revision,
            acknowledged: 0,
        });
        if row.track != active.sample.track {
            row.track = active.sample.track.clone();
        }
        row.listened_ms = row.listened_ms.saturating_add(listened_ms);
        row.plays = row.plays.saturating_add(plays);
        row.started_ms = row.started_ms.min(active.started.as_millisecond());
        row.updated_ms = row
            .updated_ms
            .max(at_ms.saturating_add(listened_ms.min(i64::MAX as u64) as i64));
        row.revision = revision;
    }
}

impl Drop for Stats {
    fn drop(&mut self) {
        // Do not block the UI on destruction. Explicit flush is the shutdown
        // durability barrier; the worker also makes a final best-effort write.
        let mut mail = self.shared.lock();
        mail.stop = true;
        self.shared.changed.notify_one();
    }
}

fn run_worker(path: PathBuf, shared: Arc<Shared>, waker: Waker) {
    let mut connection = None;
    let mut last_attempt = Instant::now();
    loop {
        let mut mail = shared.lock();
        while !mail.stop
            && !mail.write_requested
            && !mail.read_requested
            && (mail.committed >= mail.generation || last_attempt.elapsed() < SAVE_INTERVAL)
        {
            let wait = if mail.committed >= mail.generation {
                SAVE_INTERVAL
            } else {
                SAVE_INTERVAL.saturating_sub(last_attempt.elapsed())
            };
            let (next, _) = shared
                .changed
                .wait_timeout(mail, wait)
                .unwrap_or_else(|error| error.into_inner());
            mail = next;
        }
        let stop = mail.stop;
        let generation = mail.generation;
        let pending: Vec<_> = mail
            .pending
            .iter()
            .filter(|(_, row)| row.revision > row.acknowledged)
            .map(|(key, row)| (key.clone(), row.clone()))
            .collect();
        let query = mail.query.clone();
        mail.write_requested = false;
        mail.read_requested = false;
        drop(mail);
        last_attempt = Instant::now();

        let result = (|| -> anyhow::Result<Option<Report>> {
            if connection.is_none() {
                connection = Some(open_database(&path)?);
            }
            let database = connection.as_mut().expect("opened above");
            if !pending.is_empty() {
                write_pending(database, &pending)?;
            }
            // Publish the durability barrier before a potentially long read.
            {
                let mut mail = shared.lock();
                for (key, saved) in &pending {
                    if let Some(row) = mail.pending.get_mut(key)
                        && row.id == saved.id
                    {
                        row.acknowledged = row.acknowledged.max(saved.revision);
                    }
                }
                mail.committed = mail.committed.max(generation);
                shared.changed.notify_all();
            }
            query
                .as_ref()
                .map(|query| read_report(database, query))
                .transpose()
        })();
        let result = result
            .map_err(|error| format!("Could not read or save listening statistics: {error:#}"));
        if result.is_err() {
            // Reopen on the next attempt, but never rename, truncate or replace
            // a damaged database. Unacknowledged snapshots remain in memory.
            connection = None;
        }
        if let Some(query) = query {
            let response = match result {
                Ok(Some(report)) => Some(Ok(report)),
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            };
            if let Some(result) = response {
                shared.lock().response = Some(Response {
                    id: query.id,
                    result,
                });
                waker.wake();
            }
        }
        if stop {
            return;
        }
    }
}

fn open_database(path: &std::path::Path) -> anyhow::Result<Connection> {
    use anyhow::Context;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).context("create statistics directory")?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).context("open private statistics file")?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "statistics path is not a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    let database = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    database.busy_timeout(Duration::from_millis(250))?;
    // Rollback journaling keeps a live backup straightforward: use SQLite's
    // backup API, or stop/flush and copy the single database file. Never copy a
    // database during a transaction without SQLite's locking/backup protocol.
    database.execute_batch(
        "PRAGMA journal_mode=DELETE;
         PRAGMA synchronous=FULL;
         CREATE TABLE IF NOT EXISTS listening (
             id TEXT PRIMARY KEY NOT NULL,
             account TEXT NOT NULL,
             local_date TEXT NOT NULL,
             track_key TEXT NOT NULL,
             track_json TEXT NOT NULL,
             listened_ms INTEGER NOT NULL CHECK (listened_ms >= 0),
             plays INTEGER NOT NULL CHECK (plays >= 0),
             started_at_ms INTEGER NOT NULL,
             updated_at_ms INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS listening_account_date
             ON listening(account, local_date);",
    )?;
    Ok(database)
}

fn write_pending(database: &mut Connection, pending: &[(Bucket, Pending)]) -> anyhow::Result<()> {
    let transaction = database.transaction()?;
    {
        let mut statement = transaction.prepare_cached(
            "INSERT INTO listening
             (id, account, local_date, track_key, track_json, listened_ms,
              plays, started_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
               track_json = excluded.track_json,
               listened_ms = MAX(listening.listened_ms, excluded.listened_ms),
               plays = MAX(listening.plays, excluded.plays),
               started_at_ms = MIN(listening.started_at_ms, excluded.started_at_ms),
               updated_at_ms = MAX(listening.updated_at_ms, excluded.updated_at_ms)",
        )?;
        for (key, row) in pending {
            statement.execute(params![
                row.id,
                key.account,
                key.date.to_string(),
                key.track,
                serde_json::to_string(&row.track)?,
                i64::try_from(row.listened_ms)?,
                i64::try_from(row.plays)?,
                row.started_ms,
                row.updated_ms,
            ])?;
        }
    }
    transaction.commit()?;
    Ok(())
}

fn track_key(track: &Track) -> String {
    if !track.uri.is_empty() {
        return track.uri.clone();
    }
    if let Some(id) = track.id.as_deref().filter(|id| !id.is_empty()) {
        return format!("spotify:track:{id}");
    }
    format!(
        "metadata:{:?}:{:?}:{}",
        track.name,
        track.artist_names(),
        track.duration_ms
    )
}

fn artist_key(artist: &crate::api::models::ArtistRef) -> Option<(String, String)> {
    if let Some(uri) = artist.uri.as_ref().filter(|uri| !uri.is_empty()) {
        Some((uri.clone(), uri.clone()))
    } else if let Some(id) = artist.id.as_ref().filter(|id| !id.is_empty()) {
        let uri = format!("spotify:artist:{id}");
        Some((uri.clone(), uri))
    } else if !artist.name.is_empty() {
        Some((format!("name:{:?}", artist.name), String::new()))
    } else {
        None
    }
}

fn add_rank(ranks: &mut BTreeMap<String, Rank>, key: String, entry: Rank) {
    ranks
        .entry(key)
        .and_modify(|rank| {
            rank.plays = rank.plays.saturating_add(entry.plays);
            rank.listened_ms = rank.listened_ms.saturating_add(entry.listened_ms);
            if rank.image.is_none() {
                rank.image = entry.image.clone();
            }
        })
        .or_insert(entry);
}

fn top(ranks: BTreeMap<String, Rank>) -> Vec<Rank> {
    let mut ranks: Vec<_> = ranks.into_values().collect();
    ranks.sort_by(|a, b| {
        b.plays
            .cmp(&a.plays)
            .then_with(|| b.listened_ms.cmp(&a.listened_ms))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.uri.cmp(&b.uri))
    });
    ranks.truncate(50);
    ranks
}

fn read_report(database: &Connection, query: &Query) -> anyhow::Result<Report> {
    let (first, last) = match query.period {
        Period::AllTime => (None, None),
        Period::Month(date) => (
            Some(date.first_of_month().to_string()),
            Some(date.last_of_month().to_string()),
        ),
    };
    let mut statement = database.prepare_cached(
        "SELECT local_date, track_key, track_json, listened_ms, plays
         FROM listening WHERE account = ?1
         AND (?2 IS NULL OR local_date >= ?2)
         AND (?3 IS NULL OR local_date <= ?3)
         ORDER BY local_date, track_key, id",
    )?;
    let mut rows = statement.query(params![query.account, first, last])?;
    let mut report = Report::default();
    let mut tracks = BTreeMap::new();
    let mut albums = BTreeMap::new();
    let mut artists = BTreeMap::new();
    let mut days = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let date: Date = row.get::<_, String>(0)?.parse()?;
        let key: String = row.get(1)?;
        let track: Track = serde_json::from_str(&row.get::<_, String>(2)?)?;
        let listened_ms = u64::try_from(row.get::<_, i64>(3)?)?;
        let plays = u64::try_from(row.get::<_, i64>(4)?)?;
        if listened_ms == 0 && plays == 0 {
            continue;
        }
        report.plays = report.plays.saturating_add(plays);
        report.listened_ms = report.listened_ms.saturating_add(listened_ms);
        let day = days.entry(date).or_insert(Day {
            date,
            listened_ms: 0,
            plays: 0,
        });
        day.listened_ms = day.listened_ms.saturating_add(listened_ms);
        day.plays = day.plays.saturating_add(plays);
        let subtitle = track.artist_names();
        let uri = if track.uri.is_empty() {
            track
                .id
                .as_ref()
                .filter(|id| !id.is_empty())
                .map(|id| format!("spotify:track:{id}"))
                .unwrap_or_default()
        } else {
            track.uri.clone()
        };
        add_rank(
            &mut tracks,
            key,
            Rank {
                uri,
                name: track.name.clone(),
                subtitle: subtitle.clone(),
                image: track.image(160).map(str::to_owned),
                plays,
                listened_ms,
            },
        );

        let mut credited = HashSet::new();
        for artist in &track.artists {
            if let Some((key, uri)) = artist_key(artist)
                && credited.insert(key.clone())
            {
                add_rank(
                    &mut artists,
                    key,
                    Rank {
                        uri,
                        name: artist.name.clone(),
                        plays,
                        listened_ms,
                        ..Rank::default()
                    },
                );
            }
        }
        if let Some(album) = &track.album {
            let uri = if !album.uri.is_empty() {
                album.uri.clone()
            } else if !album.id.is_empty() {
                format!("spotify:album:{}", album.id)
            } else {
                String::new()
            };
            // Missing album metadata is omitted rather than inventing one
            // album shared by every track without an album.
            if !uri.is_empty() || !album.name.is_empty() {
                let key = if !uri.is_empty() {
                    uri.clone()
                } else {
                    let mut identities: Vec<_> = credited.into_iter().collect();
                    identities.sort();
                    format!("metadata:{identities:?}:{:?}", album.name)
                };
                add_rank(
                    &mut albums,
                    key,
                    Rank {
                        uri,
                        name: album.name.clone(),
                        subtitle,
                        image: track.image(160).map(str::to_owned),
                        plays,
                        listened_ms,
                    },
                );
            }
        }
    }
    report.unique_tracks = tracks.len() as u64;
    report.unique_albums = albums.len() as u64;
    report.unique_artists = artists.len() as u64;
    report.tracks = top(tracks);
    report.albums = top(albums);
    report.artists = top(artists);
    report.days = days.into_values().collect();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::{Album, ArtistRef};

    struct Fixture {
        directory: PathBuf,
        stats: Stats,
        origin: Instant,
        at: Timestamp,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!(
                "spotifast-statistics-{:032x}",
                rand::random::<u128>()
            ));
            let mut stats = Stats::new(directory.join("listening.sqlite3"), &Waker::default());
            // Inject a fixed zone without changing the process-global TZ in
            // parallel tests. Production obtains this zone from jiff/system.
            stats.zone = TimeZone::UTC;
            stats.set_period(Period::AllTime);
            Self {
                directory,
                stats,
                origin: Instant::now(),
                at: "2026-01-15T12:00:00Z".parse().unwrap(),
            }
        }

        fn observe(&mut self, account: Option<&str>, sample: Option<Sample>, milliseconds: u64) {
            self.stats.observe(
                account,
                sample,
                self.origin + Duration::from_millis(milliseconds),
                self.at
                    .checked_add(Duration::from_millis(milliseconds))
                    .unwrap(),
            );
        }

        fn report(&mut self) -> Report {
            assert!(
                self.stats.flush(Duration::from_secs(3)),
                "{:?}",
                self.stats.error
            );
            self.stats.refresh();
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                self.stats.poll();
                assert!(self.stats.error.is_none(), "{:?}", self.stats.error);
                if !self.stats.loading {
                    return self.stats.report.clone().expect("completed report");
                }
                assert!(Instant::now() < deadline, "statistics read timed out");
                thread::sleep(Duration::from_millis(2));
            }
        }

        fn wait_for_error(&mut self) -> String {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                self.stats.poll();
                if let Some(error) = self.stats.error.clone() {
                    return error;
                }
                assert!(
                    Instant::now() < deadline,
                    "statistics error was not published"
                );
                thread::sleep(Duration::from_millis(2));
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // Tests wait for their private worker before removing its files.
            // Production Drop deliberately does not block the UI this way.
            self.stats.shared.lock().stop = true;
            self.stats.shared.changed.notify_one();
            if let Some(worker) = self.stats.worker.take() {
                worker.join().unwrap();
            }
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    fn track(name: &str, duration_ms: u32) -> Track {
        Track {
            uri: format!("spotify:track:{name}"),
            name: name.to_owned(),
            duration_ms,
            artists: vec![ArtistRef {
                id: Some("artist".into()),
                name: "Artist".into(),
                ..ArtistRef::default()
            }],
            ..Track::default()
        }
    }

    fn sample(track: &Track, sequence: u64, position_ms: u32, playing: bool) -> Sample {
        Sample {
            track: track.clone(),
            sequence,
            position_ms,
            playing,
            seek_sequence: 0,
        }
    }

    #[test]
    fn construction_and_signed_out_frames_do_not_create_a_database() {
        let mut fixture = Fixture::new();
        fixture.stats.refresh();
        for milliseconds in [0, 1_000, 60_000] {
            fixture.observe(
                None,
                Some(sample(&track("ignored", 4_000), 1, 0, true)),
                milliseconds,
            );
        }
        assert!(fixture.stats.flush(Duration::ZERO));
        assert!(!fixture.directory.exists());
        assert!(fixture.stats.report.is_none());
        assert!(!fixture.stats.loading);
    }

    #[test]
    fn pause_loading_and_sleep_keep_only_actual_short_intervals() {
        let mut fixture = Fixture::new();
        let track = track("pauses", 20_000);
        fixture.observe(Some("alice"), Some(sample(&track, 1, 0, true)), 0);
        fixture.observe(Some("alice"), Some(sample(&track, 1, 2_000, true)), 2_000);
        // Include the brief interval that ends in a pause/loading sample.
        fixture.observe(Some("alice"), Some(sample(&track, 1, 2_250, false)), 2_250);
        fixture.observe(Some("alice"), Some(sample(&track, 1, 2_250, false)), 90_000);
        fixture.observe(Some("alice"), Some(sample(&track, 1, 2_250, true)), 90_000);
        fixture.observe(Some("alice"), Some(sample(&track, 1, 3_250, true)), 91_000);
        // No callback while the PC sleeps: do not credit the 30 minute gap.
        fixture.observe(
            Some("alice"),
            Some(sample(&track, 1, 4_250, true)),
            1_891_000,
        );
        fixture.observe(Some("alice"), None, 1_891_500);
        let report = fixture.report();
        assert_eq!(report.listened_ms, 3_750);
        assert_eq!(report.plays, 0);
        assert_eq!(report.unique_tracks, 1);
    }

    #[test]
    fn thirty_second_threshold_survives_loading_without_counting_the_loading_time() {
        let mut fixture = Fixture::new();
        let track = track("threshold", 240_000);
        fixture.observe(Some("alice"), Some(sample(&track, 1, 0, true)), 0);
        for second in 1..=29 {
            fixture.observe(
                Some("alice"),
                Some(sample(&track, 1, second * 1_000, true)),
                u64::from(second) * 1_000,
            );
        }
        let before = fixture.report();
        assert_eq!((before.plays, before.listened_ms), (0, 29_000));
        fixture.observe(
            Some("alice"),
            Some(sample(&track, 1, 29_000, false)),
            29_000,
        );
        fixture.observe(
            Some("alice"),
            Some(sample(&track, 1, 29_000, false)),
            100_000,
        );
        fixture.observe(
            Some("alice"),
            Some(sample(&track, 1, 29_000, true)),
            100_000,
        );
        fixture.observe(
            Some("alice"),
            Some(sample(&track, 1, 30_000, false)),
            101_000,
        );
        fixture.observe(Some("alice"), None, 102_000);
        let after = fixture.report();
        assert_eq!((after.plays, after.listened_ms), (1, 30_000));
    }

    #[test]
    fn seek_distance_is_not_time_and_only_new_sequences_count_repeat_plays() {
        let mut fixture = Fixture::new();
        let track = track("repeat", 8_000);
        fixture.observe(Some("alice"), Some(sample(&track, 1, 0, true)), 0);
        fixture.observe(Some("alice"), Some(sample(&track, 1, 2_000, true)), 2_000);
        let mut sought = sample(&track, 1, 6_000, true);
        sought.seek_sequence = 1;
        fixture.observe(Some("alice"), Some(sought.clone()), 2_000);
        sought.position_ms = 8_000;
        fixture.observe(Some("alice"), Some(sought.clone()), 4_000);
        fixture.observe(Some("alice"), Some(sought), 5_000);
        fixture.observe(Some("alice"), Some(sample(&track, 2, 0, true)), 5_000);
        fixture.observe(Some("alice"), Some(sample(&track, 2, 4_000, false)), 9_000);
        let report = fixture.report();
        assert_eq!(report.listened_ms, 8_000);
        assert_eq!(report.plays, 2);
        assert_eq!(report.tracks.len(), 1);
    }

    #[test]
    fn ended_and_rounded_end_positions_do_not_accumulate_time() {
        let mut fixture = Fixture::new();
        let track = track("end", 4_000);
        fixture.observe(Some("alice"), Some(sample(&track, 1, 3_500, true)), 0);
        fixture.observe(Some("alice"), Some(sample(&track, 1, 3_999, true)), 1_000);
        for second in 2..10 {
            fixture.observe(
                Some("alice"),
                Some(sample(&track, 1, 3_999, true)),
                second * 1_000,
            );
        }
        fixture.observe(Some("alice"), Some(sample(&track, 1, 4_000, true)), 10_000);
        fixture.observe(Some("alice"), Some(sample(&track, 1, 4_000, true)), 11_000);
        fixture.observe(Some("alice"), None, 12_000);
        let report = fixture.report();
        assert_eq!(report.listened_ms, 500);
        assert_eq!(report.plays, 0);
    }

    #[test]
    fn short_skips_keep_time_and_track_switch_keeps_the_last_interval() {
        let mut fixture = Fixture::new();
        let first = track("skip", 240_000);
        let second = track("short", 4_000);
        fixture.observe(Some("alice"), Some(sample(&first, 1, 0, true)), 0);
        fixture.observe(Some("alice"), Some(sample(&second, 2, 0, true)), 750);
        fixture.observe(Some("alice"), Some(sample(&second, 2, 2_000, false)), 2_750);
        let report = fixture.report();
        assert_eq!(report.plays, 1);
        assert_eq!(report.listened_ms, 2_750);
        assert_eq!(report.tracks[0].name, "short");
        assert_eq!(report.tracks[1].listened_ms, 750);
        assert_eq!(report.tracks[1].plays, 0);
    }

    #[test]
    fn continued_listening_after_flush_and_restart_is_not_lost_or_doubled() {
        let mut fixture = Fixture::new();
        let track = track("durable", 20_000);
        fixture.observe(Some("alice"), Some(sample(&track, 1, 0, true)), 0);
        for second in 1..=12 {
            fixture.observe(
                Some("alice"),
                Some(sample(&track, 1, second * 1_000, true)),
                u64::from(second) * 1_000,
            );
            assert!(fixture.stats.flush(Duration::from_secs(3)));
        }
        assert_eq!(fixture.report().listened_ms, 12_000);
        assert_eq!(fixture.report().plays, 1);
        // Repeat the same URI/day after more than two acknowledged saves.
        fixture.observe(Some("alice"), Some(sample(&track, 2, 0, true)), 12_000);
        for second in 1..=12 {
            fixture.observe(
                Some("alice"),
                Some(sample(&track, 2, second * 1_000, true)),
                12_000 + u64::from(second) * 1_000,
            );
            assert!(fixture.stats.flush(Duration::from_secs(3)));
        }
        assert_eq!(fixture.report().listened_ms, 24_000);
        assert_eq!(fixture.report().plays, 2);
        assert!(fixture.stats.flush(Duration::from_secs(3)));
        assert!(fixture.stats.flush(Duration::from_secs(3)));

        let mut restarted = Stats::new(fixture.stats.path.clone(), &Waker::default());
        restarted.zone = TimeZone::UTC;
        restarted.set_period(Period::AllTime);
        restarted.observe(Some("alice"), None, Instant::now(), fixture.at);
        let old = std::mem::replace(&mut fixture.stats, restarted);
        drop(old);
        assert_eq!(fixture.report().listened_ms, 24_000);
        assert_eq!(fixture.report().plays, 2);
        // Engine sequences restart too. A new process must not deduplicate it
        // against the old process's sequence 1.
        fixture.observe(Some("alice"), Some(sample(&track, 1, 0, true)), 30_000);
        fixture.observe(Some("alice"), Some(sample(&track, 1, 2_000, false)), 32_000);
        assert_eq!(fixture.report().listened_ms, 26_000);
        assert_eq!(fixture.report().plays, 2);
    }

    #[test]
    fn account_switches_clear_reports_and_errors_and_never_mix_listening() {
        let mut fixture = Fixture::new();
        let track = track("accounts", 4_000);
        fixture.observe(Some("alice"), Some(sample(&track, 1, 0, true)), 0);
        fixture.observe(Some("alice"), Some(sample(&track, 1, 2_000, false)), 2_000);
        assert_eq!(fixture.report().plays, 1);
        fixture.stats.error = Some("old account error".into());
        fixture.observe(Some("bob"), Some(sample(&track, 1, 0, true)), 3_000);
        assert!(
            fixture
                .stats
                .report
                .as_ref()
                .is_none_or(|report| report.plays == 0)
        );
        assert!(fixture.stats.error.is_none());
        fixture.observe(Some("bob"), Some(sample(&track, 1, 500, true)), 3_500);
        // A final interval at an account switch still belongs to Bob.
        fixture.observe(Some("alice"), None, 3_750);
        assert_eq!(fixture.report().listened_ms, 2_000);
        fixture.observe(Some("bob"), None, 4_000);
        let bob = fixture.report();
        assert_eq!(bob.listened_ms, 750);
        assert_eq!(bob.plays, 0);
        fixture.observe(None, None, 4_000);
        fixture.stats.poll();
        assert!(fixture.stats.report.is_none());
        assert!(fixture.stats.error.is_none());
        assert!(!fixture.stats.loading);
    }

    #[test]
    fn local_month_boundary_splits_time_but_assigns_play_to_start_day() {
        let mut fixture = Fixture::new();
        fixture.stats.zone = TimeZone::get("America/New_York").unwrap();
        // UTC is already February. New York is still January for two seconds.
        fixture.at = "2026-02-01T04:59:58Z".parse().unwrap();
        let track = track("midnight", 8_000);
        fixture.observe(Some("alice"), Some(sample(&track, 1, 0, true)), 0);
        fixture.observe(Some("alice"), Some(sample(&track, 1, 4_000, false)), 4_000);
        let all = fixture.report();
        assert_eq!(all.listened_ms, 4_000);
        assert_eq!(all.plays, 1);
        assert_eq!(all.days.len(), 2);
        assert_eq!(all.days[0].date, "2026-01-31".parse::<Date>().unwrap());
        assert_eq!(all.days[0].listened_ms, 2_000);
        assert_eq!(all.days[0].plays, 1);
        assert_eq!(all.days[1].date, "2026-02-01".parse::<Date>().unwrap());
        assert_eq!(all.days[1].listened_ms, 2_000);
        assert_eq!(all.days[1].plays, 0);
        fixture
            .stats
            .set_period(Period::Month("2026-01-14".parse().unwrap()));
        assert!(fixture.stats.report.is_none());
        assert_eq!(
            fixture.stats.period,
            Period::Month("2026-01-01".parse().unwrap())
        );
        let january = fixture.report();
        assert_eq!((january.plays, january.listened_ms), (1, 2_000));
        fixture
            .stats
            .set_period(Period::Month("2026-02-01".parse().unwrap()));
        let february = fixture.report();
        assert_eq!((february.plays, february.listened_ms), (0, 2_000));
    }

    #[test]
    fn rankings_are_top_fifty_with_full_artist_credit_and_distinct_album_fallbacks() {
        let mut fixture = Fixture::new();
        let mut entries = Vec::new();
        for number in (0..55).rev() {
            let mut track = track(&format!("Track {number:02}"), 4_000);
            track.album = Some(Album {
                name: "Same album name".into(),
                ..Album::default()
            });
            track.artists.push(ArtistRef {
                uri: Some(format!("spotify:artist:{number}")),
                name: format!("Collaborator {number}"),
                ..ArtistRef::default()
            });
            // Duplicate credits on a track do not double its artist totals.
            track.artists.push(track.artists[0].clone());
            entries.push((track, fixture.at, 2_000));
        }
        let unknown = track("Unknown album", 4_000);
        entries.push((unknown, fixture.at, 500));
        fixture.stats.seed("alice", entries);
        let report = fixture.report();
        assert_eq!(report.plays, 55);
        assert_eq!(report.listened_ms, 110_500);
        assert_eq!(report.unique_tracks, 56);
        assert_eq!(report.unique_albums, 55);
        assert_eq!(report.unique_artists, 56);
        assert_eq!(report.tracks.len(), 50);
        assert_eq!(report.tracks[0].name, "Track 00");
        assert_eq!(report.tracks[49].name, "Track 49");
        assert_eq!(report.albums.len(), 50);
        assert!(report.albums.iter().all(|album| album.plays == 1));
        assert_eq!(report.artists[0].name, "Artist");
        assert_eq!(report.artists[0].listened_ms, 110_500);
        assert_eq!(report.artists[0].plays, 55);
    }

    #[test]
    fn artist_ids_and_name_fallbacks_and_known_album_uris_are_grouped() {
        let mut fixture = Fixture::new();
        let mut first = track("one", 4_000);
        first.artists.push(ArtistRef {
            name: "Name only".into(),
            ..ArtistRef::default()
        });
        first.album = Some(Album {
            uri: "spotify:album:shared".into(),
            name: "Shared".into(),
            ..Album::default()
        });
        let mut second = first.clone();
        second.name = "two".into();
        second.uri.clear();
        second.id = Some("two".into());
        second.artists[0].id = None;
        second.artists[0].uri = Some("spotify:artist:artist".into());
        fixture.stats.seed(
            "alice",
            vec![(first, fixture.at, 2_000), (second, fixture.at, 2_000)],
        );
        let report = fixture.report();
        assert_eq!(report.unique_artists, 2);
        assert_eq!(report.albums.len(), 1);
        assert_eq!(report.albums[0].plays, 2);
        assert!(
            report
                .tracks
                .iter()
                .any(|track| track.uri == "spotify:track:two")
        );
        assert!(
            report
                .artists
                .iter()
                .all(|artist| artist.listened_ms == 4_000 && artist.plays == 2)
        );
    }

    #[test]
    fn metadata_enriched_on_pause_updates_an_already_saved_bucket() {
        let mut fixture = Fixture::new();
        let mut track = track("enriched", 4_000);
        fixture.observe(Some("alice"), Some(sample(&track, 1, 0, true)), 0);
        fixture.observe(Some("alice"), Some(sample(&track, 1, 1_000, true)), 1_000);
        assert!(fixture.report().albums.is_empty());
        track.album = Some(Album {
            id: "found".into(),
            name: "Found album".into(),
            images: vec![crate::api::models::Image {
                url: "https://i.scdn.co/image/found".into(),
                width: Some(160),
                height: Some(160),
            }],
            ..Album::default()
        });
        fixture.observe(Some("alice"), Some(sample(&track, 1, 2_000, false)), 2_000);
        let report = fixture.report();
        assert_eq!(report.albums.len(), 1);
        assert_eq!(report.albums[0].uri, "spotify:album:found");
        assert_eq!(report.albums[0].name, "Found album");
        assert_eq!(report.unique_albums, 1);
        assert_eq!(
            report.albums[0].image.as_deref(),
            Some("https://i.scdn.co/image/found")
        );
        assert_eq!(report.tracks[0].image, report.albums[0].image);
        assert_eq!(
            (report.albums[0].plays, report.albums[0].listened_ms),
            (1, 2_000)
        );
    }

    #[test]
    fn daylight_saving_days_have_their_actual_length() {
        let mut fixture = Fixture::new();
        fixture.stats.zone = TimeZone::get("America/New_York").unwrap();
        let at = "2026-03-08T05:00:00Z".parse().unwrap();
        fixture
            .stats
            .seed("alice", vec![(track("dst", 240_000), at, 86_400_000)]);
        let report = fixture.report();
        assert_eq!(report.listened_ms, 86_400_000);
        assert_eq!(report.days.len(), 2);
        assert_eq!(report.days[0].date, "2026-03-08".parse::<Date>().unwrap());
        assert_eq!(report.days[0].listened_ms, 23 * 60 * 60 * 1_000);
        assert_eq!(report.days[0].plays, 1);
        assert_eq!(report.days[1].listened_ms, 60 * 60 * 1_000);
        assert_eq!(report.days[1].plays, 0);
    }

    #[test]
    fn failed_directory_is_visible_and_pending_listening_survives_recovery() {
        let mut fixture = Fixture::new();
        std::fs::write(&fixture.directory, b"not a directory").unwrap();
        let track = track("retry", 4_000);
        fixture.observe(Some("alice"), Some(sample(&track, 1, 0, true)), 0);
        fixture.observe(Some("alice"), Some(sample(&track, 1, 2_000, false)), 2_000);
        assert!(!fixture.stats.flush(Duration::from_millis(100)));
        assert!(fixture.wait_for_error().contains("statistics"));
        assert_eq!(
            std::fs::read(&fixture.directory).unwrap(),
            b"not a directory"
        );
        std::fs::remove_file(&fixture.directory).unwrap();
        let report = fixture.report();
        assert_eq!((report.plays, report.listened_ms), (1, 2_000));
        assert!(fixture.stats.flush(Duration::from_secs(3)));
        let again = fixture.report();
        assert_eq!((again.plays, again.listened_ms), (1, 2_000));
    }

    #[test]
    fn corrupt_database_is_reported_and_never_replaced() {
        let mut fixture = Fixture::new();
        std::fs::create_dir_all(&fixture.directory).unwrap();
        let corrupt = b"This is deliberately not an SQLite database.";
        std::fs::write(&fixture.stats.path, corrupt).unwrap();
        fixture
            .stats
            .seed("alice", vec![(track("corrupt", 4_000), fixture.at, 2_000)]);
        assert!(!fixture.stats.flush(Duration::from_millis(100)));
        assert!(fixture.wait_for_error().contains("statistics"));
        assert_eq!(std::fs::read(&fixture.stats.path).unwrap(), corrupt);
        fixture
            .stats
            .set_period(Period::Month("2026-01-01".parse().unwrap()));
        assert!(fixture.stats.report.is_none());
        assert!(fixture.stats.error.is_none());
        fixture.observe(None, None, 0);
        thread::sleep(Duration::from_millis(10));
        fixture.stats.poll();
        assert!(
            fixture.stats.error.is_none(),
            "old account error leaked after sign-out"
        );
    }

    #[cfg(unix)]
    #[test]
    fn database_is_private_even_when_an_existing_file_was_public() {
        use std::os::unix::fs::PermissionsExt;
        let mut fixture = Fixture::new();
        std::fs::create_dir_all(&fixture.directory).unwrap();
        std::fs::write(&fixture.stats.path, []).unwrap();
        std::fs::set_permissions(&fixture.stats.path, std::fs::Permissions::from_mode(0o644))
            .unwrap();
        fixture
            .stats
            .seed("alice", vec![(track("private", 4_000), fixture.at, 2_000)]);
        assert_eq!(fixture.report().plays, 1);
        assert_eq!(
            std::fs::metadata(&fixture.stats.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
