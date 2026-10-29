//! The sessions service. Follows every live Claude Code session on the machine and hands the hub a
//! [`SessionInfo`] snapshot whenever one changes, plus [`FeedEvent`]s as they happen.
//!
//! Sources, under `%USERPROFILE%\.claude` unless `EREBUS_CLAUDE_DIR` says otherwise:
//!
//! - `sessions\<pid>.json` lists sessions. One is live while its pid runs and that process's
//!   creation time equals `procStart`. A new `sessionId` under the same pid ends the old session
//!   and starts a new one.
//! - `projects\<slug>\<sessionId>.jsonl` is the transcript, tailed by byte offset.
//! - `projects\<slug>\<sessionId>\subagents\agent-<id>.jsonl` and `.meta.json` are its workers.
//! - The Erebus mod's spool (`%LOCALAPPDATA%\Erebus\spool`, or `EREBUS_SPOOL_DIR`) reports
//!   permission prompts. Each file is read once and deleted.
//!
//! The service never opens `sessions\*.key` or anything under `daemon\`.
//!
//! One thread does all the IO. A file change opens a 250 ms window that gathers further changes,
//! then one pass runs, so a busy session cannot postpone a pass. A pass also runs every 2 s,
//! because a process can die without touching a file.
//!
//! Feed policy: a tool call shows when it starts. Its result shows only when it fails, when a
//! command, fetch or MCP call ran longer than 10 s, or when it ends a worker. Real transcripts hold
//! one result per call, so a line per result would double the feed and push useful lines out of
//! the face's six. Permission lines show only for a person's decision or a hook or rule's block;
//! classifier and config auto-accepts are noise.
//!
//! A worker is live from its meta file until the meta says `finished`, a task notification or a
//! foreground result for its Agent call lands, or the session ends. Async Agent calls return
//! `async_launched` at once, so that first result does not end the worker.
//!
//! A background task is a shell or Monitor that outlives its tool call. It is live from the result
//! that names it (`toolUseResult.backgroundTaskId`, or `taskId` for Monitor) until its task
//! notification, a TaskStop or KillShell call, a Monitor expiry notice, or the session ends. Tasks
//! left over after 30 minutes of transcript silence are dropped. A session reads as idle only when
//! no worker and no task is live and no turn is under way.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use erebus_protocol::{
    FeedEvent, FeedKind, Pending, SessionInfo, SessionStatus, TaskInfo, TaskKind, WorkerInfo, FEED_HISTORY, MAX_PHRASE,
};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::de::{Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use tokio::sync::mpsc::UnboundedSender;

use crate::hub::Event;
use crate::labels::Labels;
use crate::log::{log_info, log_warn};
use crate::server::now_ms;

const HARNESS: &str = "claude";

/// How long a pass waits after the first file change, gathering the rest of the burst.
pub(crate) const WINDOW: Duration = Duration::from_millis(250);

/// A pass runs at least this often.
pub(crate) const RESCAN: Duration = Duration::from_secs(2);

/// History read from a file the core sees for the first time.
pub(crate) const SEED_BYTES: u64 = 512 * 1024;

/// A file created this close to its session's start counts as new, not history.
const BIRTH_SLACK_MS: u64 = 5_000;

/// An ask becomes `pending` after this long unresolved. Auto mode settles most asks sooner.
const SETTLE_MS: u64 = 1_500;

/// A spool file that still fails to parse after this long is deleted.
const SPOOL_GIVE_UP_MS: u64 = 2_000;

/// Retry delay for a spool file that may be mid-write.
const SPOOL_RETRY: Duration = Duration::from_millis(250);

/// A tool that ran at least this long gets a `result` line.
pub(crate) const LONG_TOOL_MS: u64 = 10_000;

/// A live worker whose transcript has not grown for this long is dropped.
const STALE_WORKER_MS: u64 = 30 * 60 * 1000;

/// Resolved tool calls are remembered this long, so a late ask for one is ignored.
const RESOLVED_KEEP_MS: u64 = 60_000;

/// An ask for a session the core does not know is dropped after this long.
const ORPHAN_ASK_MS: u64 = 30_000;

/// Bytes of a line inspected for a leading `"type"` before any parse.
const PEEK: usize = 80;

/// Characters of a text block kept for prefix checks.
const SNIPPET: usize = 64;

/// Record types that can carry feed facts. Every other type is skipped unparsed.
const KEPT: &[&str] = &["user", "assistant", "system", "attachment"];

const INTERRUPT: &str = "[Request interrupted by user";
const DECLINED: &str = "The user doesn't want to proceed";

pub fn spawn(hub: UnboundedSender<Event>, labels: Labels) {
    let spawned = thread::Builder::new().name("sessions".into()).spawn(move || run(hub, labels));
    if let Err(e) = spawned {
        log_warn!("cannot start sessions service: {e}");
    }
}

fn run(hub: UnboundedSender<Event>, labels: Labels) {
    let (tx, rx) = mpsc::channel();
    let mut svc = Service::new(Paths::from_env(), labels);
    let mut watch = Watch { tx, w: None, watched: Vec::new(), warned: false };
    let mut window: Option<Instant> = None;
    let mut next_scan = Instant::now();
    loop {
        let mut due = window.map_or(next_scan, |w| w.min(next_scan));
        if let Some(w) = svc.wake {
            due = due.min(w);
        }
        let now = Instant::now();
        if due > now {
            match rx.recv_timeout(due - now) {
                Ok(()) => {
                    window.get_or_insert_with(|| Instant::now() + WINDOW);
                    continue;
                }
                Err(RecvTimeoutError::Timeout) => continue,
                // `watch` holds a sender, so this cannot happen; sleep rather than spin.
                Err(RecvTimeoutError::Disconnected) => thread::sleep(due - now),
            }
        }
        window = None;
        next_scan = Instant::now() + RESCAN;
        watch.ensure(&svc.paths);
        let (sessions, feed) = svc.pass(now_ms());
        if let Some(s) = sessions {
            if hub.send(Event::Sessions(HARNESS, s)).is_err() {
                return;
            }
        }
        if !feed.is_empty() && hub.send(Event::Feed(feed)).is_err() {
            return;
        }
    }
}

struct Paths {
    root: Option<PathBuf>,
    spool: Option<PathBuf>,
}

impl Paths {
    fn from_env() -> Paths {
        let dir = |var: &str| std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from);
        let root = dir("EREBUS_CLAUDE_DIR").or_else(|| dir("USERPROFILE").map(|p| p.join(".claude")));
        let spool = dir("EREBUS_SPOOL_DIR").or_else(|| dir("LOCALAPPDATA").map(|p| p.join("Erebus").join("spool")));
        Paths { root, spool }
    }

    fn sessions(&self) -> Option<PathBuf> {
        self.root.as_ref().map(|r| r.join("sessions"))
    }

    fn projects(&self) -> Option<PathBuf> {
        self.root.as_ref().map(|r| r.join("projects"))
    }
}

/// One watcher over the sessions, projects and spool folders. Folders that do not exist yet are
/// added on a later pass.
struct Watch {
    tx: mpsc::Sender<()>,
    w: Option<RecommendedWatcher>,
    watched: Vec<PathBuf>,
    warned: bool,
}

impl Watch {
    fn ensure(&mut self, paths: &Paths) {
        let want = [
            (paths.sessions(), RecursiveMode::NonRecursive),
            (paths.projects(), RecursiveMode::Recursive),
            (paths.spool.clone(), RecursiveMode::NonRecursive),
        ];
        for (dir, mode) in want {
            let Some(dir) = dir else { continue };
            if self.watched.contains(&dir) || !dir.is_dir() {
                continue;
            }
            if self.w.is_none() {
                let tx = self.tx.clone();
                let made = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                    if let Ok(ev) = res {
                        if !matches!(ev.kind, EventKind::Access(_)) && ev.paths.iter().any(|p| matters(p)) {
                            let _ = tx.send(());
                        }
                    }
                });
                match made {
                    Ok(w) => self.w = Some(w),
                    Err(e) => {
                        if !std::mem::replace(&mut self.warned, true) {
                            log_warn!("sessions watcher unavailable: {e}; rescanning every 2 s");
                        }
                        return;
                    }
                }
            }
            let Some(w) = self.w.as_mut() else { return };
            match w.watch(&dir, mode) {
                Ok(()) => {
                    log_info!("sessions: watching {}", dir.display());
                    self.watched.push(dir);
                }
                Err(e) => {
                    if !std::mem::replace(&mut self.warned, true) {
                        log_warn!("cannot watch {}: {e}", dir.display());
                    }
                }
            }
        }
    }
}

/// Only transcript, meta, session and spool files wake the service.
fn matters(p: &Path) -> bool {
    matches!(p.extension().and_then(|e| e.to_str()), Some("json" | "jsonl"))
}

/// How a batch of lines reaches the feed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// History read when the core first saw the file.
    Backlog,
    Live,
    /// Re-read after a truncation: state changes, no feed lines.
    Quiet,
}

/// Feed events gathered during one pass.
#[derive(Default)]
pub(crate) struct Out {
    backlog: Vec<FeedEvent>,
    live: Vec<FeedEvent>,
}

impl Out {
    pub(crate) fn push(&mut self, mode: Mode, event: FeedEvent) {
        match mode {
            Mode::Backlog => self.backlog.push(event),
            Mode::Live => self.live.push(event),
            Mode::Quiet => {}
        }
    }

    /// Backlog first, trimmed to what the hub's ring can hold, then live events. Each part is
    /// sorted by time; a stable sort keeps file order within a millisecond.
    pub(crate) fn finish(mut self) -> Vec<FeedEvent> {
        self.backlog.sort_by_key(|e| e.unix_ms);
        let cut = self.backlog.len().saturating_sub(FEED_HISTORY);
        self.backlog.drain(..cut);
        self.live.sort_by_key(|e| e.unix_ms);
        self.backlog.extend(self.live);
        self.backlog
    }
}

struct Service {
    paths: Paths,
    /// Live sessions in start order.
    sessions: Vec<Session>,
    asks: Asks,
    /// Spool files that failed to parse, with when that first happened.
    unreadable: HashMap<PathBuf, u64>,
    first: bool,
    /// Whether the sessions folder existed on the last pass, for logging changes only.
    root_seen: Option<bool>,
    spool_ready: bool,
    spool_warned: bool,
    sent: Option<Vec<SessionInfo>>,
    counts: (usize, usize),
    /// When the next pass must run regardless of file changes.
    wake: Option<Instant>,
    labels: Labels,
}

impl Service {
    fn new(paths: Paths, labels: Labels) -> Service {
        Service {
            paths,
            labels,
            sessions: Vec::new(),
            asks: Asks::default(),
            unreadable: HashMap::new(),
            first: true,
            root_seen: None,
            spool_ready: false,
            spool_warned: false,
            sent: None,
            counts: (0, 0),
            wake: None,
        }
    }

    /// Returns a snapshot when it differs from the last one sent, and the pass's feed events.
    fn pass(&mut self, now: u64) -> (Option<Vec<SessionInfo>>, Vec<FeedEvent>) {
        let mut out = Out::default();
        let first = std::mem::replace(&mut self.first, false);
        self.reconcile(first, now, &mut out);
        self.read_spool(now, &mut out);
        if let Some(projects) = self.paths.projects() {
            for s in &mut self.sessions {
                s.follow(&projects, now, &mut self.asks, &mut out);
            }
        }
        let live: HashSet<String> = self.sessions.iter().map(|s| s.id.clone()).collect();
        self.asks.prune(now, &live);

        let mut infos: Vec<SessionInfo> =
            self.sessions.iter().map(|s| s.info(self.asks.pending(&s.id, now))).collect();
        infos.sort_by(|a, b| (a.started_unix_ms, &a.label).cmp(&(b.started_unix_ms, &b.label)));
        let counts = (infos.len(), infos.iter().map(|s| s.workers.len()).sum());
        if counts != self.counts {
            log_info!("sessions: {} live, {} workers", counts.0, counts.1);
            self.counts = counts;
        }

        let settle = self.asks.next_settle(now).map(|ms| Instant::now() + Duration::from_millis(ms - now));
        let retry = (!self.unreadable.is_empty()).then(|| Instant::now() + SPOOL_RETRY);
        self.wake = match (settle, retry) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };

        let changed = self.sent.as_ref() != Some(&infos);
        if changed {
            self.sent = Some(infos.clone());
        }
        (changed.then_some(infos), out.finish())
    }

    /// Ends sessions that are gone or re-keyed and starts new ones, oldest first.
    fn reconcile(&mut self, first: bool, now: u64, out: &mut Out) {
        let dir = self.paths.sessions();
        let listing = dir.as_deref().and_then(list_sessions);
        let seen = listing.is_some();
        if self.root_seen != Some(seen) {
            let shown = dir.as_ref().map_or("~/.claude".to_owned(), |d| d.display().to_string());
            if seen {
                log_info!("sessions: reading {shown}");
            } else {
                log_info!("sessions: no {shown}; idle until it appears");
            }
            self.root_seen = Some(seen);
        }
        let (found, unsure) = listing.unwrap_or_default();

        let mut kept = Vec::new();
        let mut relabel = HashMap::new();
        for s in self.sessions.drain(..) {
            let same = found.iter().find(|f| f.pid == s.pid).map(|f| f.session_id == s.id);
            if same == Some(true) || (same.is_none() && unsure.contains(&s.pid)) {
                kept.push(s);
                continue;
            }
            s.emit(out, Mode::Live, now, FeedKind::Session, None, "ended");
            let st = &s.stats;
            log_info!(
                "session ended: {} pid {}; {} lines, {} skipped, {} malformed",
                s.label, s.pid, st.lines, st.skipped, st.bad
            );
            if same == Some(false) {
                relabel.insert(s.pid, s.label);
            }
        }
        self.sessions = kept;

        let mut fresh: Vec<&SessionFile> = found.iter().filter(|f| !self.sessions.iter().any(|s| s.pid == f.pid)).collect();
        fresh.sort_by_key(|f| (f.started_ms(), f.pid));
        for f in fresh {
            let label = relabel.remove(&f.pid).unwrap_or_else(|| {
                let others = self.labels.others(HARNESS);
                let taken: Vec<&str> = self.sessions.iter().map(|s| s.label.as_str()).chain(others.iter().map(String::as_str)).collect();
                allocate(&base_label(f.cwd.as_deref().unwrap_or(""), "claude"), &taken)
            });
            let s = Session::new(f, label, !first, now);
            let mode = if first { Mode::Backlog } else { Mode::Live };
            s.emit(out, mode, s.started_ms, FeedKind::Session, None, "started");
            log_info!("session started: {} pid {}", s.label, s.pid);
            self.sessions.push(s);
            self.labels.set(HARNESS, self.sessions.iter().map(|s| s.label.clone()).collect());
        }
        self.labels.set(HARNESS, self.sessions.iter().map(|s| s.label.clone()).collect());

        for s in &mut self.sessions {
            if let Some(f) = found.iter().find(|f| f.pid == s.pid) {
                s.status = status_of_str(f.status.as_deref());
            }
        }
    }

    /// Reads every spool file once, deleting it after. Asks are applied before dones.
    fn read_spool(&mut self, now: u64, out: &mut Out) {
        let Some(dir) = self.paths.spool.clone() else { return };
        if !self.spool_ready {
            match fs::create_dir_all(&dir) {
                Ok(()) => self.spool_ready = true,
                Err(e) => {
                    if !std::mem::replace(&mut self.spool_warned, true) {
                        log_warn!("cannot create spool {}: {e}", dir.display());
                    }
                    return;
                }
            }
        }
        let Ok(entries) = fs::read_dir(&dir) else { return };
        let mut recs = Vec::new();
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            match fs::read(&path).ok().and_then(|b| serde_json::from_slice::<SpoolRec>(&b).ok()) {
                Some(r) => {
                    let _ = fs::remove_file(&path);
                    self.unreadable.remove(&path);
                    recs.push(r);
                }
                None => {
                    let since = *self.unreadable.entry(path.clone()).or_insert(now);
                    if now.saturating_sub(since) >= SPOOL_GIVE_UP_MS {
                        let _ = fs::remove_file(&path);
                        self.unreadable.remove(&path);
                        log_warn!("spool: dropped unreadable {}", e.file_name().to_string_lossy());
                    }
                }
            }
        }
        self.unreadable.retain(|p, _| p.exists());

        recs.sort_by_key(|r| (r.kind != "ask", r.unix_ms));
        for r in recs {
            if r.v != 1 {
                continue;
            }
            match r.kind.as_str() {
                "ask" => {
                    let ask = Ask {
                        session: r.session,
                        tool: r.tool.unwrap_or_else(|| "tool".into()),
                        agent: r.agent,
                        unix_ms: r.unix_ms,
                        seen: now,
                        shown: false,
                    };
                    self.asks.ask(r.tool_use_id, ask);
                }
                "done" => {
                    let Some(a) = self.asks.resolve(&r.tool_use_id, now) else { continue };
                    if !a.shown {
                        continue;
                    }
                    let Some(s) = self.sessions.iter_mut().find(|s| s.id == a.session) else { continue };
                    let word = if r.outcome.as_deref() == Some("denied") { "denied" } else { "granted" };
                    s.perm(&r.tool_use_id, a.agent.as_deref(), &format!("{word} {}", a.tool), now, Mode::Live, out);
                }
                _ => {}
            }
        }
    }
}

/// Live sessions from `sessions\*.json`, and the pids whose file could not be read this time.
/// `None` when the folder is missing.
fn list_sessions(dir: &Path) -> Option<(Vec<SessionFile>, HashSet<u32>)> {
    let entries = fs::read_dir(dir).ok()?;
    let mut found = Vec::new();
    let mut unsure = HashSet::new();
    for e in entries.flatten() {
        let path = e.path();
        // Only `<pid>.json`; `.key` files and anything else are never opened.
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let parsed = fs::read(&path).ok().and_then(|b| serde_json::from_slice::<SessionFile>(&b).ok());
        let Some(f) = parsed else {
            if let Some(pid) = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse().ok()) {
                unsure.insert(pid);
            }
            continue;
        };
        let proc_start = f.proc_start.as_deref().and_then(|p| p.trim().parse::<u64>().ok());
        if safe_id(&f.session_id) && alive(f.pid, proc_start) {
            found.push(f);
        }
    }
    Some((found, unsure))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionFile {
    pid: u32,
    session_id: String,
    #[serde(default, deserialize_with = "loose_str")]
    cwd: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    started_at: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    proc_start: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    status: Option<String>,
}

impl SessionFile {
    fn started_ms(&self) -> u64 {
        self.started_at.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0)
    }
}

/// `shell` means the session waits on an outside program it started.
fn status_of_str(s: Option<&str>) -> SessionStatus {
    match s {
        Some("busy") => SessionStatus::Busy,
        Some("shell") => SessionStatus::Waiting,
        _ => SessionStatus::Idle,
    }
}

/// Session and agent ids become path components, so only plain ids are accepted.
pub(crate) fn safe_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// True when `pid` runs and, if known, was created at `proc_start` (a FILETIME).
fn alive(pid: u32, proc_start: Option<u64>) -> bool {
    creation_time(pid).is_some_and(|t| proc_start.is_none_or(|p| p == t))
}

/// Creation time of a running process as a FILETIME, or `None` when it is gone or unreadable.
fn creation_time(pid: u32) -> Option<u64> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const STILL_ACTIVE: u32 = 259;
    // SAFETY: plain Win32 calls; every out-pointer is a local and the handle is closed once.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        let mut code = 0u32;
        let ok = GetProcessTimes(h, &mut created, &mut exited, &mut kernel, &mut user) != 0
            && GetExitCodeProcess(h, &mut code) != 0;
        CloseHandle(h);
        (ok && code == STILL_ACTIVE).then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }
}

#[derive(Default)]
struct Stats {
    lines: u64,
    skipped: u64,
    bad: u64,
}

/// A tool call waiting for its result.
struct Call {
    tool: String,
    ms: u64,
    /// The task a TaskStop or KillShell call targets.
    stop: Option<String>,
    /// What the call runs, for a background task's name.
    label: String,
}

/// A background shell or monitor, live from the result that names it.
#[derive(Clone, Debug)]
struct Task {
    task_id: String,
    kind: TaskKind,
    label: String,
    started_ms: u64,
}

struct Agent {
    /// Spawn order within the session, from 1; 0 for agents that were never live.
    n: u32,
    kind: String,
    started_ms: u64,
    tool_use_id: Option<String>,
    live: bool,
    tail: Option<Tail>,
    meta: PathBuf,
    meta_mtime: Option<SystemTime>,
}

struct Session {
    pid: u32,
    id: String,
    cwd: String,
    label: String,
    status: SessionStatus,
    started_ms: u64,
    /// Started while the core was already watching, so its files are new.
    born_live: bool,
    transcript: Option<Tail>,
    looked: bool,
    /// The transcript was read as history, so workers found on the first look are history too.
    history: bool,
    agents: HashMap<String, Agent>,
    agents_looked: bool,
    next_worker: u32,
    calls: HashMap<String, Call>,
    /// Agent calls that finished before their worker was seen.
    ended_spawns: HashSet<String>,
    /// Live background tasks of the main transcript, by the tool call that started each.
    tasks: HashMap<String, Task>,
    /// Tool calls that already have a perm line.
    perms: HashSet<String>,
    /// The main transcript has shown activity since its last turn end.
    mid_turn: bool,
    /// The last feed line about the session's turn state was `idle`.
    idle_shown: bool,
    stats: Stats,
}

impl Session {
    fn new(f: &SessionFile, label: String, born_live: bool, now: u64) -> Session {
        let started = f.started_ms();
        Session {
            pid: f.pid,
            id: f.session_id.clone(),
            cwd: f.cwd.clone().unwrap_or_default(),
            label,
            status: status_of_str(f.status.as_deref()),
            started_ms: if started == 0 { now } else { started },
            born_live,
            transcript: None,
            looked: false,
            history: false,
            agents: HashMap::new(),
            agents_looked: false,
            next_worker: 0,
            calls: HashMap::new(),
            ended_spawns: HashSet::new(),
            tasks: HashMap::new(),
            perms: HashSet::new(),
            mid_turn: false,
            idle_shown: false,
            stats: Stats::default(),
        }
    }

    /// `<label>` for the session itself, `<label>·w<N>` for one of its workers.
    fn who(&self, agent: Option<&str>) -> String {
        match agent.and_then(|id| self.agents.get(id)) {
            Some(a) => format!("{}·w{}", self.label, a.n),
            None => self.label.clone(),
        }
    }

    fn emit(&self, out: &mut Out, mode: Mode, ms: u64, kind: FeedKind, agent: Option<&str>, what: &str) {
        let event = FeedEvent {
            unix_ms: ms,
            kind,
            phrase: phrase(&self.who(agent), what),
            session: self.id.clone(),
            worker: agent.map(str::to_owned),
            backlog: mode == Mode::Backlog,
        };
        out.push(mode, event);
    }

    /// One perm line per tool call, whichever source reports it first.
    fn perm(&mut self, tool_use_id: &str, agent: Option<&str>, what: &str, ms: u64, mode: Mode, out: &mut Out) {
        if self.perms.insert(tool_use_id.to_owned()) {
            self.emit(out, mode, ms, FeedKind::Perm, agent, what);
        }
    }

    /// Whether a file that existed when the core first looked was created by this session's
    /// own run, rather than a run the session resumed.
    fn own(&self, created_ms: u64) -> bool {
        self.born_live && created_ms + BIRTH_SLACK_MS >= self.started_ms
    }

    fn info(&self, pending: Option<Pending>) -> SessionInfo {
        let mut workers: Vec<(&String, &Agent)> = self.agents.iter().filter(|(_, a)| a.live).collect();
        workers.sort_by_key(|(_, a)| a.n);
        SessionInfo {
            id: self.id.clone(),
            label: self.label.clone(),
            cwd: self.cwd.clone(),
            harness: HARNESS.into(),
            headless: false,
            status: self.status,
            started_unix_ms: self.started_ms,
            workers: workers
                .into_iter()
                .map(|(id, a)| WorkerInfo {
                    id: id.clone(),
                    label: format!("w{}", a.n),
                    kind: a.kind.clone(),
                    started_unix_ms: a.started_ms,
                })
                .collect(),
            pending,
            tasks: {
                let mut t: Vec<TaskInfo> = self
                    .tasks
                    .iter()
                    .map(|(id, t)| TaskInfo { id: id.clone(), kind: t.kind, label: t.label.clone(), started_unix_ms: t.started_ms })
                    .collect();
                t.sort_by(|a, b| (a.started_unix_ms, &a.id).cmp(&(b.started_unix_ms, &b.id)));
                t
            },
        }
    }

    fn follow(&mut self, projects: &Path, now: u64, asks: &mut Asks, out: &mut Out) {
        let first_look = !std::mem::replace(&mut self.looked, true);
        if self.transcript.is_none() {
            if let Some(path) = find_transcript(projects, &self.cwd, &self.id) {
                let seed = first_look && !self.own(created_ms(&path).unwrap_or(now));
                self.history = seed;
                self.transcript = Some(Tail::open(path, seed));
            }
        }
        if let Some(mut tail) = self.transcript.take() {
            let (block, mode) = tail.read(now);
            let facts = scan(&mut tail, &block, now, &mut self.stats);
            let quiet_since = if tail.last_ms > 0 { tail.last_ms } else { tail.grew_ms };
            self.transcript = Some(tail);
            for (fact, ms) in facts {
                self.apply(None, fact, ms, mode, asks, out);
            }
            if !self.tasks.is_empty() && now.saturating_sub(quiet_since) > STALE_WORKER_MS {
                self.tasks.clear();
                let at = if mode == Mode::Backlog { quiet_since } else { now };
                self.settle(out, mode, at);
            }
        }
        self.follow_agents(now, asks, out);
    }

    fn follow_agents(&mut self, now: u64, asks: &mut Asks, out: &mut Out) {
        let Some(dir) = self.transcript.as_ref().map(|t| t.path.with_extension("").join("subagents")) else { return };
        let first_look = !std::mem::replace(&mut self.agents_looked, true);
        if dir.is_dir() {
            self.discover(&dir, first_look && self.history, now);
        }
        let mut ids: Vec<String> = self.agents.iter().filter(|(_, a)| a.live).map(|(id, _)| id.clone()).collect();
        ids.sort();
        for id in ids {
            let Some(a) = self.agents.get_mut(&id) else { continue };
            let mtime = fs::metadata(&a.meta).and_then(|m| m.modified()).ok();
            let finished = mtime != a.meta_mtime && {
                a.meta_mtime = mtime;
                read_meta(&a.meta).is_some_and(|m| m.finished == Some(true))
            };
            let Some(mut tail) = a.tail.take() else { continue };
            let (block, mode) = tail.read(now);
            let facts = scan(&mut tail, &block, now, &mut self.stats);
            let stale = now.saturating_sub(tail.grew_ms) > STALE_WORKER_MS;
            if let Some(a) = self.agents.get_mut(&id) {
                a.tail = Some(tail);
            }
            for (fact, ms) in facts {
                self.apply(Some(&id), fact, ms, mode, asks, out);
            }
            if finished {
                self.end_agent(&id, "completed", now, Mode::Live, out);
            } else if stale {
                self.end_agent(&id, "stale", now, Mode::Quiet, out);
            }
        }
    }

    /// Registers agents whose meta file is new. Finished, stale and already-ended ones are
    /// remembered as dead so they are not read again. With `history`, agents found now predate
    /// the core.
    fn discover(&mut self, dir: &Path, history: bool, now: u64) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        let mut found = Vec::new();
        for e in entries.flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(id) = name.strip_prefix("agent-").and_then(|n| n.strip_suffix(".meta.json")) else { continue };
            if !safe_id(id) || self.agents.contains_key(id) {
                continue;
            }
            let meta_path = dir.join(name);
            // A meta file mid-write fails to parse; the next pass tries again.
            let Some(meta) = read_meta(&meta_path) else { continue };
            let log = dir.join(format!("agent-{id}.jsonl"));
            let modified = mtime_ms(&log).or_else(|| mtime_ms(&meta_path)).unwrap_or(now);
            let created = created_ms(&meta_path).unwrap_or(now);
            let ended = meta.finished == Some(true)
                || (history && now.saturating_sub(modified) > STALE_WORKER_MS)
                || meta.tool_use_id.as_ref().is_some_and(|t| self.ended_spawns.contains(t));
            found.push((id.to_owned(), meta, meta_path, log, created, ended));
        }
        found.sort_by(|a, b| (a.4, &a.0).cmp(&(b.4, &b.0)));
        for (id, meta, meta_path, log, created, ended) in found {
            let meta_mtime = fs::metadata(&meta_path).and_then(|m| m.modified()).ok();
            let mut agent = Agent {
                n: 0,
                kind: meta.agent_type.unwrap_or_else(|| "agent".into()),
                started_ms: created,
                tool_use_id: meta.tool_use_id,
                live: false,
                tail: None,
                meta: meta_path,
                meta_mtime,
            };
            if !ended {
                self.next_worker += 1;
                agent.n = self.next_worker;
                agent.live = true;
                let seed = (history && !self.own(created_ms(&log).unwrap_or(created)))
                    || file_len(&log).unwrap_or(0) > SEED_BYTES;
                let mut tail = Tail::open(log, seed);
                tail.grew_ms = mtime_ms(&tail.path).unwrap_or(now);
                agent.tail = Some(tail);
                log_info!("worker {}·w{} started", self.label, agent.n);
            }
            self.agents.insert(id, agent);
        }
    }

    fn end_agent(&mut self, id: &str, status: &str, ms: u64, mode: Mode, out: &mut Out) {
        let Some(a) = self.agents.get_mut(id) else { return };
        if !a.live {
            return;
        }
        a.live = false;
        a.tail = None;
        let n = a.n;
        let (kind, what) = match status {
            "completed" => (FeedKind::Result, "done"),
            "failed" => (FeedKind::Error, "failed"),
            _ => (FeedKind::Error, "stopped"),
        };
        self.emit(out, mode, ms, kind, Some(id), what);
        log_info!("worker {}·w{n} ended ({status})", self.label);
        self.settle(out, mode, ms);
    }

    /// Reads as idle once no turn, worker or background task is left.
    fn settle(&mut self, out: &mut Out, mode: Mode, ms: u64) {
        if !self.mid_turn && self.tasks.is_empty() && !self.agents.values().any(|a| a.live) {
            self.go_idle(out, mode, ms);
        }
    }

    /// Ends the background task started by tool call `tool_use_id`. False when none was live.
    fn end_task(&mut self, tool_use_id: &str, ms: u64, mode: Mode, out: &mut Out) -> bool {
        let ended = self.tasks.remove(tool_use_id).is_some();
        if ended {
            self.settle(out, mode, ms);
        }
        ended
    }

    /// Ends the background task with id `task_id`.
    fn end_task_id(&mut self, task_id: &str, ms: u64, mode: Mode, out: &mut Out) {
        let before = self.tasks.len();
        self.tasks.retain(|_, t| t.task_id != task_id);
        if self.tasks.len() != before {
            self.settle(out, mode, ms);
        }
    }

    /// One `idle` line per quiet spell.
    fn go_idle(&mut self, out: &mut Out, mode: Mode, ms: u64) {
        if !std::mem::replace(&mut self.idle_shown, true) {
            self.emit(out, mode, ms, FeedKind::Turn, None, "idle");
        }
    }

    /// The main transcript closed a turn. Workers, background agents and background tasks still
    /// running keep the session from reading as idle. The pending count covers agents only.
    fn turn_ended(&mut self, pending: u32, ms: u64, mode: Mode, out: &mut Out) {
        self.mid_turn = false;
        let live = (self.agents.values().filter(|a| a.live).count() + self.tasks.len()) as u32;
        let running = live.max(pending);
        if running == 0 {
            self.go_idle(out, mode, ms);
        } else {
            self.idle_shown = false;
            self.emit(out, mode, ms, FeedKind::Turn, None, &format!("turn done · {running} running"));
        }
    }

    /// Ends the worker started by Agent call `tool_use_id`, or remembers that it ended.
    fn finish_spawn(&mut self, tool_use_id: &str, status: &str, ms: u64, mode: Mode, out: &mut Out) {
        let id = self
            .agents
            .iter()
            .find(|(_, a)| a.live && a.tool_use_id.as_deref() == Some(tool_use_id))
            .map(|(id, _)| id.clone());
        match id {
            Some(id) => self.end_agent(&id, status, ms, mode, out),
            None => {
                self.ended_spawns.insert(tool_use_id.to_owned());
            }
        }
    }

    fn apply(&mut self, agent: Option<&str>, fact: Fact, ms: u64, mode: Mode, asks: &mut Asks, out: &mut Out) {
        match fact {
            Fact::Active => {
                if agent.is_none() {
                    self.mid_turn = true;
                    self.idle_shown = false;
                }
            }
            Fact::TurnEnded { pending } => {
                if agent.is_none() {
                    self.turn_ended(pending, ms, mode, out);
                }
            }
            Fact::Call { id, tool, act, stop, desc } => {
                let label = match (&act, desc) {
                    (_, Some(d)) if tool == "Monitor" => clip(&d, 24),
                    (Act::Tool(what), _) => what.strip_prefix(tool.as_str()).map_or(what.as_str(), str::trim).to_owned(),
                    _ => tool.clone(),
                };
                let label = if label.is_empty() { tool.clone() } else { label };
                self.calls.insert(id, Call { tool, ms, stop, label });
                match act {
                    Act::Tool(what) => self.emit(out, mode, ms, FeedKind::Tool, agent, &what),
                    Act::Spawn(kind) => self.emit(out, mode, ms, FeedKind::Spawn, agent, &format!("spawned {kind}")),
                    Act::Skill(name) => self.emit(out, mode, ms, FeedKind::Skill, agent, &format!("/{name}")),
                }
            }
            Fact::Return { id, error, declined, decision, launched, bg_task, monitor_task } => {
                asks.resolve(&id, ms);
                let call = self.calls.remove(&id);
                let tool = call.as_ref().map_or("tool", |c| c.tool.as_str()).to_owned();
                let label = call.as_ref().map_or_else(|| tool.clone(), |c| c.label.clone());
                let started = call.as_ref().map_or(ms, |c| c.ms);
                let stop = call.as_ref().and_then(|c| c.stop.clone());
                if let Some(d) = decision {
                    let person = d.source.starts_with("user");
                    let word = match (d.decision.as_str(), person) {
                        ("accept", true) => Some("granted"),
                        ("reject", true) => Some("denied"),
                        ("reject", false) => Some("blocked"),
                        _ => None,
                    };
                    if let Some(w) = word {
                        self.perm(&id, agent, &format!("{w} {tool}"), ms, mode, out);
                    }
                    if d.decision == "reject" {
                        return;
                    }
                }
                if declined {
                    return;
                }
                if is_agent(&tool) {
                    if !launched {
                        self.finish_spawn(&id, if error { "failed" } else { "completed" }, ms, mode, out);
                    }
                    return;
                }
                if agent.is_none() {
                    if let Some(task) = stop {
                        self.end_task_id(&task, ms, mode, out);
                    } else if let Some(task) = bg_task.or_else(|| monitor_task.filter(|_| tool == "Monitor")) {
                        if !error {
                            let kind = if tool == "Monitor" { TaskKind::Monitor } else { TaskKind::Shell };
                            self.tasks.insert(id, Task { task_id: task, kind, label, started_ms: started });
                        }
                    }
                }
                if error {
                    self.emit(out, mode, ms, FeedKind::Error, agent, &format!("{tool} failed"));
                } else if runs(&tool) && call.is_some_and(|c| ms.saturating_sub(c.ms) >= LONG_TOOL_MS) {
                    self.emit(out, mode, ms, FeedKind::Result, agent, &format!("{tool} ok"));
                }
            }
            Fact::Aborted | Fact::Interrupted => self.emit(out, mode, ms, FeedKind::Error, agent, "interrupted"),
            Fact::Compacted => self.emit(out, mode, ms, FeedKind::Compact, agent, "compacted"),
            Fact::TaskEnded { tool_use_id, status } => {
                if agent.is_some() || !self.end_task(&tool_use_id, ms, mode, out) {
                    self.finish_spawn(&tool_use_id, &status, ms, mode, out);
                }
            }
            Fact::TaskGone { task_id } => {
                if agent.is_none() {
                    self.end_task_id(&task_id, ms, mode, out);
                }
            }
        }
    }
}

fn is_agent(tool: &str) -> bool {
    tool == "Agent" || tool == "Task"
}

/// Tools whose own run can be long. Parallel calls write their results together when the
/// slowest ends, so a file edit beside a slow command would otherwise read as long.
fn runs(tool: &str) -> bool {
    matches!(tool, "Bash" | "PowerShell" | "WebFetch" | "WebSearch") || tool.starts_with("mcp__")
}

/// The transcript for session `id`: under the cwd's slug, else under any project folder.
fn find_transcript(projects: &Path, cwd: &str, id: &str) -> Option<PathBuf> {
    let name = format!("{id}.jsonl");
    let guess = projects.join(slug(cwd)).join(&name);
    if guess.is_file() {
        return Some(guess);
    }
    fs::read_dir(projects).ok()?.flatten().map(|e| e.path().join(&name)).find(|p| p.is_file())
}

/// Claude Code's project folder name for a cwd: every character but ASCII letters and digits
/// becomes `-`.
pub fn slug(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

pub(crate) fn file_len(p: &Path) -> Option<u64> {
    fs::metadata(p).ok().map(|m| m.len())
}

fn to_ms(t: SystemTime) -> Option<u64> {
    t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_millis() as u64)
}

fn mtime_ms(p: &Path) -> Option<u64> {
    fs::metadata(p).and_then(|m| m.modified()).ok().and_then(to_ms)
}

pub(crate) fn created_ms(p: &Path) -> Option<u64> {
    fs::metadata(p).and_then(|m| m.created()).ok().and_then(to_ms)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetaFile {
    #[serde(default, deserialize_with = "loose_str")]
    agent_type: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    tool_use_id: Option<String>,
    #[serde(default, deserialize_with = "loose_bool")]
    finished: Option<bool>,
}

fn read_meta(p: &Path) -> Option<MetaFile> {
    serde_json::from_slice(&fs::read(p).ok()?).ok()
}

/// A permission ask waiting on its outcome.
struct Ask {
    session: String,
    tool: String,
    agent: Option<String>,
    unix_ms: u64,
    /// When the core read it.
    seen: u64,
    /// Whether it has been reported as `pending`.
    shown: bool,
}

#[derive(Default)]
struct Asks {
    open: HashMap<String, Ask>,
    /// Tool calls already resolved, with when.
    resolved: HashMap<String, u64>,
}

impl Asks {
    fn ask(&mut self, tool_use_id: String, ask: Ask) {
        if !self.resolved.contains_key(&tool_use_id) {
            self.open.entry(tool_use_id).or_insert(ask);
        }
    }

    fn resolve(&mut self, tool_use_id: &str, ms: u64) -> Option<Ask> {
        self.resolved.insert(tool_use_id.to_owned(), ms);
        self.open.remove(tool_use_id)
    }

    /// The oldest settled ask for `session`. Marks every settled ask as shown.
    fn pending(&mut self, session: &str, now: u64) -> Option<Pending> {
        let mut best: Option<(u64, &String)> = None;
        for (id, a) in self.open.iter_mut().filter(|(_, a)| a.session == session) {
            if now < a.unix_ms + SETTLE_MS {
                continue;
            }
            a.shown = true;
            if best.is_none_or(|b| (a.unix_ms, id) < b) {
                best = Some((a.unix_ms, id));
            }
        }
        let id = best?.1.clone();
        let a = &self.open[&id];
        Some(Pending { tool_use_id: id, tool: a.tool.clone(), worker: a.agent.clone(), since_unix_ms: a.unix_ms })
    }

    /// When the next unsettled ask settles, if one does in the future.
    fn next_settle(&self, now: u64) -> Option<u64> {
        self.open.values().map(|a| a.unix_ms + SETTLE_MS).filter(|t| *t > now).min()
    }

    fn prune(&mut self, now: u64, live: &HashSet<String>) {
        self.resolved.retain(|_, t| now.saturating_sub(*t) < RESOLVED_KEEP_MS);
        self.open.retain(|_, a| live.contains(&a.session) || now.saturating_sub(a.seen) < ORPHAN_ASK_MS);
    }
}

#[derive(Deserialize)]
struct SpoolRec {
    v: u32,
    kind: String,
    unix_ms: u64,
    session: String,
    tool_use_id: String,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    outcome: Option<String>,
}

/// Reads a file from a byte offset, one batch of complete lines at a time.
pub(crate) struct Tail {
    path: PathBuf,
    offset: u64,
    lines: LineBuf,
    /// How the next read reports; `Backlog` for a seeded file's first read.
    next: Mode,
    /// The last user or assistant record was an aborted reply.
    aborted: bool,
    last_ms: u64,
    /// When the file last grew.
    grew_ms: u64,
}

impl Tail {
    /// Starts at the last [`SEED_BYTES`] as backlog when `seed`, else at the start as live.
    pub(crate) fn open(path: PathBuf, seed: bool) -> Tail {
        let (offset, next) = if seed { (file_len(&path).unwrap_or(0).saturating_sub(SEED_BYTES), Mode::Backlog) } else { (0, Mode::Live) };
        let lines = LineBuf { partial: Vec::new(), skip: offset > 0 };
        Tail { path, offset, lines, next, aborted: false, last_ms: 0, grew_ms: 0 }
    }

    #[cfg(test)]
    fn detached() -> Tail {
        Tail::open(PathBuf::new(), false)
    }

    /// New complete lines and how to report them. A file shorter than the offset was truncated
    /// or replaced: it is read again from the start without feed lines.
    pub(crate) fn read(&mut self, now: u64) -> (Vec<u8>, Mode) {
        let Ok(mut f) = File::open(&self.path) else { return (Vec::new(), Mode::Live) };
        let Ok(len) = f.metadata().map(|m| m.len()) else { return (Vec::new(), Mode::Live) };
        let mut mode = self.next;
        if len < self.offset {
            self.offset = 0;
            self.lines = LineBuf::default();
            self.aborted = false;
            mode = Mode::Quiet;
        }
        if len == self.offset {
            return (Vec::new(), Mode::Live);
        }
        let mut buf = Vec::new();
        if f.seek(SeekFrom::Start(self.offset)).is_err() || f.take(len - self.offset).read_to_end(&mut buf).is_err() {
            return (Vec::new(), Mode::Live);
        }
        self.offset += buf.len() as u64;
        self.grew_ms = now;
        self.next = Mode::Live;
        (self.lines.push(&buf), mode)
    }
}

/// Splits a byte stream into whole lines, holding back a trailing line without `\n`.
#[derive(Default)]
pub(crate) struct LineBuf {
    partial: Vec<u8>,
    /// Drop everything up to the first `\n`: reading started mid-line.
    skip: bool,
}

impl LineBuf {
    /// The complete lines now available, `\n`-terminated, as one block.
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut chunk = chunk;
        if self.skip {
            match chunk.iter().position(|b| *b == b'\n') {
                Some(i) => {
                    chunk = &chunk[i + 1..];
                    self.skip = false;
                }
                None => return Vec::new(),
            }
        }
        self.partial.extend_from_slice(chunk);
        match self.partial.iter().rposition(|b| *b == b'\n') {
            Some(i) => {
                let rest = self.partial.split_off(i + 1);
                std::mem::replace(&mut self.partial, rest)
            }
            None => Vec::new(),
        }
    }
}

/// Facts from a block of lines, each with its record time.
fn scan(tail: &mut Tail, block: &[u8], now: u64, stats: &mut Stats) -> Vec<(Fact, u64)> {
    let mut out = Vec::new();
    for line in block.split(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        stats.lines += 1;
        match parse_line(line) {
            Line::Skip => stats.skipped += 1,
            Line::Bad => stats.bad += 1,
            Line::Kept { turn, ms, facts } => {
                if let Some(ms) = ms {
                    tail.last_ms = ms;
                }
                let ms = ms.unwrap_or(if tail.last_ms > 0 { tail.last_ms } else { now });
                let after_abort = tail.aborted;
                if turn {
                    tail.aborted = facts.contains(&Fact::Aborted);
                }
                // An aborted reply is followed by an interrupt marker; one line covers both.
                if turn {
                    out.push((Fact::Active, ms));
                }
                out.extend(facts.into_iter().filter(|f| !(after_abort && *f == Fact::Interrupted)).map(|f| (f, ms)));
            }
        }
    }
    out
}

/// What one transcript line reports.
#[derive(Clone, Debug, PartialEq)]
enum Fact {
    /// `desc` is the call's own description, kept for a Monitor's name.
    Call { id: String, tool: String, act: Act, stop: Option<String>, desc: Option<String> },
    /// `bg_task` and `monitor_task` are the task ids a result names, if any.
    Return {
        id: String,
        error: bool,
        declined: bool,
        decision: Option<Verdict>,
        launched: bool,
        bg_task: Option<String>,
        monitor_task: Option<String>,
    },
    /// The model's reply was cut off mid-stream.
    Aborted,
    /// The person interrupted the turn.
    Interrupted,
    Compacted,
    /// A user or assistant record: the turn is under way.
    Active,
    /// The turn closed, with the count of background agents still pending.
    TurnEnded { pending: u32 },
    /// A background task ended; `tool_use_id` is the call that started it.
    TaskEnded { tool_use_id: String, status: String },
    /// A Monitor reported that it expired; the notice names the task, not the call.
    TaskGone { task_id: String },
}

#[derive(Clone, Debug, PartialEq)]
struct Verdict {
    decision: String,
    source: String,
}

/// How a tool call shows in the feed.
#[derive(Clone, Debug, PartialEq)]
enum Act {
    /// `<Tool> <target>`.
    Tool(String),
    /// Agent type.
    Spawn(String),
    /// Skill name.
    Skill(String),
}

enum Line {
    /// Bookkeeping or an unknown record type.
    Skip,
    /// Not valid JSON, or not the expected shape.
    Bad,
    /// `turn` is true for user and assistant records.
    Kept { turn: bool, ms: Option<u64>, facts: Vec<Fact> },
}

/// True when the line opens with a `"type"` that cannot carry feed facts. Such lines (request
/// blobs among them, often megabytes) are never parsed.
fn bookkeeping(line: &[u8]) -> bool {
    let head = &line[..line.len().min(PEEK)];
    let Some(rest) = head.strip_prefix(br#"{"type":""#) else { return false };
    match rest.iter().position(|b| *b == b'"') {
        Some(end) => !KEPT.iter().any(|k| k.as_bytes() == &rest[..end]),
        None => true,
    }
}

fn parse_line(line: &[u8]) -> Line {
    if bookkeeping(line) {
        return Line::Skip;
    }
    let Ok(r) = serde_json::from_slice::<Record>(line) else { return Line::Bad };
    let ms = r.timestamp.as_deref().and_then(parse_ts);
    let mut facts = Vec::new();
    let task = r.origin.as_deref() == Some("task-notification");
    let turn = match r.kind.as_deref() {
        Some("assistant") => {
            if r.is_aborted_mid_stream == Some(true) {
                facts.push(Fact::Aborted);
            }
            if let Some(Content::Blocks(blocks)) = r.message.and_then(|m| m.content) {
                for b in blocks {
                    if b.kind.as_deref() != Some("tool_use") {
                        continue;
                    }
                    let (Some(id), Some(tool)) = (b.id, b.name) else { continue };
                    let input = b.input.unwrap_or_default();
                    let act = act_of(&tool, &input);
                    let stop = matches!(tool.as_str(), "TaskStop" | "KillShell").then(|| input.task_id.or(input.shell_id)).flatten();
                    let desc = input.description.as_deref().map(tidy).filter(|d| !d.is_empty());
                    facts.push(Fact::Call { id, tool, act, stop, desc });
                }
            }
            true
        }
        Some("user") => {
            match r.message.and_then(|m| m.content) {
                Some(Content::Text(s)) if task => task_ends(&s, &mut facts),
                Some(Content::Text(s)) if s.starts_with(INTERRUPT) => facts.push(Fact::Interrupted),
                Some(Content::Blocks(blocks)) => {
                    let decision = r.permission_decision.and_then(|d| {
                        Some(Verdict { decision: d.decision?, source: d.source.unwrap_or_default() })
                    });
                    let launched = r.tool_use_result.status.as_deref() == Some("async_launched");
                    let (bg_task, monitor_task) = (r.tool_use_result.background, r.tool_use_result.task);
                    for b in blocks {
                        match b.kind.as_deref() {
                            Some("tool_result") => {
                                let Some(id) = b.tool_use_id else { continue };
                                facts.push(Fact::Return {
                                    id,
                                    error: b.is_error == Some(true),
                                    declined: b.content.as_deref().is_some_and(|c| c.starts_with(DECLINED)),
                                    decision: decision.clone(),
                                    launched,
                                    bg_task: bg_task.clone(),
                                    monitor_task: monitor_task.clone(),
                                });
                            }
                            Some("text") if b.text.as_deref().is_some_and(|t| t.starts_with(INTERRUPT)) => {
                                facts.push(Fact::Interrupted)
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            true
        }
        Some("system") => {
            if r.subtype.as_deref().is_some_and(|s| s.ends_with("compact_boundary")) {
                facts.push(Fact::Compacted);
            }
            if r.subtype.as_deref() == Some("turn_duration") {
                let pending = r.pending_background_agent_count.as_deref().and_then(|p| p.parse().ok()).unwrap_or(0);
                facts.push(Fact::TurnEnded { pending });
            }
            false
        }
        Some("attachment") => {
            if let Some(a) = r.attachment {
                if a.kind.as_deref() == Some("queued_command") && a.origin.as_deref() == Some("task-notification") {
                    if let Some(p) = a.prompt {
                        task_ends(&p, &mut facts);
                    }
                }
            }
            false
        }
        _ => return Line::Skip,
    };
    Line::Kept { turn, ms, facts }
}

/// One [`Fact::TaskEnded`] per `<task-notification>` that names a tool call and a status, and a
/// [`Fact::TaskGone`] per Monitor expiry notice. Per-event Monitor notices carry neither.
fn task_ends(text: &str, facts: &mut Vec<Fact>) {
    const OPEN: &str = "<task-notification>";
    const CLOSE: &str = "</task-notification>";
    let mut rest = text;
    while let Some(i) = rest.find(OPEN) {
        let body = &rest[i + OPEN.len()..];
        let end = body.find(CLOSE).unwrap_or(body.len());
        let note = &body[..end];
        if let (Some(id), Some(status)) = (tag(note, "tool-use-id"), tag(note, "status")) {
            facts.push(Fact::TaskEnded { tool_use_id: id.to_owned(), status: status.to_owned() });
        } else if let (Some(task), Some(event)) = (tag(note, "task-id"), tag(note, "event")) {
            if event.starts_with("[Monitor expired") {
                facts.push(Fact::TaskGone { task_id: task.to_owned() });
            }
        }
        rest = &body[end..];
    }
}

fn tag<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = s.find(&open)? + open.len();
    let len = s[start..].find(&format!("</{name}>"))?;
    Some(s[start..start + len].trim()).filter(|t| !t.is_empty())
}

/// A transcript record, reduced to the fields the feed needs. Unknown fields are skipped
/// without allocating, and odd field types read as absent instead of failing the record.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    #[serde(rename = "type", default, deserialize_with = "loose_str")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    subtype: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    timestamp: Option<String>,
    #[serde(default, deserialize_with = "loose_bool")]
    is_aborted_mid_stream: Option<bool>,
    /// Present on `turn_duration` records only while above zero.
    #[serde(default, deserialize_with = "loose_str")]
    pending_background_agent_count: Option<String>,
    #[serde(default)]
    message: Option<Message>,
    #[serde(default)]
    permission_decision: Option<Decision>,
    /// Only its `status`, `backgroundTaskId` and `taskId`.
    #[serde(default)]
    tool_use_result: ToolOut,
    /// Only its `kind`.
    #[serde(default, deserialize_with = "kind_field")]
    origin: Option<String>,
    #[serde(default)]
    attachment: Option<Attachment>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    content: Option<Content>,
}

#[derive(Deserialize)]
struct Decision {
    #[serde(default, deserialize_with = "loose_str")]
    decision: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    source: Option<String>,
}

#[derive(Deserialize)]
struct Attachment {
    #[serde(rename = "type", default, deserialize_with = "loose_str")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    prompt: Option<String>,
    #[serde(default, deserialize_with = "kind_field")]
    origin: Option<String>,
}

#[derive(Deserialize, Default)]
struct Block {
    #[serde(rename = "type", default, deserialize_with = "loose_str")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    id: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    name: Option<String>,
    #[serde(default)]
    input: Option<Input>,
    #[serde(default, deserialize_with = "loose_str")]
    tool_use_id: Option<String>,
    #[serde(default, deserialize_with = "loose_bool")]
    is_error: Option<bool>,
    #[serde(default, deserialize_with = "snippet")]
    text: Option<String>,
    #[serde(default, deserialize_with = "snippet")]
    content: Option<String>,
}

#[derive(Deserialize, Default)]
struct Input {
    #[serde(default, deserialize_with = "loose_str")]
    file_path: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    notebook_path: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    command: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    pattern: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    url: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    skill: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    subagent_type: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    task_id: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    shell_id: Option<String>,
    #[serde(default, deserialize_with = "loose_str")]
    description: Option<String>,
}

/// `message.content`: a plain string or a list of blocks.
enum Content {
    Text(String),
    Blocks(Vec<Block>),
}

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Content, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Content;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string or a list of blocks")
            }
            fn visit_str<E>(self, v: &str) -> Result<Content, E> {
                Ok(Content::Text(v.to_owned()))
            }
            fn visit_string<E>(self, v: String) -> Result<Content, E> {
                Ok(Content::Text(v))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Content, A::Error> {
                let mut blocks = Vec::new();
                while let Some(b) = a.next_element::<Block>()? {
                    blocks.push(b);
                }
                Ok(Content::Blocks(blocks))
            }
        }
        d.deserialize_any(V)
    }
}

/// What a lenient field read found.
enum Scalar {
    Str(String),
    Bool(bool),
    Other,
}

/// Accepts any JSON value; strings and numbers read as text, booleans as booleans.
struct ScalarV;

impl<'de> Visitor<'de> for ScalarV {
    type Value = Scalar;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any value")
    }
    fn visit_str<E>(self, v: &str) -> Result<Scalar, E> {
        Ok(Scalar::Str(v.to_owned()))
    }
    fn visit_string<E>(self, v: String) -> Result<Scalar, E> {
        Ok(Scalar::Str(v))
    }
    fn visit_u64<E>(self, v: u64) -> Result<Scalar, E> {
        Ok(Scalar::Str(v.to_string()))
    }
    fn visit_i64<E>(self, v: i64) -> Result<Scalar, E> {
        Ok(Scalar::Str(v.to_string()))
    }
    fn visit_f64<E>(self, _: f64) -> Result<Scalar, E> {
        Ok(Scalar::Other)
    }
    fn visit_bool<E>(self, v: bool) -> Result<Scalar, E> {
        Ok(Scalar::Bool(v))
    }
    fn visit_unit<E>(self) -> Result<Scalar, E> {
        Ok(Scalar::Other)
    }
    fn visit_none<E>(self) -> Result<Scalar, E> {
        Ok(Scalar::Other)
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Scalar, D::Error> {
        d.deserialize_any(ScalarV)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Scalar, A::Error> {
        while a.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Scalar::Other)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Scalar, A::Error> {
        while a.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(Scalar::Other)
    }
}

impl<'de> Deserialize<'de> for Scalar {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Scalar, D::Error> {
        d.deserialize_any(ScalarV)
    }
}

fn loose_str<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match Scalar::deserialize(d)? {
        Scalar::Str(s) => Some(s),
        _ => None,
    })
}

fn loose_bool<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    Ok(match Scalar::deserialize(d)? {
        Scalar::Bool(b) => Some(b),
        _ => None,
    })
}

/// Reads one text field of an object; anything else reads as `None`.
struct FieldV(&'static str);

impl<'de> Visitor<'de> for FieldV {
    type Value = Option<String>;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any value")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Option<String>, A::Error> {
        let mut found = None;
        while let Some(key) = a.next_key::<Scalar>()? {
            match key {
                Scalar::Str(k) if k == self.0 => {
                    if let Scalar::Str(v) = a.next_value::<Scalar>()? {
                        found = Some(v);
                    }
                }
                _ => {
                    a.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(found)
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Option<String>, D::Error> {
        d.deserialize_any(self)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Option<String>, A::Error> {
        while a.next_element::<IgnoredAny>()?.is_some() {}
        Ok(None)
    }
    fn visit_str<E>(self, _: &str) -> Result<Option<String>, E> {
        Ok(None)
    }
    fn visit_u64<E>(self, _: u64) -> Result<Option<String>, E> {
        Ok(None)
    }
    fn visit_i64<E>(self, _: i64) -> Result<Option<String>, E> {
        Ok(None)
    }
    fn visit_f64<E>(self, _: f64) -> Result<Option<String>, E> {
        Ok(None)
    }
    fn visit_bool<E>(self, _: bool) -> Result<Option<String>, E> {
        Ok(None)
    }
    fn visit_unit<E>(self) -> Result<Option<String>, E> {
        Ok(None)
    }
    fn visit_none<E>(self) -> Result<Option<String>, E> {
        Ok(None)
    }
}

/// The parts of a transcript record's `toolUseResult` the feed reads.
#[derive(Default)]
struct ToolOut {
    /// `async_launched` for a background Agent call.
    status: Option<String>,
    /// Set when a Bash or PowerShell call runs on in the background.
    background: Option<String>,
    /// Set by Monitor.
    task: Option<String>,
}

impl<'de> Deserialize<'de> for ToolOut {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<ToolOut, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = ToolOut;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<ToolOut, A::Error> {
                let mut out = ToolOut::default();
                while let Some(key) = a.next_key::<Scalar>()? {
                    let slot = match key {
                        Scalar::Str(k) if k == "status" => Some(&mut out.status),
                        Scalar::Str(k) if k == "backgroundTaskId" => Some(&mut out.background),
                        Scalar::Str(k) if k == "taskId" => Some(&mut out.task),
                        _ => None,
                    };
                    match slot {
                        Some(slot) => {
                            if let Scalar::Str(v) = a.next_value::<Scalar>()? {
                                *slot = Some(v);
                            }
                        }
                        None => {
                            a.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(out)
            }
            fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<ToolOut, D::Error> {
                d.deserialize_any(self)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<ToolOut, A::Error> {
                while a.next_element::<IgnoredAny>()?.is_some() {}
                Ok(ToolOut::default())
            }
            fn visit_str<E>(self, _: &str) -> Result<ToolOut, E> {
                Ok(ToolOut::default())
            }
            fn visit_u64<E>(self, _: u64) -> Result<ToolOut, E> {
                Ok(ToolOut::default())
            }
            fn visit_i64<E>(self, _: i64) -> Result<ToolOut, E> {
                Ok(ToolOut::default())
            }
            fn visit_f64<E>(self, _: f64) -> Result<ToolOut, E> {
                Ok(ToolOut::default())
            }
            fn visit_bool<E>(self, _: bool) -> Result<ToolOut, E> {
                Ok(ToolOut::default())
            }
            fn visit_unit<E>(self) -> Result<ToolOut, E> {
                Ok(ToolOut::default())
            }
            fn visit_none<E>(self) -> Result<ToolOut, E> {
                Ok(ToolOut::default())
            }
        }
        d.deserialize_any(V)
    }
}

fn kind_field<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    d.deserialize_any(FieldV("kind"))
}

/// The first [`SNIPPET`] characters of a string, or of the first `text` in a list of blocks.
struct SnippetV;

struct Snip(Option<String>);

impl<'de> Deserialize<'de> for Snip {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Snip, D::Error> {
        d.deserialize_any(SnippetV).map(Snip)
    }
}

impl<'de> Visitor<'de> for SnippetV {
    type Value = Option<String>;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any value")
    }
    fn visit_str<E>(self, v: &str) -> Result<Option<String>, E> {
        Ok(Some(v.chars().take(SNIPPET).collect()))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Option<String>, A::Error> {
        let mut found = None;
        while let Some(Snip(s)) = a.next_element::<Snip>()? {
            found = found.or(s);
        }
        Ok(found)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Option<String>, A::Error> {
        let mut found = None;
        while let Some(key) = a.next_key::<Scalar>()? {
            match key {
                Scalar::Str(k) if k == "text" => found = a.next_value::<Snip>()?.0,
                _ => {
                    a.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(found)
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Option<String>, D::Error> {
        d.deserialize_any(self)
    }
    fn visit_u64<E>(self, _: u64) -> Result<Option<String>, E> {
        Ok(None)
    }
    fn visit_i64<E>(self, _: i64) -> Result<Option<String>, E> {
        Ok(None)
    }
    fn visit_f64<E>(self, _: f64) -> Result<Option<String>, E> {
        Ok(None)
    }
    fn visit_bool<E>(self, _: bool) -> Result<Option<String>, E> {
        Ok(None)
    }
    fn visit_unit<E>(self) -> Result<Option<String>, E> {
        Ok(None)
    }
    fn visit_none<E>(self) -> Result<Option<String>, E> {
        Ok(None)
    }
}

fn snippet<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    d.deserialize_any(SnippetV)
}

/// `2026-10-14T17:52:19.999Z` as unix milliseconds. UTC only.
pub(crate) fn parse_ts(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |from: usize, to: usize| s.get(from..to)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, se) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let mut rest = &s[19..];
    let mut ms = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        let three: String = frac[..digits].chars().chain("000".chars()).take(3).collect();
        ms = three.parse().ok()?;
        rest = &frac[digits..];
    }
    if rest != "Z" && rest != "+00:00" {
        return None;
    }
    let days = days_from_civil(y, mo, d);
    u64::try_from(((days * 86_400 + h * 3600 + mi * 60 + se) * 1000) + ms).ok()
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `<who> · <what>`, cut to [`MAX_PHRASE`] characters.
pub(crate) fn phrase(who: &str, what: &str) -> String {
    clip(&format!("{who} · {what}"), MAX_PHRASE)
}

/// At most `max` characters, ending in `…` when cut. Never splits a character.
pub(crate) fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// One line of text: control characters and runs of whitespace become single spaces.
fn tidy(s: &str) -> String {
    s.split(|c: char| c.is_whitespace() || c.is_control()).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ")
}

pub(crate) fn basename(p: &str) -> &str {
    let p = p.trim().trim_end_matches(['/', '\\']);
    p.rsplit(['/', '\\']).next().unwrap_or(p)
}

/// The program a shell command runs, without a path or `.exe`: the first word of the first
/// statement that runs one. Statements that assign or evaluate (`$x = …`, `1..3 | …`) and loop
/// or branch headers are passed over.
pub(crate) fn command_name(cmd: &str) -> Option<String> {
    cmd.split([';', '|', '\n']).find_map(statement_name)
}

/// Shell words that open a loop or branch; the program comes in a later statement.
const HEADERS: &[&str] = &["for", "foreach", "while", "until", "if", "elif", "case", "function", "select"];

/// Shell words that precede the program within the same statement.
const LEADERS: &[&str] = &["&", "&&", "do", "then", "else", "time", "exec", "sudo"];

fn statement_name(stmt: &str) -> Option<String> {
    let mut rest = stmt.trim_start();
    // `$name = <rhs>` runs a program only when the right side starts with one.
    let rhs = rest.strip_prefix('$').and_then(|r| {
        let name = r.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == ':')).unwrap_or(r.len());
        r[name..].trim_start().strip_prefix('=').filter(|v| !v.starts_with('='))
    });
    if let Some(rhs) = rhs {
        rest = rhs.trim_start();
    }
    loop {
        let (word, after) = match rest.chars().next()? {
            q @ ('"' | '\'') => {
                if rhs.is_some() {
                    return None;
                }
                let body = &rest[1..];
                let end = body.find(q).unwrap_or(body.len());
                (&body[..end], body.get(end + 1..).unwrap_or(""))
            }
            _ => {
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                (&rest[..end], &rest[end..])
            }
        };
        rest = after.trim_start();
        let word = word.trim_start_matches(['(', '{']).split([')', ';', '|', '<', '>', '}']).next().unwrap_or("");
        let assignment = word.split_once('=').is_some_and(|(k, _)| {
            !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        if word.is_empty() || assignment || LEADERS.contains(&word) {
            continue;
        }
        if word.starts_with(['$', '@']) || word.starts_with(|c: char| c.is_ascii_digit()) || HEADERS.contains(&word) {
            return None;
        }
        let base = basename(word);
        let base = match base.len().checked_sub(4) {
            Some(i) if base.is_char_boundary(i) && base[i..].eq_ignore_ascii_case(".exe") => &base[..i],
            _ => base,
        };
        return (!base.is_empty()).then(|| base.to_owned());
    }
}

/// The host of a URL, without scheme, credentials or port.
fn host(url: &str) -> Option<&str> {
    let rest = url.trim().split_once("://").map_or(url.trim(), |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    let host = host.split(':').next()?;
    (!host.is_empty()).then_some(host)
}

/// `mcp__server__tool` as `server tool`.
pub(crate) fn mcp(tool: &str) -> Option<String> {
    let parts: Vec<&str> = tool.strip_prefix("mcp__")?.split("__").filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [server, .., name] => Some(format!("{server} {name}")),
        _ => None,
    }
}

fn act_of(tool: &str, input: &Input) -> Act {
    let target = match tool {
        "Read" | "Edit" | "Write" | "MultiEdit" => input.file_path.as_deref().map(|p| basename(p).to_owned()),
        "NotebookEdit" => input.notebook_path.as_deref().or(input.file_path.as_deref()).map(|p| basename(p).to_owned()),
        "Bash" | "PowerShell" | "Monitor" => input.command.as_deref().and_then(command_name),
        "Grep" | "Glob" => input.pattern.clone(),
        "WebFetch" => input.url.as_deref().and_then(host).map(str::to_owned),
        "Agent" | "Task" => {
            let kind = input.subagent_type.as_deref().map(tidy).filter(|k| !k.is_empty());
            return Act::Spawn(kind.unwrap_or_else(|| "agent".into()));
        }
        "Skill" => {
            let name = input.skill.as_deref().map(tidy).filter(|k| !k.is_empty());
            return Act::Skill(name.unwrap_or_else(|| "skill".into()));
        }
        _ => {
            return Act::Tool(mcp(tool).unwrap_or_else(|| tidy(tool)));
        }
    };
    match target.map(|t| tidy(&t)).filter(|t| !t.is_empty()) {
        Some(t) => Act::Tool(format!("{tool} {t}")),
        None => Act::Tool(tool.to_owned()),
    }
}

/// Lowercase last folder of a cwd, e.g. `erebus` for `F:\Projects\Erebus`.
pub(crate) fn base_label(cwd: &str, fallback: &str) -> String {
    let name = tidy(basename(cwd).trim_end_matches(':')).to_lowercase();
    if name.is_empty() {
        fallback.into()
    } else {
        name
    }
}

/// `base`, or `base-2`, `base-3`… for the first one no live session holds.
pub(crate) fn allocate(base: &str, taken: &[&str]) -> String {
    (1..)
        .map(|n| if n == 1 { base.to_owned() } else { format!("{base}-{n}") })
        .find(|l| !taken.contains(&l.as_str()))
        .unwrap_or_else(|| base.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_on_a_shell_is_waiting() {
        assert_eq!(status_of_str(Some("busy")), SessionStatus::Busy);
        assert_eq!(status_of_str(Some("shell")), SessionStatus::Waiting);
        assert_eq!(status_of_str(Some("idle")), SessionStatus::Idle);
        assert_eq!(status_of_str(None), SessionStatus::Idle);
    }

    fn session(label: &str) -> Session {
        let f = SessionFile {
            pid: 1,
            session_id: "s1".into(),
            cwd: Some(r"C:\work\Alpha".into()),
            started_at: Some("1000".into()),
            proc_start: None,
            status: None,
        };
        Session::new(&f, label.into(), false, 0)
    }

    /// Feeds lines through the parser and session as live transcript input.
    fn feed(s: &mut Session, lines: &[String]) -> Vec<FeedEvent> {
        let mut tail = Tail::detached();
        let mut block = Vec::new();
        for l in lines {
            block.extend_from_slice(l.as_bytes());
            block.push(b'\n');
        }
        let mut asks = Asks::default();
        let mut out = Out::default();
        for (fact, ms) in scan(&mut tail, &block, 5, &mut Stats::default()) {
            s.apply(None, fact, ms, Mode::Live, &mut asks, &mut out);
        }
        out.finish()
    }

    fn phrases(events: &[FeedEvent]) -> Vec<String> {
        events.iter().map(|e| e.phrase.clone()).collect()
    }

    fn ts(ms: u64) -> String {
        let secs = ms / 1000;
        format!("2026-10-29T10:{:02}:{:02}.{:03}Z", secs / 60 % 60, secs % 60, ms % 1000)
    }

    fn call(id: &str, name: &str, input: &str, ms: u64) -> String {
        format!(
            r#"{{"parentUuid":null,"isSidechain":false,"message":{{"role":"assistant","content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{input}}}]}},"type":"assistant","timestamp":"{}"}}"#,
            ts(ms)
        )
    }

    fn result(id: &str, is_error: bool, extra: &str, ms: u64) -> String {
        format!(
            r#"{{"parentUuid":null,"isSidechain":false,"type":"user","message":{{"role":"user","content":[{{"tool_use_id":"{id}","type":"tool_result","content":"out","is_error":{is_error}}}]}}{extra},"timestamp":"{}"}}"#,
            ts(ms)
        )
    }

    #[test]
    fn tool_calls_become_short_phrases() {
        let mut s = session("alpha");
        let lines = [
            call("t1", "Read", r#"{"file_path":"F:\\Projects\\Erebus\\src\\main.rs"}"#, 1000),
            call("t2", "Bash", r#"{"command":"cargo test -p erebus-core 2>&1 | tail","description":"x"}"#, 1001),
            call("t3", "Bash", r#"{"command":"RUST_LOG=debug \"C:/tools/rg.exe\" foo"}"#, 1002),
            call("t4", "PowerShell", r#"{"command":"& 'C:\\Program Files\\Git\\bin\\git.exe' status"}"#, 1003),
            call("t5", "Grep", r#"{"pattern":"fn\\s+main","path":"src"}"#, 1004),
            call("t6", "WebFetch", r#"{"url":"https://user@docs.rs:443/notify/latest?x=1","prompt":"p"}"#, 1005),
            call("t7", "Agent", r#"{"description":"d","prompt":"secret plan","subagent_type":"Explore"}"#, 1006),
            call("t8", "Skill", r#"{"skill":"code-review","args":"high"}"#, 1007),
            call("t9", "mcp__claude_ai_Claude_Docs__batch", r#"{"batch":[]}"#, 1008),
            call("t10", "TodoWrite", r#"{"todos":[]}"#, 1009),
            call("t11", "Edit", r#"{"file_path":"/home/a/notes.md","old_string":"a","new_string":"b"}"#, 1010),
            call("t12", "Write", r#"{"file_path":[1]}"#, 1011),
        ];
        let events = feed(&mut s, &lines);
        assert_eq!(
            phrases(&events),
            [
                "alpha · Read main.rs",
                "alpha · Bash cargo",
                "alpha · Bash rg",
                "alpha · PowerShell git",
                "alpha · Grep fn\\s+main",
                "alpha · WebFetch docs.rs",
                "alpha · spawned Explore",
                "alpha · /code-review",
                "alpha · claude_ai_Claude_Docs batch",
                "alpha · TodoWrite",
                "alpha · Edit notes.md",
                "alpha · Write",
            ]
        );
        let kinds: Vec<FeedKind> = events.iter().map(|e| e.kind).collect();
        assert_eq!(kinds[6], FeedKind::Spawn);
        assert_eq!(kinds[7], FeedKind::Skill);
        assert!(events.iter().all(|e| !e.backlog && e.session == "s1" && e.worker.is_none()));
        assert_eq!(events[0].unix_ms, parse_ts(&ts(1000)).unwrap());
    }

    #[test]
    fn results_show_only_failures_and_long_runs() {
        let mut s = session("alpha");
        let lines = [
            call("a", "Bash", r#"{"command":"ls"}"#, 1000),
            result("a", false, "", 2000),
            call("b", "Bash", r#"{"command":"cargo build"}"#, 3000),
            result("b", true, "", 4000),
            call("c", "Bash", r#"{"command":"cargo test"}"#, 5000),
            call("d", "Write", r#"{"file_path":"w.rs"}"#, 5001),
            result("c", false, "", 16_000),
            result("d", false, "", 16_001),
        ];
        let events = feed(&mut s, &lines);
        assert_eq!(
            phrases(&events),
            [
                "alpha · Bash ls",
                "alpha · Bash cargo",
                "alpha · Bash failed",
                "alpha · Bash cargo",
                "alpha · Write w.rs",
                "alpha · Bash ok"
            ]
        );
        assert_eq!(events[2].kind, FeedKind::Error);
        assert_eq!(events[5].kind, FeedKind::Result);
    }

    #[test]
    fn permission_decisions() {
        let mut s = session("alpha");
        let pd = |d: &str, src: &str| format!(r#","permissionDecision":{{"decision":"{d}","source":"{src}","reasonType":"x"}}"#);
        let lines = [
            call("a", "Bash", r#"{"command":"rm -rf x"}"#, 1000),
            result("a", true, &pd("reject", "hook"), 1001),
            call("b", "Bash", r#"{"command":"ls"}"#, 1002),
            result("b", false, &pd("accept", "config"), 1003),
            call("c", "Bash", r#"{"command":"git push"}"#, 1004),
            result("c", false, &pd("accept", "user_temporary"), 1005),
            call("d", "Edit", r#"{"file_path":"a.rs"}"#, 1006),
            result("d", true, &pd("reject", "user_reject"), 1007),
        ];
        let events = feed(&mut s, &lines);
        assert_eq!(
            phrases(&events),
            [
                "alpha · Bash rm",
                "alpha · blocked Bash",
                "alpha · Bash ls",
                "alpha · Bash git",
                "alpha · granted Bash",
                "alpha · Edit a.rs",
                "alpha · denied Edit",
            ]
        );
        assert!(events.iter().filter(|e| e.phrase.contains("ed ")).all(|e| e.kind == FeedKind::Perm));
    }

    #[test]
    fn aborted_reply_gives_one_interrupted_line() {
        let mut s = session("alpha");
        let aborted = format!(
            r#"{{"parentUuid":null,"message":{{"content":[{{"type":"text","text":"partial"}}]}},"type":"assistant","isAbortedMidStream":true,"timestamp":"{}"}}"#,
            ts(1000)
        );
        let marker = format!(
            r#"{{"parentUuid":null,"type":"user","message":{{"role":"user","content":[{{"type":"text","text":"[Request interrupted by user]"}}]}},"timestamp":"{}"}}"#,
            ts(1001)
        );
        let events = feed(&mut s, &[aborted, marker.clone()]);
        assert_eq!(phrases(&events), ["alpha · interrupted"]);
        assert_eq!(events[0].kind, FeedKind::Error);
        // A marker on its own still counts.
        assert_eq!(phrases(&feed(&mut s, &[marker])), ["alpha · interrupted"]);
        // A declined tool result is covered by the marker that follows it.
        let declined = format!(
            r#"{{"parentUuid":null,"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"z","is_error":true,"content":[{{"type":"text","text":"The user doesn't want to proceed with this tool use."}}]}}]}},"timestamp":"{}"}}"#,
            ts(1002)
        );
        let events = feed(&mut s, &[call("z", "Bash", r#"{"command":"sleep 9"}"#, 1000), declined]);
        assert_eq!(phrases(&events), ["alpha · Bash sleep"]);
    }

    #[test]
    fn compaction_and_task_notifications() {
        let mut s = session("alpha");
        let compact = format!(
            r#"{{"parentUuid":null,"isSidechain":false,"type":"system","subtype":"compact_boundary","content":"Conversation compacted","timestamp":"{}"}}"#,
            ts(1000)
        );
        assert_eq!(phrases(&feed(&mut s, &[compact])), ["alpha · compacted"]);

        let note = r#"<task-notification>\n<task-id>a1</task-id>\n<tool-use-id>toolu_9</tool-use-id>\n<status>failed</status>\n<summary>s</summary>\n</task-notification>"#;
        let line = format!(r#"{{"parentUuid":null,"type":"user","message":{{"role":"user","content":"{note}"}},"origin":{{"kind":"task-notification"}}}}"#);
        let Line::Kept { facts, .. } = parse_line(line.as_bytes()) else { panic!("not kept") };
        assert_eq!(facts, [Fact::TaskEnded { tool_use_id: "toolu_9".into(), status: "failed".into() }]);

        let att = format!(
            r#"{{"parentUuid":null,"attachment":{{"type":"queued_command","prompt":"{note}","origin":{{"kind":"task-notification"}}}},"type":"attachment"}}"#
        );
        let Line::Kept { facts, .. } = parse_line(att.as_bytes()) else { panic!("not kept") };
        assert_eq!(facts.len(), 1);

        // The same text typed by a person is not a notification.
        let typed = format!(r#"{{"parentUuid":null,"type":"user","message":{{"content":"{note}"}}}}"#);
        let Line::Kept { facts, .. } = parse_line(typed.as_bytes()) else { panic!("not kept") };
        assert!(facts.is_empty());
    }

    #[test]
    fn async_agent_launch_is_silent() {
        let mut s = session("alpha");
        let launched = format!(
            r#"{{"parentUuid":null,"type":"user","message":{{"content":[{{"tool_use_id":"ag","type":"tool_result","content":[{{"type":"text","text":"Async agent launched"}}]}}]}},"toolUseResult":{{"isAsync":true,"status":"async_launched","agentId":"a1","prompt":"p"}},"timestamp":"{}"}}"#,
            ts(1001)
        );
        let events = feed(&mut s, &[call("ag", "Agent", r#"{"subagent_type":"Explore","prompt":"p"}"#, 1000), launched]);
        assert_eq!(phrases(&events), ["alpha · spawned Explore"]);
        assert!(!s.ended_spawns.contains("ag"));
        // A foreground result ends the spawn.
        let done = result("fg", false, r#","toolUseResult":"text""#, 1003);
        feed(&mut s, &[call("fg", "Agent", r#"{"prompt":"p"}"#, 1002), done]);
        assert!(s.ended_spawns.contains("fg"));
    }

    fn turn_end(pending: u32, ms: u64) -> String {
        let extra = if pending > 0 { format!(r#""pendingBackgroundAgentCount":{pending},"#) } else { String::new() };
        format!(
            r#"{{"parentUuid":null,"isSidechain":false,"type":"system","subtype":"turn_duration","durationMs":5,"messageCount":3,{extra}"timestamp":"{}"}}"#,
            ts(ms)
        )
    }

    fn prompt(ms: u64) -> String {
        format!(r#"{{"parentUuid":null,"type":"user","message":{{"role":"user","content":"do it"}},"timestamp":"{}"}}"#, ts(ms))
    }

    fn add_worker(s: &mut Session, id: &str, tool_use_id: &str) {
        s.next_worker += 1;
        let agent = Agent {
            n: s.next_worker,
            kind: "Explore".into(),
            started_ms: 0,
            tool_use_id: Some(tool_use_id.into()),
            live: true,
            tail: None,
            meta: PathBuf::new(),
            meta_mtime: None,
        };
        s.agents.insert(id.into(), agent);
    }

    #[test]
    fn turn_end_without_workers_is_idle() {
        let mut s = session("alpha");
        let events = feed(&mut s, &[prompt(1000), call("a", "Read", r#"{"file_path":"a.rs"}"#, 1001), turn_end(0, 2000)]);
        assert_eq!(phrases(&events), ["alpha · Read a.rs", "alpha · idle"]);
        assert_eq!(events[1].kind, FeedKind::Turn);
        assert_eq!(events[1].unix_ms, parse_ts(&ts(2000)).unwrap());
        assert!(!s.mid_turn);
    }

    #[test]
    fn turn_end_with_worker_then_worker_finish_is_idle() {
        let mut s = session("alpha");
        add_worker(&mut s, "w-1", "ag");
        let events = feed(&mut s, &[prompt(1000), turn_end(1, 2000)]);
        assert_eq!(phrases(&events), ["alpha · turn done · 1 running"]);
        assert_eq!(events[0].kind, FeedKind::Turn);

        // The pending count wins when it exceeds the live workers.
        let events = feed(&mut s, &[prompt(2500), turn_end(3, 2600)]);
        assert_eq!(phrases(&events), ["alpha · turn done · 3 running"]);

        let mut out = Out::default();
        s.end_agent("w-1", "completed", 3000, Mode::Live, &mut out);
        assert_eq!(phrases(&out.finish()), ["alpha·w1 · done", "alpha · idle"]);
    }

    #[test]
    fn worker_finish_mid_turn_is_not_idle() {
        let mut s = session("alpha");
        add_worker(&mut s, "w-1", "ag");
        feed(&mut s, &[prompt(1000)]);
        let mut out = Out::default();
        s.end_agent("w-1", "completed", 3000, Mode::Live, &mut out);
        assert_eq!(phrases(&out.finish()), ["alpha·w1 · done"]);
        // The turn then ends with nothing running.
        assert_eq!(phrases(&feed(&mut s, &[turn_end(0, 4000)])), ["alpha · idle"]);
    }

    #[test]
    fn idle_is_not_repeated() {
        let mut s = session("alpha");
        let events = feed(&mut s, &[prompt(1000), turn_end(0, 2000), turn_end(0, 2001)]);
        assert_eq!(phrases(&events), ["alpha · idle"]);

        // A worker ending after the session went idle adds nothing.
        add_worker(&mut s, "w-1", "ag");
        let mut out = Out::default();
        s.end_agent("w-1", "completed", 3000, Mode::Live, &mut out);
        assert_eq!(phrases(&out.finish()), ["alpha·w1 · done"]);

        // An interrupted turn still closes with one idle line.
        let marker = format!(
            r#"{{"parentUuid":null,"type":"user","message":{{"role":"user","content":[{{"type":"text","text":"[Request interrupted by user]"}}]}},"timestamp":"{}"}}"#,
            ts(4000)
        );
        let events = feed(&mut s, &[prompt(3500), marker, turn_end(0, 4001), turn_end(0, 4002)]);
        assert_eq!(phrases(&events), ["alpha · interrupted", "alpha · idle"]);
    }

    /// The result of a call that went on running in the background as task `task`.
    fn started(id: &str, task: &str, ms: u64) -> String {
        result(id, false, &format!(r#","toolUseResult":{{"stdout":"","stderr":"","interrupted":false,"backgroundTaskId":"{task}"}}"#), ms)
    }

    /// A `queued_command` task notification for the call `id`.
    fn note(task: &str, id: &str, status: &str, ms: u64) -> String {
        format!(
            r#"{{"parentUuid":null,"attachment":{{"type":"queued_command","prompt":"<task-notification>\n<task-id>{task}</task-id>\n<tool-use-id>{id}</tool-use-id>\n<status>{status}</status>\n<summary>s</summary>\n</task-notification>","origin":{{"kind":"task-notification"}}}},"type":"attachment","timestamp":"{}"}}"#,
            ts(ms)
        )
    }

    fn shell(id: &str, task: &str, ms: u64) -> [String; 2] {
        [call(id, "Bash", r#"{"command":"sleep 99","run_in_background":true}"#, ms), started(id, task, ms + 1)]
    }

    #[test]
    fn shells_and_monitors_reach_the_snapshot_named_and_leave_it_when_done() {
        let mut s = session("alpha");
        let mut lines = shell("b1", "t1", 1000).to_vec();
        lines.push(call("m1", "Monitor", r#"{"command":"tail -f build.log","description":"build log","timeout_ms":60000}"#, 2000));
        lines.push(result("m1", false, r#","toolUseResult":{"taskId":"m9","timeoutMs":60000,"persistent":false}"#, 2001));
        feed(&mut s, &lines);
        let tasks = s.info(None).tasks;
        assert_eq!(tasks.len(), 2);
        assert_eq!((tasks[0].kind, tasks[0].label.as_str(), tasks[0].id.as_str()), (TaskKind::Shell, "sleep", "b1"));
        assert_eq!((tasks[1].kind, tasks[1].label.as_str()), (TaskKind::Monitor, "build log"));
        assert_eq!(tasks[0].started_unix_ms, parse_ts(&ts(1000)).unwrap());
        feed(&mut s, &[note("t1", "b1", "completed", 3000)]);
        let tasks = s.info(None).tasks;
        assert_eq!(tasks.iter().map(|t| t.kind).collect::<Vec<_>>(), [TaskKind::Monitor]);
    }

    #[test]
    fn background_result_names_its_task() {
        let line = started("b1", "t1", 1000);
        let Line::Kept { facts, .. } = parse_line(line.as_bytes()) else { panic!("not kept") };
        assert!(matches!(
            facts.as_slice(),
            [Fact::Return { bg_task: Some(b), monitor_task: None, launched: false, .. }] if b == "t1"
        ));
        let line = result("m1", false, r#","toolUseResult":{"taskId":"m9","timeoutMs":1800000,"persistent":false}"#, 1000);
        let Line::Kept { facts, .. } = parse_line(line.as_bytes()) else { panic!("not kept") };
        assert!(matches!(facts.as_slice(), [Fact::Return { bg_task: None, monitor_task: Some(m), .. }] if m == "m9"));
        // Strings, arrays and nulls in the same slot read as absent.
        for extra in [r#","toolUseResult":"text""#, r#","toolUseResult":[1,{"a":2}]"#, r#","toolUseResult":null"#] {
            let line = result("x", false, extra, 1000);
            let Line::Kept { facts, .. } = parse_line(line.as_bytes()) else { panic!("not kept") };
            assert!(matches!(facts.as_slice(), [Fact::Return { bg_task: None, monitor_task: None, .. }]), "{extra}");
        }
    }

    #[test]
    fn turn_end_with_background_shell_keeps_running() {
        let mut s = session("alpha");
        let mut lines = vec![prompt(1000)];
        lines.extend(shell("b1", "t1", 1001));
        lines.push(turn_end(0, 2000));
        let events = feed(&mut s, &lines);
        assert_eq!(phrases(&events), ["alpha · Bash sleep", "alpha · turn done · 1 running"]);
        assert_eq!(s.tasks.len(), 1);
        // The pending agent count does not include shells; the larger figure shows.
        let events = feed(&mut s, &[prompt(2500), turn_end(3, 2600)]);
        assert_eq!(phrases(&events), ["alpha · turn done · 3 running"]);
        // Its notification ends the task and the session reads as idle.
        let events = feed(&mut s, &[note("t1", "b1", "completed", 3000)]);
        assert_eq!(phrases(&events), ["alpha · idle"]);
        assert!(s.tasks.is_empty());
        assert!(feed(&mut s, &[note("t1", "b1", "completed", 3001)]).is_empty());
    }

    #[test]
    fn worker_ends_while_shell_runs_is_not_idle() {
        let mut s = session("alpha");
        add_worker(&mut s, "w-1", "ag");
        let mut lines = vec![prompt(1000)];
        lines.extend(shell("b1", "t1", 1001));
        lines.push(turn_end(1, 2000));
        assert_eq!(phrases(&feed(&mut s, &lines)), ["alpha · Bash sleep", "alpha · turn done · 2 running"]);

        let mut out = Out::default();
        s.end_agent("w-1", "completed", 3000, Mode::Live, &mut out);
        assert_eq!(phrases(&out.finish()), ["alpha·w1 · done"]);

        // A failed shell ends the same way.
        assert_eq!(phrases(&feed(&mut s, &[note("t1", "b1", "failed", 3500)])), ["alpha · idle"]);
    }

    #[test]
    fn shell_ends_before_worker_waits_for_the_worker() {
        let mut s = session("alpha");
        add_worker(&mut s, "w-1", "ag");
        let mut lines = vec![prompt(1000)];
        lines.extend(shell("b1", "t1", 1001));
        lines.push(turn_end(0, 2000));
        feed(&mut s, &lines);
        assert!(feed(&mut s, &[note("t1", "b1", "completed", 2500)]).is_empty());
        let mut out = Out::default();
        s.end_agent("w-1", "completed", 3000, Mode::Live, &mut out);
        assert_eq!(phrases(&out.finish()), ["alpha·w1 · done", "alpha · idle"]);
    }

    #[test]
    fn shell_ending_mid_turn_is_not_idle() {
        let mut s = session("alpha");
        let mut lines = vec![prompt(1000)];
        lines.extend(shell("b1", "t1", 1001));
        lines.push(turn_end(0, 1500));
        lines.push(prompt(2000));
        lines.push(note("t1", "b1", "completed", 2100));
        assert_eq!(phrases(&feed(&mut s, &lines)), ["alpha · Bash sleep", "alpha · turn done · 1 running"]);
        assert_eq!(phrases(&feed(&mut s, &[turn_end(0, 2200)])), ["alpha · idle"]);
    }

    #[test]
    fn stopping_a_task_ends_it() {
        let mut s = session("alpha");
        let mut lines = vec![prompt(1000)];
        lines.extend(shell("b1", "t1", 1001));
        lines.push(turn_end(0, 2000));
        feed(&mut s, &lines);
        let stop = [
            call("k1", "TaskStop", r#"{"task_id":"t1"}"#, 3000),
            result("k1", false, r#","toolUseResult":{"message":"Successfully stopped task: t1","task_id":"t1","task_type":"local_bash"}"#, 3001),
        ];
        // The call itself is mid-turn activity, so the idle waits for the turn to close.
        assert_eq!(phrases(&feed(&mut s, &stop)), ["alpha · TaskStop"]);
        assert!(s.tasks.is_empty());
        assert_eq!(phrases(&feed(&mut s, &[turn_end(0, 4000)])), ["alpha · idle"]);

        // KillShell names its target `shell_id`.
        let mut s = session("alpha");
        let mut lines = vec![prompt(1000)];
        lines.extend(shell("b1", "t1", 1001));
        lines.push(turn_end(0, 2000));
        feed(&mut s, &lines);
        let kill = [call("k1", "KillShell", r#"{"shell_id":"t1"}"#, 3000), result("k1", false, "", 3001)];
        assert_eq!(phrases(&feed(&mut s, &kill)), ["alpha · KillShell"]);
        assert_eq!(phrases(&feed(&mut s, &[turn_end(0, 4000)])), ["alpha · idle"]);
    }

    #[test]
    fn declined_stop_leaves_the_task_running() {
        let mut s = session("alpha");
        let mut lines = vec![prompt(1000)];
        lines.extend(shell("b1", "t1", 1001));
        lines.push(turn_end(0, 2000));
        feed(&mut s, &lines);
        let declined = r#"{"parentUuid":null,"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"k1","is_error":true,"content":[{"type":"text","text":"The user doesn't want to proceed with this tool use."}]}]}}"#;
        feed(&mut s, &[call("k1", "TaskStop", r#"{"task_id":"t1"}"#, 3000), declined.to_owned()]);
        assert_eq!(s.tasks.len(), 1);
    }

    #[test]
    fn backlog_that_finished_a_shell_has_no_live_task() {
        let mut s = session("alpha");
        let mut tail = Tail::detached();
        let mut lines = vec![prompt(1000)];
        lines.extend(shell("b1", "t1", 1001));
        lines.extend(shell("b2", "t2", 1010));
        lines.push(note("t1", "b1", "completed", 1500));
        lines.push(turn_end(0, 2000));
        let block = lines.join("\n") + "\n";
        let mut asks = Asks::default();
        let mut out = Out::default();
        for (fact, ms) in scan(&mut tail, block.as_bytes(), 5, &mut Stats::default()) {
            s.apply(None, fact, ms, Mode::Backlog, &mut asks, &mut out);
        }
        // Only the shell without a completion in the backlog is live.
        assert_eq!(s.tasks.keys().collect::<Vec<_>>(), ["b2"]);
        assert_eq!(out.finish().last().map(|e| e.phrase.as_str()), Some("alpha · turn done · 1 running"));
    }

    #[test]
    fn stale_tasks_expire_after_transcript_silence() {
        let dir = std::env::temp_dir().join(format!("erebus-stale-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s1.jsonl");
        let mut lines = vec![prompt(1000)];
        lines.extend(shell("b1", "t1", 1001));
        lines.push(turn_end(0, 2000));
        fs::write(&path, lines.join("\n") + "\n").unwrap();

        let last = parse_ts(&ts(2000)).unwrap();
        let mut s = session("alpha");
        s.transcript = Some(Tail::open(path.clone(), false));
        let mut asks = Asks::default();
        let mut out = Out::default();
        s.follow(&dir, last + STALE_WORKER_MS - 1, &mut asks, &mut out);
        assert_eq!(s.tasks.len(), 1);
        s.follow(&dir, last + STALE_WORKER_MS + 1, &mut asks, &mut out);
        assert!(s.tasks.is_empty());
        let events = out.finish();
        assert_eq!(events.last().map(|e| e.phrase.as_str()), Some("alpha · idle"));
        assert!(!events.last().unwrap().backlog);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn monitor_runs_until_it_expires_or_its_stream_ends() {
        let expiry = |task: &str, ms: u64| {
            format!(
                r#"{{"parentUuid":null,"attachment":{{"type":"queued_command","prompt":"<task-notification>\n<task-id>{task}</task-id>\n<summary>Monitor event: \"w\"</summary>\n<event>[Monitor expired after 30m with no events delivered. Re-arm it.]</event>\n</task-notification>","origin":{{"kind":"task-notification"}}}},"type":"attachment","timestamp":"{}"}}"#,
                ts(ms)
            )
        };
        let event = |task: &str, ms: u64| {
            format!(
                r#"{{"parentUuid":null,"attachment":{{"type":"queued_command","prompt":"<task-notification>\n<task-id>{task}</task-id>\n<summary>Monitor event: \"w\"</summary>\n<event>[17:40:18] line</event>\n</task-notification>","origin":{{"kind":"task-notification"}}}},"type":"attachment","timestamp":"{}"}}"#,
                ts(ms)
            )
        };
        let monitor = |ms: u64| {
            [
                call("m1", "Monitor", r#"{"command":"tail -f x","description":"w","timeout_ms":1800000}"#, ms),
                result("m1", false, r#","toolUseResult":{"taskId":"m9","timeoutMs":1800000,"persistent":false}"#, ms + 1),
            ]
        };

        let mut s = session("alpha");
        let mut lines = vec![prompt(1000)];
        lines.extend(monitor(1001));
        lines.push(turn_end(0, 2000));
        assert_eq!(phrases(&feed(&mut s, &lines)), ["alpha · Monitor tail", "alpha · turn done · 1 running"]);
        // A per-event notice changes nothing.
        assert!(feed(&mut s, &[event("m9", 2500)]).is_empty());
        assert_eq!(s.tasks.len(), 1);
        assert_eq!(phrases(&feed(&mut s, &[expiry("m9", 3000)])), ["alpha · idle"]);

        // A stream that ends reports a status against the call, like a shell.
        let mut lines = vec![prompt(4000)];
        lines.extend(monitor(4001));
        lines.push(turn_end(0, 5000));
        feed(&mut s, &lines);
        assert_eq!(phrases(&feed(&mut s, &[note("m9", "m1", "completed", 6000)])), ["alpha · idle"]);
    }

    #[test]
    fn task_id_on_other_tools_is_not_a_task() {
        let mut s = session("alpha");
        let lines = [
            prompt(1000),
            call("u1", "TaskUpdate", r#"{"taskId":"1","status":"completed"}"#, 1001),
            result("u1", false, r#","toolUseResult":{"success":true,"taskId":"1","updatedFields":["status"]}"#, 1002),
            turn_end(0, 2000),
        ];
        assert_eq!(phrases(&feed(&mut s, &lines)), ["alpha · TaskUpdate", "alpha · idle"]);
        assert!(s.tasks.is_empty());
    }

    #[test]
    fn worker_background_shell_is_not_a_session_task() {
        let mut s = session("alpha");
        add_worker(&mut s, "w-1", "ag");
        let mut tail = Tail::detached();
        let block = format!("{}\n{}\n", call("b1", "Bash", r#"{"command":"sleep 9","run_in_background":true}"#, 1000), started("b1", "t1", 1001));
        let mut asks = Asks::default();
        let mut out = Out::default();
        for (fact, ms) in scan(&mut tail, block.as_bytes(), 5, &mut Stats::default()) {
            s.apply(Some("w-1"), fact, ms, Mode::Live, &mut asks, &mut out);
        }
        assert!(s.tasks.is_empty());
    }

    #[test]
    fn worker_turn_end_is_ignored() {
        let mut s = session("alpha");
        add_worker(&mut s, "w-1", "ag");
        let mut tail = Tail::detached();
        let block = format!("{}
{}
", prompt(1000), turn_end(0, 2000));
        let mut asks = Asks::default();
        let mut out = Out::default();
        for (fact, ms) in scan(&mut tail, block.as_bytes(), 5, &mut Stats::default()) {
            s.apply(Some("w-1"), fact, ms, Mode::Live, &mut asks, &mut out);
        }
        assert!(out.finish().is_empty());
        assert!(!s.mid_turn && !s.idle_shown);
    }

    #[test]
    fn bookkeeping_is_skipped_without_parsing() {
        // Invalid JSON after the type proves no parse happened.
        for line in [
            r#"{"type":"api-request-blob","sessionId":"x","message":{"content":[ broken"#,
            r#"{"type":"file-history-snapshot", nope"#,
            r#"{"type":"a-type-name-so-long-that-its-closing-quote-lies-beyond-the-peek-window-xxxxxxxxxx"#,
        ] {
            assert!(matches!(parse_line(line.as_bytes()), Line::Skip), "{line}");
        }
        assert!(matches!(parse_line(br#"{"type":"user", broken"#), Line::Bad));
        assert!(matches!(parse_line(br#"{"parentUuid":null,"type":"user","message":{"content":"#), Line::Bad));
        assert!(matches!(parse_line(br#"{"type":"queue-operation","operation":"enqueue"}"#), Line::Skip));
        // Unknown types that do not lead with "type" parse, then skip.
        assert!(matches!(parse_line(br#"{"parentUuid":null,"type":"progress"}"#), Line::Skip));

        let mut tail = Tail::detached();
        let mut stats = Stats::default();
        let block = b"{\"type\":\"api-request\",x\n{\"type\":\"user\",x\n\n";
        assert!(scan(&mut tail, block, 0, &mut stats).is_empty());
        assert_eq!((stats.lines, stats.skipped, stats.bad), (2, 1, 1));
    }

    #[test]
    fn partial_line_is_held_until_complete() {
        let mut lb = LineBuf::default();
        assert_eq!(lb.push(b"{\"a\":1}\n{\"b\":"), b"{\"a\":1}\n");
        assert_eq!(lb.push(b"2"), b"");
        assert_eq!(lb.push(b"}\n{\"c\""), b"{\"b\":2}\n");
        assert_eq!(lb.partial, b"{\"c\"");

        // Reading from mid-file drops the first, partial line.
        let mut lb = LineBuf { partial: Vec::new(), skip: true };
        assert_eq!(lb.push(b"tail of a line"), b"");
        assert_eq!(lb.push(b" still\n{\"x\":1}\n"), b"{\"x\":1}\n");
    }

    #[test]
    fn tail_reads_appends_and_survives_truncation() {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!("erebus-tail-{}.jsonl", std::process::id()));
        fs::write(&path, call("a", "Read", r#"{"file_path":"one.rs"}"#, 1000) + "\n").unwrap();
        let mut tail = Tail::open(path.clone(), true);
        let (block, mode) = tail.read(1);
        assert_eq!(mode, Mode::Backlog);
        assert_eq!(block.iter().filter(|b| **b == b'\n').count(), 1);

        let next = call("b", "Read", r#"{"file_path":"two.rs"}"#, 2000);
        let (head, rest) = next.split_at(30);
        fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(head.as_bytes()).unwrap();
        let (block, mode) = tail.read(2);
        assert!(block.is_empty());
        assert_eq!(mode, Mode::Live);
        fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(format!("{rest}\n").as_bytes()).unwrap();
        let (block, mode) = tail.read(3);
        assert_eq!((block, mode), ((next + "\n").into_bytes(), Mode::Live));

        fs::write(&path, "{}\n").unwrap();
        let (block, mode) = tail.read(4);
        assert_eq!((block.as_slice(), mode), (&b"{}\n"[..], Mode::Quiet));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn phrases_clip_on_char_boundaries() {
        let long = "ünïcødé-🦀-".repeat(10);
        let p = phrase("alpha", &long);
        assert_eq!(p.chars().count(), MAX_PHRASE);
        assert!(p.ends_with('…'));
        assert!(p.starts_with("alpha · ünïcødé-🦀"));
        assert_eq!(phrase("alpha", "Read a.rs"), "alpha · Read a.rs");
        let exact = "x".repeat(MAX_PHRASE);
        assert_eq!(clip(&exact, MAX_PHRASE), exact);
        assert_eq!(clip(&format!("{exact}y"), MAX_PHRASE).chars().count(), MAX_PHRASE);
        assert_eq!(tidy(" a\n\tb  c\u{7} "), "a b c");
    }

    #[test]
    fn labels_take_suffixes_in_start_order() {
        assert_eq!(base_label(r"F:\Projects\Erebus", "claude"), "erebus");
        assert_eq!(base_label("/home/me/My Repo/", "claude"), "my repo");
        assert_eq!(base_label(r"C:\", "claude"), "c");
        assert_eq!(base_label("", "claude"), "claude");
        assert_eq!(allocate("erebus", &[]), "erebus");
        assert_eq!(allocate("erebus", &["erebus"]), "erebus-2");
        assert_eq!(allocate("erebus", &["erebus", "erebus-2", "stratum"]), "erebus-3");
        // A freed first slot is reused while later sessions keep theirs.
        assert_eq!(allocate("erebus", &["erebus-2"]), "erebus");
    }

    #[test]
    fn slugs_match_claude_code_folders() {
        assert_eq!(slug(r"F:\Projects\Erebus"), "F--Projects-Erebus");
        assert_eq!(slug(r"F:\Projects\dev-mods\wt\W-0004"), "F--Projects-dev-mods-wt-W-0004");
        assert_eq!(
            slug(r"C:\Users\me\AppData\Local\Temp\claude\F--Projects-dev-mods\400b4785-e4ea\scratchpad"),
            "C--Users-me-AppData-Local-Temp-claude-F--Projects-dev-mods-400b4785-e4ea-scratchpad"
        );
        assert_eq!(slug("a_b.c d"), "a-b-c-d");
    }

    #[test]
    fn command_names_and_hosts() {
        assert_eq!(command_name("  cargo build --release").as_deref(), Some("cargo"));
        assert_eq!(command_name(r#""C:\Program Files\Git\bin\git.EXE" log"#).as_deref(), Some("git"));
        assert_eq!(command_name("A=1 B_2=x python -I s.py").as_deref(), Some("python"));
        assert_eq!(command_name("(Get-Process).Count").as_deref(), Some("Get-Process"));
        assert_eq!(command_name("git status; ls").as_deref(), Some("git"));
        assert_eq!(command_name("$s = 'C:\\x\\y'\nNew-Item -Force $s").as_deref(), Some("New-Item"));
        assert_eq!(command_name("$p = Get-Process; $p.Id").as_deref(), Some("Get-Process"));
        assert_eq!(command_name("1..2 | ForEach-Object { cargo test }").as_deref(), Some("ForEach-Object"));
        assert_eq!(command_name("for f in *.cs; do dotnet build $f; done").as_deref(), Some("dotnet"));
        assert_eq!(command_name("if [ -f x ]; then cat x; fi").as_deref(), Some("cat"));
        assert_eq!(command_name("$x"), None);
        assert_eq!(command_name("   "), None);
        assert_eq!(host("https://github.com/a/b"), Some("github.com"));
        assert_eq!(host("example.org:8080/x"), Some("example.org"));
        assert_eq!(host(""), None);
        assert_eq!(mcp("mcp__plugin_runpod_runpod__list-pods").as_deref(), Some("plugin_runpod_runpod list-pods"));
        assert_eq!(mcp("mcp__x"), None);
    }

    #[test]
    fn timestamps() {
        assert_eq!(parse_ts("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(parse_ts("2026-10-14T17:52:19.999Z"), Some(1_792_000_339_999));
        assert_eq!(parse_ts("2000-02-29T00:00:01Z"), Some(951_782_401_000));
        assert_eq!(parse_ts("2026-10-14T17:52:19.9Z"), Some(1_792_000_339_900));
        assert_eq!(parse_ts("2026-10-14 17:52:19Z"), None);
        assert_eq!(parse_ts("2026-10-14T17:52:19+02:00"), None);
    }

    #[test]
    fn asks_settle_then_resolve() {
        let mut asks = Asks::default();
        let ask = |session: &str, at: u64| Ask { session: session.into(), tool: "Bash".into(), agent: None, unix_ms: at, seen: at, shown: false };
        asks.ask("t1".into(), ask("s1", 1000));
        assert_eq!(asks.pending("s1", 2000), None);
        assert_eq!(asks.next_settle(2000), Some(2500));
        let p = asks.pending("s1", 2500).expect("settled");
        assert_eq!((p.tool_use_id.as_str(), p.since_unix_ms), ("t1", 1000));
        assert_eq!(asks.pending("s2", 2500), None);
        assert!(asks.resolve("t1", 3000).unwrap().shown);
        // A late ask for a resolved call is ignored.
        asks.ask("t1".into(), ask("s1", 1000));
        assert_eq!(asks.pending("s1", 4000), None);
        // Asks of unknown sessions are dropped after a while.
        asks.ask("t2".into(), ask("gone", 4000));
        asks.prune(4000 + ORPHAN_ASK_MS, &HashSet::new());
        assert!(asks.open.is_empty());
    }

    #[test]
    fn this_process_is_alive() {
        let me = std::process::id();
        let t = creation_time(me).expect("own creation time");
        assert!(alive(me, Some(t)));
        assert!(alive(me, None));
        assert!(!alive(me, Some(t + 1)));
    }
}
