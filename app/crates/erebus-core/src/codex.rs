//! The Codex service. Follows every live Codex thread on the machine and hands the hub its
//! [`SessionInfo`]s whenever they change, plus [`FeedEvent`]s as they happen.
//!
//! Sources, under `%USERPROFILE%\.codex` unless `EREBUS_CODEX_DIR` says otherwise:
//!
//! - `thread-writer-locks\<thread id>.lock`: a thread is live while its writer holds a byte
//!   lock on this file. The service probes it with a shared try-lock and lets go at once.
//! - `sessions\YYYY\MM\DD\rollout-<ts>-<thread id>.jsonl`, append-only, tailed by byte offset.
//!   Its first record, `session_meta`, names the folder, the client and any parent thread.
//!
//! The service never opens `auth.json`, `secrets\` or the SQLite state.
//!
//! A turn is open from `task_started` until `task_complete` or `turn_aborted`; any item or
//! tool record also means one is under way, so a seed that starts mid-turn still reads busy.
//! An open `request_user_input` call reads as waiting on the person.
//!
//! Sub-agent threads (`thread_source: "subagent"`) are workers of their parent. Guardian
//! reviews are hidden entirely. `codex exec` runs are sessions marked headless.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, TryLockError};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Instant;

use erebus_protocol::{FeedEvent, FeedKind, SessionInfo, SessionStatus, WorkerInfo};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::hub::Event;
use crate::labels::Labels;
use crate::log::{log_info, log_warn};
use crate::server::now_ms;
use crate::sessions::{
    allocate, base_label, basename, clip, command_name, created_ms, parse_ts, phrase, safe_id, Mode, Out, Tail, RESCAN,
    WINDOW,
};

const HARNESS: &str = "codex";

/// A missing rollout triggers a fresh index of `sessions\` at most this often, milliseconds.
const INDEX_EVERY_MS: u64 = 30_000;

/// The longest first line read for `session_meta`.
const META_MAX: u64 = 4 * 1024 * 1024;

/// A thread first seen this soon after its rollout was created is read from the start as live.
const BIRTH_SLACK_MS: u64 = 10_000;

/// Record types that never carry feed or turn facts. Some are megabytes long.
const SKIP: &[&str] = &["world_state", "compacted", "token_usage_record", "retained_context", "session_meta", "turn_context"];

/// How far into a line its `"type"` is looked for.
const PEEK: usize = 160;

pub fn spawn(hub: UnboundedSender<Event>, labels: Labels) {
    let spawned = thread::Builder::new().name("codex".into()).spawn(move || run(hub, labels));
    if let Err(e) = spawned {
        log_warn!("cannot start codex service: {e}");
    }
}

fn run(hub: UnboundedSender<Event>, labels: Labels) {
    let (tx, rx) = mpsc::channel();
    let mut svc = Service::new(Paths::from_env(), labels);
    let mut watch = Watch { tx, w: None, watched: Vec::new(), warned: false };
    let mut window: Option<Instant> = None;
    let mut next_scan = Instant::now();
    loop {
        let due = window.map_or(next_scan, |w| w.min(next_scan));
        let now = Instant::now();
        if due > now {
            match rx.recv_timeout(due - now) {
                Ok(()) => {
                    window.get_or_insert_with(|| Instant::now() + WINDOW);
                    continue;
                }
                Err(RecvTimeoutError::Timeout) => continue,
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
}

impl Paths {
    fn from_env() -> Paths {
        let root = std::env::var_os("EREBUS_CODEX_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".codex")));
        Paths { root }
    }

    fn locks(&self) -> Option<PathBuf> {
        self.root.as_ref().map(|r| r.join("thread-writer-locks"))
    }

    fn sessions(&self) -> Option<PathBuf> {
        self.root.as_ref().map(|r| r.join("sessions"))
    }
}

/// One watcher over the lock and rollout folders. Folders that do not exist yet are added on a
/// later pass.
struct Watch {
    tx: mpsc::Sender<()>,
    w: Option<RecommendedWatcher>,
    watched: Vec<PathBuf>,
    warned: bool,
}

impl Watch {
    fn ensure(&mut self, paths: &Paths) {
        for (dir, mode) in [(paths.locks(), RecursiveMode::NonRecursive), (paths.sessions(), RecursiveMode::Recursive)] {
            let Some(dir) = dir else { continue };
            if self.watched.contains(&dir) || !dir.is_dir() {
                continue;
            }
            if self.w.is_none() {
                let tx = self.tx.clone();
                let made = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                    if let Ok(ev) = res {
                        let wanted = |p: &PathBuf| matches!(p.extension().and_then(|e| e.to_str()), Some("lock" | "jsonl"));
                        if !matches!(ev.kind, EventKind::Access(_)) && ev.paths.iter().any(wanted) {
                            let _ = tx.send(());
                        }
                    }
                });
                match made {
                    Ok(w) => self.w = Some(w),
                    Err(e) => {
                        if !std::mem::replace(&mut self.warned, true) {
                            log_warn!("codex watcher unavailable: {e}; rescanning every 2 s");
                        }
                        return;
                    }
                }
            }
            let Some(w) = self.w.as_mut() else { return };
            match w.watch(&dir, mode) {
                Ok(()) => {
                    log_info!("codex: watching {}", dir.display());
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

/// True while another process holds a lock on `lock`. A shared try-lock fails against the
/// writer's exclusive byte lock; when it succeeds, the lock is dropped at once.
fn held(lock: &Path) -> bool {
    let Ok(f) = File::open(lock) else { return false };
    match f.try_lock_shared() {
        Ok(()) => {
            let _ = f.unlock();
            false
        }
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(_)) => false,
    }
}

/// Thread ids whose writer lock is held, or `None` when the lock folder is missing.
fn live_ids(dir: &Path) -> Option<Vec<String>> {
    let entries = fs::read_dir(dir).ok()?;
    let mut ids = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("lock") {
            continue;
        }
        let Some(id) = p.file_stem().and_then(|s| s.to_str()) else { continue };
        if safe_id(id) && held(&p) {
            ids.push(id.to_owned());
        }
    }
    ids.sort();
    Some(ids)
}

/// The thread id in a rollout file name: `rollout-<timestamp>-<id>.jsonl`, the id a UUID.
fn rollout_id(p: &Path) -> Option<&str> {
    let stem = p.file_stem()?.to_str()?;
    if !stem.starts_with("rollout-") || p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
        return None;
    }
    stem.get(stem.len().checked_sub(36)?..)
}

/// The newest `n` entries of a folder that are folders, by name.
fn newest_dirs(dir: &Path, n: usize) -> Vec<PathBuf> {
    let mut d: Vec<PathBuf> = fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    d.sort();
    d.split_off(d.len().saturating_sub(n))
}

/// The rollout for `id` in the two newest day folders (`YYYY\MM\DD`), which cover a new
/// thread across midnight.
fn recent(sessions: &Path, id: &str) -> Option<PathBuf> {
    let mut days = Vec::new();
    for y in newest_dirs(sessions, 2).into_iter().rev() {
        for m in newest_dirs(&y, 2).into_iter().rev() {
            days.extend(newest_dirs(&m, 2).into_iter().rev());
        }
    }
    days.sort();
    days.into_iter().rev().take(2).find_map(|d| {
        fs::read_dir(d).ok()?.flatten().map(|e| e.path()).find(|p| rollout_id(p) == Some(id))
    })
}

/// Every rollout under `dir` by thread id.
fn index(dir: &Path) -> HashMap<String, PathBuf> {
    let mut out = HashMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Some(id) = rollout_id(&p) {
                out.insert(id.to_owned(), p);
            }
        }
    }
    out
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Meta {
    cwd: String,
    originator: String,
    thread_source: String,
    parent: Option<String>,
    nickname: Option<String>,
    role: Option<String>,
    started_ms: u64,
}

impl Meta {
    fn guardian(&self) -> bool {
        self.thread_source == "guardian_review"
    }

    fn headless(&self) -> bool {
        self.originator == "codex_exec"
    }
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// `session_meta` from a rollout's first line.
fn parse_meta(line: &[u8]) -> Option<Meta> {
    let v: Value = serde_json::from_slice(line).ok()?;
    if str_at(&v, "type") != Some("session_meta") {
        return None;
    }
    let p = v.get("payload")?;
    Some(Meta {
        cwd: str_at(p, "cwd").unwrap_or_default().to_owned(),
        originator: str_at(p, "originator").unwrap_or_default().to_owned(),
        thread_source: str_at(p, "thread_source").unwrap_or_default().to_owned(),
        parent: str_at(p, "parent_thread_id").map(str::to_owned),
        nickname: str_at(p, "agent_nickname").map(str::to_owned),
        role: str_at(p, "agent_role").map(str::to_owned),
        started_ms: str_at(p, "timestamp").or_else(|| str_at(&v, "timestamp")).and_then(parse_ts).unwrap_or(0),
    })
}

fn read_meta(path: &Path) -> Option<Meta> {
    let f = File::open(path).ok()?;
    let mut line = Vec::new();
    BufReader::new(f.take(META_MAX)).read_until(b'\n', &mut line).ok()?;
    parse_meta(&line)
}

/// What one rollout line means.
#[derive(Debug, PartialEq)]
enum Fact {
    TurnStarted,
    /// A turn ended; `true` when the person interrupted it.
    TurnEnded(bool),
    /// Work under way inside a turn, with an optional feed line.
    Item(Option<(FeedKind, String)>),
    Ask(String),
    Answered(String),
}

/// The record type, read from the start of the line without parsing it.
fn peek_type(line: &[u8]) -> Option<&str> {
    let head = std::str::from_utf8(&line[..line.len().min(PEEK)]).ok().or_else(|| {
        let cut = (1..=PEEK.min(line.len())).rev().find(|&n| std::str::from_utf8(&line[..n]).is_ok())?;
        std::str::from_utf8(&line[..cut]).ok()
    })?;
    let at = head.find("\"type\":\"")? + 8;
    let rest = &head[at..];
    Some(&rest[..rest.find('"')?])
}

/// The facts on one line, and its record time.
fn parse_line(line: &[u8]) -> Option<(Fact, u64)> {
    if peek_type(line).is_some_and(|t| SKIP.contains(&t)) {
        return None;
    }
    let v: Value = serde_json::from_slice(line).ok()?;
    let ms = str_at(&v, "timestamp").and_then(parse_ts).unwrap_or(0);
    let p = v.get("payload")?;
    let fact = match (str_at(&v, "type")?, str_at(p, "type").unwrap_or("")) {
        ("event_msg", "task_started") => Fact::TurnStarted,
        ("event_msg", "task_complete") => Fact::TurnEnded(false),
        ("event_msg", "turn_aborted") => Fact::TurnEnded(str_at(p, "reason") == Some("interrupted")),
        ("event_msg", "item_completed") => Fact::Item(p.get("item").and_then(item_line)),
        ("response_item", "function_call") if str_at(p, "name").is_some_and(|n| n.starts_with("request_user_input")) => {
            Fact::Ask(str_at(p, "call_id")?.to_owned())
        }
        ("response_item", "function_call_output") => Fact::Answered(str_at(p, "call_id")?.to_owned()),
        ("response_item", "function_call" | "custom_tool_call" | "reasoning") => Fact::Item(None),
        _ => return None,
    };
    Some((fact, ms))
}

/// The feed line for a finished item: a failure reads as an error, anything else as the tool.
fn item_line(item: &Value) -> Option<(FeedKind, String)> {
    let failed = str_at(item, "status") == Some("failed")
        || item.get("exit_code").and_then(Value::as_i64).is_some_and(|c| c != 0);
    let what = match str_at(item, "type")? {
        "CommandExecution" => {
            let cmd = item
                .get("parsed_cmd")
                .and_then(|c| c.get(0))
                .and_then(|c| str_at(c, "cmd"))
                .or_else(|| item.get("command").and_then(Value::as_array).and_then(|a| a.last()).and_then(Value::as_str));
            cmd.and_then(command_name).unwrap_or_else(|| "command".into())
        }
        "McpToolCall" => {
            let server = str_at(item, "server").unwrap_or("mcp");
            match str_at(item, "tool") {
                Some(t) => format!("{server} {t}"),
                None => server.to_owned(),
            }
        }
        "FileChange" => {
            let changes = item.get("changes").and_then(Value::as_object)?;
            let first = changes.keys().next().map(|k| basename(k).to_owned())?;
            match changes.len() {
                1 => format!("edit {first}"),
                n => format!("edit {first} +{}", n - 1),
            }
        }
        "Extension" if str_at(item, "kind") == Some("web.search") => "search".into(),
        _ => return None,
    };
    Some(if failed { (FeedKind::Error, format!("{what} failed")) } else { (FeedKind::Tool, what) })
}

struct Thread {
    id: String,
    meta: Meta,
    /// Root threads: the label. Sub-agents: their worker number under the parent.
    label: String,
    n: u32,
    /// The session this thread's events belong to: itself, or its parent.
    session: String,
    who: String,
    tail: Tail,
    turn_open: bool,
    asks: HashSet<String>,
}

impl Thread {
    fn sub(&self) -> bool {
        self.session != self.id
    }

    fn emit(&self, out: &mut Out, mode: Mode, ms: u64, kind: FeedKind, what: &str) {
        let worker = self.sub().then_some(self.id.as_str());
        emit(out, mode, ms, kind, (&self.who, &self.session, worker), what);
    }

    /// Reads new lines and applies them.
    fn follow(&mut self, now: u64, out: &mut Out) {
        let (block, mode) = self.tail.read(now);
        for line in block.split(|b| *b == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if let Some((fact, ms)) = parse_line(line) {
                self.apply(fact, ms, mode, out);
            }
        }
    }

    fn apply(&mut self, fact: Fact, ms: u64, mode: Mode, out: &mut Out) {
        match fact {
            Fact::TurnStarted => self.turn_open = true,
            Fact::TurnEnded(interrupted) => {
                self.turn_open = false;
                self.asks.clear();
                if interrupted {
                    self.emit(out, mode, ms, FeedKind::Error, "interrupted");
                } else if !self.sub() {
                    self.emit(out, mode, ms, FeedKind::Turn, "idle");
                }
            }
            Fact::Item(line) => {
                self.turn_open = true;
                if let Some((kind, what)) = line {
                    self.emit(out, mode, ms, kind, &what);
                }
            }
            Fact::Ask(id) => {
                if self.asks.insert(id) {
                    self.emit(out, mode, ms, FeedKind::Perm, "waiting on you");
                }
            }
            Fact::Answered(id) => {
                self.asks.remove(&id);
            }
        }
    }
}

struct Service {
    paths: Paths,
    /// Live root threads and sub-agents, roots in start order.
    threads: Vec<Thread>,
    /// Live guardian threads, hidden.
    hidden: HashSet<String>,
    rollouts: HashMap<String, PathBuf>,
    indexed_ms: Option<u64>,
    first: bool,
    root_seen: Option<bool>,
    sent: Option<Vec<SessionInfo>>,
    labels: Labels,
}

impl Service {
    fn new(paths: Paths, labels: Labels) -> Service {
        Service {
            paths,
            threads: Vec::new(),
            hidden: HashSet::new(),
            rollouts: HashMap::new(),
            indexed_ms: None,
            first: true,
            root_seen: None,
            sent: None,
            labels,
        }
    }

    /// Returns a snapshot when it differs from the last one sent, and the pass's feed events.
    fn pass(&mut self, now: u64) -> (Option<Vec<SessionInfo>>, Vec<FeedEvent>) {
        let mut out = Out::default();
        let first = std::mem::replace(&mut self.first, false);
        let listing = self.paths.locks().and_then(|d| live_ids(&d));
        let seen = listing.is_some();
        if self.root_seen != Some(seen) {
            if seen {
                log_info!("codex: reading thread locks");
            } else {
                log_info!("codex: no thread locks folder; waiting for one");
            }
            self.root_seen = Some(seen);
        }
        let live = listing.unwrap_or_default();
        self.reconcile(&live, first, now, &mut out);
        for t in &mut self.threads {
            t.follow(now, &mut out);
        }
        let infos = self.infos();
        self.labels.set(HARNESS, self.threads.iter().filter(|t| !t.sub()).map(|t| t.label.clone()).collect());
        let changed = self.sent.as_ref() != Some(&infos);
        if changed {
            self.sent = Some(infos.clone());
        }
        (changed.then_some(infos), out.finish())
    }

    /// The rollout for `id`: from the index, else from the newest day folders, which hold
    /// every new thread, else from a fresh index of everything (a resumed old thread).
    fn rollout(&mut self, id: &str, now: u64) -> Option<PathBuf> {
        if let Some(p) = self.rollouts.get(id).filter(|p| p.is_file()) {
            return Some(p.clone());
        }
        if let Some(p) = self.paths.sessions().and_then(|d| recent(&d, id)) {
            self.rollouts.insert(id.to_owned(), p.clone());
            return Some(p);
        }
        if self.indexed_ms.is_some_and(|t| now.saturating_sub(t) < INDEX_EVERY_MS) {
            return None;
        }
        self.indexed_ms = Some(now);
        self.rollouts = self.paths.sessions().map(|d| index(&d)).unwrap_or_default();
        self.rollouts.get(id).cloned()
    }

    /// Ends threads whose lock was let go and starts new ones: roots oldest first, then
    /// sub-agents under them.
    fn reconcile(&mut self, live: &[String], first: bool, now: u64, out: &mut Out) {
        self.hidden.retain(|id| live.contains(id));
        let (keep, gone): (Vec<Thread>, Vec<Thread>) = self.threads.drain(..).partition(|t| live.contains(&t.id));
        self.threads = keep;
        for t in gone {
            if t.sub() {
                t.emit(out, Mode::Live, now, FeedKind::Result, "done");
            } else {
                t.emit(out, Mode::Live, now, FeedKind::Session, "ended");
                log_info!("codex thread ended: {}", t.label);
            }
        }
        // A sub-agent whose parent ended goes with it.
        let roots: HashSet<String> = self.threads.iter().filter(|t| !t.sub()).map(|t| t.id.clone()).collect();
        self.threads.retain(|t| roots.contains(&t.session));

        let mut fresh = Vec::new();
        for id in live {
            if self.hidden.contains(id) || self.threads.iter().any(|t| &t.id == id) {
                continue;
            }
            let Some(path) = self.rollout(id, now) else { continue };
            let Some(meta) = read_meta(&path) else { continue };
            if meta.guardian() {
                self.hidden.insert(id.clone());
                continue;
            }
            fresh.push((id.clone(), path, meta));
        }
        let roots_live = |t: &[Thread], p: &Option<String>| p.as_ref().is_some_and(|p| t.iter().any(|x| !x.sub() && &x.id == p));
        fresh.sort_by_key(|(_, _, m)| (m.parent.is_some(), m.started_ms));
        for (id, path, meta) in fresh {
            let born = created_ms(&path).is_some_and(|c| now.saturating_sub(c) < BIRTH_SLACK_MS);
            let seed = first || !born;
            let mode = if first { Mode::Backlog } else { Mode::Live };
            let tail = Tail::open(path, seed);
            let thread = if roots_live(&self.threads, &meta.parent) && meta.thread_source == "subagent" {
                let parent = meta.parent.clone().unwrap_or_default();
                let n = self.threads.iter().filter(|t| t.session == parent && t.sub()).map(|t| t.n).max().unwrap_or(0) + 1;
                let plabel = self.threads.iter().find(|t| t.id == parent).map(|t| t.label.clone()).unwrap_or_default();
                let t = Thread {
                    id,
                    label: String::new(),
                    n,
                    session: parent,
                    who: format!("{plabel}·w{n}"),
                    meta,
                    tail,
                    turn_open: false,
                    asks: HashSet::new(),
                };
                let kind = t.meta.role.clone().or_else(|| t.meta.nickname.clone()).unwrap_or_else(|| "agent".into());
                let what = format!("spawned {}", clip(&kind, 24));
                emit(out, mode, t.meta.started_ms.max(1), FeedKind::Spawn, (&plabel, &t.session, None), &what);
                t
            } else {
                let others = self.labels.others(HARNESS);
                let taken: Vec<&str> = self.threads.iter().filter(|t| !t.sub()).map(|t| t.label.as_str()).chain(others.iter().map(String::as_str)).collect();
                let label = allocate(&base_label(&meta.cwd, "codex"), &taken);
                let t = Thread {
                    session: id.clone(),
                    id,
                    who: label.clone(),
                    label,
                    n: 0,
                    meta,
                    tail,
                    turn_open: false,
                    asks: HashSet::new(),
                };
                t.emit(out, mode, t.meta.started_ms.max(1), FeedKind::Session, "started");
                log_info!("codex thread started: {} ({})", t.label, t.meta.originator);
                t
            };
            self.threads.push(thread);
        }
    }

    fn infos(&self) -> Vec<SessionInfo> {
        let mut infos: Vec<SessionInfo> = self
            .threads
            .iter()
            .filter(|t| !t.sub())
            .map(|t| {
                let mut subs: Vec<&Thread> = self.threads.iter().filter(|s| s.sub() && s.session == t.id).collect();
                subs.sort_by_key(|s| s.n);
                let busy = t.turn_open || subs.iter().any(|s| s.turn_open);
                let status = if !t.asks.is_empty() {
                    SessionStatus::Waiting
                } else if busy {
                    SessionStatus::Busy
                } else {
                    SessionStatus::Idle
                };
                SessionInfo {
                    id: t.id.clone(),
                    label: t.label.clone(),
                    cwd: t.meta.cwd.clone(),
                    harness: HARNESS.into(),
                    headless: t.meta.headless(),
                    status,
                    started_unix_ms: t.meta.started_ms,
                    workers: subs
                        .iter()
                        .map(|s| WorkerInfo {
                            id: s.id.clone(),
                            label: format!("w{}", s.n),
                            kind: s.meta.role.clone().or_else(|| s.meta.nickname.clone()).unwrap_or_else(|| "agent".into()),
                            started_unix_ms: s.meta.started_ms,
                        })
                        .collect(),
                    pending: None,
                    tasks: Vec::new(),
                    decided: None,
                }
            })
            .collect();
        infos.sort_by(|a, b| (a.started_unix_ms, &a.label).cmp(&(b.started_unix_ms, &b.label)));
        infos
    }
}

/// One feed line from `who`, in `session`, from `worker` when a sub-agent says it.
fn emit(out: &mut Out, mode: Mode, ms: u64, kind: FeedKind, (who, session, worker): (&str, &str, Option<&str>), what: &str) {
    let event = FeedEvent {
        unix_ms: ms,
        kind,
        phrase: phrase(who, what),
        session: session.to_owned(),
        worker: worker.map(str::to_owned),
        backlog: mode == Mode::Backlog,
    };
    out.push(mode, event);
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "01a18809-b5e5-77a3-989a-ffa72e87f01f";
    const SUB: &str = "01a18809-0000-7000-8000-00000000000a";
    const GUARD: &str = "01a18809-0000-7000-8000-00000000000b";
    const EXEC: &str = "01a18809-0000-7000-8000-00000000000c";

    fn meta(id: &str, cwd: &str, originator: &str, source: &str, parent: Option<&str>) -> String {
        let parent = parent.map_or(String::new(), |p| format!(r#","parent_thread_id":"{p}","agent_role":"verifier""#));
        format!(
            r#"{{"timestamp":"2026-10-29T14:22:15.188Z","ordinal":0,"type":"session_meta","payload":{{"id":"{id}","timestamp":"2026-10-29T14:21:40.205Z","cwd":{cwd:?},"originator":"{originator}","thread_source":"{source}"{parent}}}}}"#
        )
    }

    fn event(kind: &str, extra: &str) -> String {
        format!(r#"{{"timestamp":"2026-10-29T14:22:16.000Z","ordinal":1,"type":"event_msg","payload":{{"type":"{kind}"{extra}}}}}"#)
    }

    fn item(body: &str) -> String {
        event("item_completed", &format!(r#","turn_id":"t1","item":{body}"#))
    }

    fn fact(line: &str) -> Option<Fact> {
        parse_line(line.as_bytes()).map(|(f, _)| f)
    }

    const ASK: &str = r#"{"timestamp":"2026-10-29T14:41:54.567Z","type":"response_item","payload":{"type":"function_call","name":"request_user_input","arguments":"{}","call_id":"c1"}}"#;
    const ANSWER: &str = r#"{"timestamp":"2026-10-29T14:42:48.017Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"{}"}}"#;

    #[test]
    fn session_meta_names_the_folder_client_and_parent() {
        let m = parse_meta(meta(ROOT, r"F:\Projects\Work", "codex-tui", "user", None).as_bytes()).unwrap();
        assert_eq!(m.cwd, r"F:\Projects\Work");
        assert!(!m.headless() && !m.guardian() && m.parent.is_none());
        assert_eq!(m.started_ms, parse_ts("2026-10-29T14:21:40.205Z").unwrap());
        let g = parse_meta(meta(GUARD, "x", "codex_work_desktop", "guardian_review", Some(ROOT)).as_bytes()).unwrap();
        assert!(g.guardian());
        let e = parse_meta(meta(EXEC, "x", "codex_exec", "user", None).as_bytes()).unwrap();
        assert!(e.headless());
        assert!(parse_meta(event("task_started", "").as_bytes()).is_none());
    }

    #[test]
    fn turns_open_and_close() {
        assert_eq!(fact(&event("task_started", r#","turn_id":"t1""#)), Some(Fact::TurnStarted));
        assert_eq!(fact(&event("task_complete", r#","turn_id":"t1""#)), Some(Fact::TurnEnded(false)));
        assert_eq!(fact(&event("turn_aborted", r#","turn_id":"t1","reason":"interrupted""#)), Some(Fact::TurnEnded(true)));
        assert_eq!(fact(&event("token_count", "")), None);
    }

    #[test]
    fn finished_items_become_feed_lines() {
        let cmd = item(
            r#"{"type":"CommandExecution","command":["C:\\pwsh.exe","-Command","git status"],"parsed_cmd":[{"type":"unknown","cmd":"git status"}],"status":"completed","exit_code":0}"#,
        );
        assert_eq!(fact(&cmd), Some(Fact::Item(Some((FeedKind::Tool, "git".into())))));
        let bad = item(r#"{"type":"CommandExecution","command":["C:\\pwsh.exe","-Command","cargo test"],"status":"failed","exit_code":1}"#);
        assert_eq!(fact(&bad), Some(Fact::Item(Some((FeedKind::Error, "cargo failed".into())))));
        let mcp = item(r#"{"type":"McpToolCall","server":"cua_repl","tool":"js","status":"failed"}"#);
        assert_eq!(fact(&mcp), Some(Fact::Item(Some((FeedKind::Error, "cua_repl js failed".into())))));
        let edit = item(r#"{"type":"FileChange","changes":{"F:\\Work\\hub\\identity.py":{"type":"update"},"F:\\Work\\b.py":{"type":"add"}},"status":"completed"}"#);
        let Some(Fact::Item(Some((FeedKind::Tool, e)))) = fact(&edit) else { panic!("no edit line") };
        assert!(e.starts_with("edit ") && e.ends_with(" +1"), "{e}");
        let search = item(r#"{"type":"Extension","kind":"web.search","query":"q"}"#);
        assert_eq!(fact(&search), Some(Fact::Item(Some((FeedKind::Tool, "search".into())))));
        assert_eq!(fact(&item(r#"{"type":"AgentMessage"}"#)), Some(Fact::Item(None)));
    }

    #[test]
    fn a_question_is_asked_then_answered() {
        assert_eq!(fact(ASK), Some(Fact::Ask("c1".into())));
        assert_eq!(fact(ANSWER), Some(Fact::Answered("c1".into())));
    }

    #[test]
    fn huge_bookkeeping_records_are_skipped_unparsed() {
        let ws = format!(r#"{{"timestamp":"2026-10-29T14:22:15.549Z","ordinal":8,"type":"world_state","payload":{{"x":"{}"}}}}"#, "a".repeat(100));
        assert_eq!(peek_type(ws.as_bytes()), Some("world_state"));
        assert_eq!(fact(&ws), None);
    }

    #[test]
    fn rollout_names_carry_the_thread_id() {
        let p = PathBuf::from(format!("rollout-2026-10-29T09-21-40-{ROOT}.jsonl"));
        assert_eq!(rollout_id(&p), Some(ROOT));
        assert_eq!(rollout_id(Path::new("notes.jsonl")), None);
    }

    /// A temp `.codex` tree. A thread's lock stays held until it is released.
    struct Tree {
        root: PathBuf,
        day: PathBuf,
        locks: HashMap<String, File>,
    }

    impl Tree {
        fn new(name: &str) -> Tree {
            let root = std::env::temp_dir().join(format!("erebus-codex-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            let day = root.join("sessions").join("2026").join("10").join("29");
            fs::create_dir_all(&day).unwrap();
            fs::create_dir_all(root.join("thread-writer-locks")).unwrap();
            Tree { root, day, locks: HashMap::new() }
        }

        fn lock_path(&self, id: &str) -> PathBuf {
            self.root.join("thread-writer-locks").join(format!("{id}.lock"))
        }

        fn thread(&mut self, id: &str, lines: &[String]) {
            let f = File::create(self.lock_path(id)).unwrap();
            f.lock().unwrap();
            self.locks.insert(id.to_owned(), f);
            self.append(id, lines);
        }

        fn append(&self, id: &str, lines: &[String]) {
            use std::io::Write;
            let p = self.day.join(format!("rollout-2026-10-29T09-21-40-{id}.jsonl"));
            let mut f = fs::OpenOptions::new().create(true).append(true).open(p).unwrap();
            for l in lines {
                writeln!(f, "{l}").unwrap();
            }
        }

        fn release(&mut self, id: &str) {
            self.locks.remove(id);
        }

        fn service(&self, labels: Labels) -> Service {
            Service::new(Paths { root: Some(self.root.clone()) }, labels)
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            self.locks.clear();
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Reads the real `~/.codex` once and prints what the face would get. Run by hand.
    #[test]
    #[ignore]
    fn real_codex_dir() {
        let mut svc = Service::new(Paths::from_env(), Labels::default());
        let t0 = Instant::now();
        let (s, feed) = svc.pass(now_ms());
        println!("pass took {:?}", t0.elapsed());
        for x in s.unwrap_or_default() {
            println!("{} {} {:?} headless={} workers={} cwd={}", x.label, x.harness, x.status, x.headless, x.workers.len(), x.cwd);
        }
        for e in feed.iter().rev().take(8) {
            println!("feed {:?} {}", e.kind, e.phrase);
        }
    }

    #[test]
    fn a_held_lock_is_live_and_a_released_one_is_not() {
        let mut t = Tree::new("lock");
        t.thread(ROOT, &[]);
        assert!(held(&t.lock_path(ROOT)));
        t.release(ROOT);
        assert!(!held(&t.lock_path(ROOT)));
        assert!(!held(&t.root.join("missing.lock")));
    }

    #[test]
    fn threads_show_with_workers_and_hide_guardians() {
        let mut t = Tree::new("service");
        t.thread(ROOT, &[meta(ROOT, r"F:\Projects\Work", "codex-tui", "user", None), event("task_started", r#","turn_id":"t1""#)]);
        t.thread(SUB, &[meta(SUB, r"F:\Projects\Work", "codex-tui", "subagent", Some(ROOT))]);
        t.thread(GUARD, &[meta(GUARD, r"F:\Projects\Work", "codex-tui", "guardian_review", Some(ROOT)), event("task_started", "")]);
        t.thread(EXEC, &[meta(EXEC, r"F:\Projects\Batch", "codex_exec", "user", None)]);
        let labels = Labels::default();
        labels.set("claude", vec!["work".into()]);
        let mut svc = t.service(labels);
        let (s, feed) = svc.pass(1_793_300_000_000);
        let s = s.unwrap();
        assert_eq!(s.len(), 2, "the guardian is hidden");
        let root = s.iter().find(|x| x.id == ROOT).unwrap();
        assert_eq!(root.label, "work-2", "labels share one namespace with Claude");
        assert_eq!(root.harness, "codex");
        assert_eq!(root.status, SessionStatus::Busy);
        assert_eq!(root.workers.len(), 1);
        assert_eq!(root.workers[0].kind, "verifier");
        let exec = s.iter().find(|x| x.id == EXEC).unwrap();
        assert!(exec.headless && exec.status == SessionStatus::Idle);
        assert!(feed.iter().all(|e| e.backlog), "a first pass is history");
        assert!(feed.iter().any(|e| e.phrase.contains("spawned verifier")));
        assert!(feed.iter().all(|e| e.session != GUARD && e.worker.as_deref() != Some(GUARD)));

        t.append(ROOT, &[event("task_complete", r#","turn_id":"t1""#)]);
        t.release(SUB);
        let (s, feed) = svc.pass(1_793_300_002_000);
        let s = s.unwrap();
        let root = s.iter().find(|x| x.id == ROOT).unwrap();
        assert_eq!(root.status, SessionStatus::Idle);
        assert!(root.workers.is_empty());
        let phrases: Vec<&str> = feed.iter().map(|e| e.phrase.as_str()).collect();
        assert!(phrases.contains(&"work-2·w1 · done") && phrases.contains(&"work-2 · idle"), "{phrases:?}");

        t.release(ROOT);
        let (s, feed) = svc.pass(1_793_300_004_000);
        assert!(s.unwrap().iter().all(|x| x.id != ROOT));
        assert!(feed.iter().any(|e| e.phrase == "work-2 · ended"));
    }

    #[test]
    fn a_rollout_written_after_its_lock_is_found_on_the_next_pass() {
        let mut t = Tree::new("late");
        t.thread(EXEC, &[]);
        fs::remove_file(t.day.join(format!("rollout-2026-10-29T09-21-40-{EXEC}.jsonl"))).unwrap();
        let mut svc = t.service(Labels::default());
        let (s, _) = svc.pass(1_793_300_000_000);
        assert!(s.unwrap().is_empty(), "no rollout yet");
        t.append(EXEC, &[meta(EXEC, r"F:\Projects\Batch", "codex_exec", "user", None), event("task_started", "")]);
        let (s, _) = svc.pass(1_793_300_000_500);
        let s = s.unwrap();
        assert_eq!(s.len(), 1, "found well inside the index's 30 s rate limit");
        assert_eq!(s[0].status, SessionStatus::Busy);
    }

    #[test]
    fn a_question_reads_as_waiting_until_answered() {
        let mut t = Tree::new("ask");
        t.thread(ROOT, &[meta(ROOT, r"F:\Projects\Work", "codex-tui", "user", None), event("task_started", ""), ASK.to_owned()]);
        let mut svc = t.service(Labels::default());
        let (s, _) = svc.pass(1_793_300_000_000);
        assert_eq!(s.unwrap()[0].status, SessionStatus::Waiting);
        t.append(ROOT, &[ANSWER.to_owned()]);
        let (s, _) = svc.pass(1_793_300_002_000);
        assert_eq!(s.unwrap()[0].status, SessionStatus::Busy);
    }
}
