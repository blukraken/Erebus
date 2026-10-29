//! The context service. Owns the config and the active project's git state, and hands the hub a
//! fresh [`Context`] whenever either changes.
//!
//! Nothing polls. `ReadDirectoryChangesW` (through `notify`) wakes the service on a file change,
//! the service waits for the burst to settle, then reads the config and runs git once.

use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant as StdInstant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use erebus_protocol::Context;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::time::{sleep_until, Instant};

use crate::config::{self, ConfigState};
use crate::git;
use crate::hub::Event;
use crate::log::{log_info, log_warn};

/// Quiet time after the last change before the service re-reads anything.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// Directories whose contents never count as a project change (build output and caches).
const IGNORED_DIRS: &[&str] = &["target", "node_modules", ".vs", "__pycache__", ".gradle"];

/// Entries of `.git` that change HEAD, refs or the index.
const GIT_STATE: &[&str] = &["HEAD", "index", "packed-refs", "refs"];

pub fn spawn(hub: UnboundedSender<Event>) {
    tokio::spawn(run(hub));
}

enum Signal {
    Config,
    Tree,
}

async fn run(hub: UnboundedSender<Event>) {
    let path = config::path();
    let mut cfg = ConfigState::open(path.clone());
    let (tx, mut rx) = unbounded_channel();
    let _config_watch = path.as_deref().and_then(|p| watch_config(p, tx.clone()));
    let mut tree: Option<(PathBuf, RecommendedWatcher)> = None;
    let mut config_dirty = false;
    let mut git_runs = 0u64;
    let mut due = Some(Instant::now());

    loop {
        tokio::select! {
            sig = rx.recv() => {
                match sig {
                    Some(Signal::Config) => config_dirty = true,
                    Some(Signal::Tree) => {}
                    None => return,
                }
                due = Some(Instant::now() + DEBOUNCE);
            }
            () = async { sleep_until(due.unwrap_or_else(Instant::now)).await }, if due.is_some() => {
                due = None;
                if std::mem::take(&mut config_dirty) {
                    cfg.reload();
                }
                let project = cfg.get().active_project().map(|p| PathBuf::from(&p.path));
                if tree.as_ref().map(|(p, _)| p) != project.as_ref() {
                    tree = project.as_ref().and_then(|p| watch_tree(p, tx.clone()).map(|w| (p.clone(), w)));
                }
                let git = match project {
                    Some(p) => {
                        git_runs += 1;
                        let started = StdInstant::now();
                        let state = tokio::task::spawn_blocking(move || git::status(&p)).await.ok().flatten();
                        log_info!("git run #{git_runs} took {} ms", started.elapsed().as_millis());
                        state
                    }
                    None => None,
                };
                if hub.send(Event::Context(cfg.get().context(git))).is_err() {
                    return;
                }
            }
        }
    }
}

fn watch_config(path: &Path, tx: UnboundedSender<Signal>) -> Option<RecommendedWatcher> {
    let dir = path.parent()?;
    let name = path.file_name()?.to_owned();
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            if !matches!(ev.kind, EventKind::Access(_)) && ev.paths.iter().any(|p| p.file_name() == Some(&name)) {
                let _ = tx.send(Signal::Config);
            }
        }
    })
    .map_err(|e| log_warn!("config watcher unavailable: {e}"))
    .ok()?;
    w.watch(dir, RecursiveMode::NonRecursive).map_err(|e| log_warn!("cannot watch {}: {e}", dir.display())).ok()?;
    Some(w)
}

fn watch_tree(root: &Path, tx: UnboundedSender<Signal>) -> Option<RecommendedWatcher> {
    let base = root.to_owned();
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            if !matches!(ev.kind, EventKind::Access(_)) && ev.paths.iter().any(|p| relevant(&base, p)) {
                let _ = tx.send(Signal::Tree);
            }
        }
    })
    .map_err(|e| log_warn!("project watcher unavailable: {e}"))
    .ok()?;
    w.watch(root, RecursiveMode::Recursive).map_err(|e| log_warn!("cannot watch {}: {e}", root.display())).ok()?;
    Some(w)
}

/// Whether a change at `path` under `root` can alter the git state.
fn relevant(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else { return true };
    let parts: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    match parts.first().map(String::as_str) {
        Some(".git") => {
            let Some(entry) = parts.get(1) else { return false };
            GIT_STATE.iter().any(|s| s.eq_ignore_ascii_case(entry)) && !parts.last().is_some_and(|l| l.ends_with(".lock"))
        }
        _ => !parts.iter().any(|p| IGNORED_DIRS.contains(&p.as_str())),
    }
}

/// One log line for a context, e.g. `dev · Erebus · main@348c109 · 2 changed`.
pub fn summary(c: &Context) -> String {
    let mut s = c.workspace.clone();
    let Some(p) = &c.project else { return format!("{s} · no project") };
    s.push_str(&format!(" · {}", p.name));
    let Some(g) = &p.git else { return format!("{s} · no git") };
    let branch = g.branch.as_deref().unwrap_or("HEAD");
    s.push_str(&format!(" · {branch}@{}", g.head.as_deref().unwrap_or("unborn")));
    s.push_str(&format!(" · {} changed", g.changed));
    match (g.ahead.unwrap_or(0), g.behind.unwrap_or(0)) {
        (0, 0) => {}
        (a, b) => s.push_str(&format!(" · +{a}/-{b}")),
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use erebus_protocol::{GitState, Project};

    fn rel(p: &str) -> bool {
        relevant(Path::new(r"C:\proj"), &Path::new(r"C:\proj").join(p))
    }

    #[test]
    fn working_tree_changes_count() {
        assert!(rel(r"src\main.rs"));
        assert!(rel("README.md"));
        assert!(rel(r"docs\target.md"));
    }

    #[test]
    fn build_output_is_ignored() {
        assert!(!rel(r"app\target\release\x.exe"));
        assert!(!rel(r"target\debug\.fingerprint\a"));
        assert!(!rel(r"web\node_modules\pkg\index.js"));
    }

    #[test]
    fn only_head_refs_and_index_count_inside_git() {
        assert!(rel(r".git\HEAD"));
        assert!(rel(r".git\index"));
        assert!(rel(r".git\refs\heads\main"));
        assert!(rel(r".git\packed-refs"));
        assert!(!rel(r".git\index.lock"));
        assert!(!rel(r".git\refs\heads\main.lock"));
        assert!(!rel(r".git\objects\ab\cdef"));
        assert!(!rel(r".git\logs\HEAD"));
        assert!(!rel(r".git\FETCH_HEAD"));
        assert!(!rel(".git"));
    }

    #[test]
    fn summary_lines() {
        let git = GitState { branch: Some("main".into()), head: Some("348c109".into()), changed: 2, files: vec![], ahead: None, behind: None };
        let mut c = Context {
            workspace: "dev".into(),
            workspaces: vec!["dev".into()],
            project: Some(Project { name: "Erebus".into(), path: "x".into(), git: Some(git) }),
        };
        assert_eq!(summary(&c), "dev · Erebus · main@348c109 · 2 changed");
        c.project.as_mut().unwrap().git.as_mut().unwrap().ahead = Some(1);
        assert_eq!(summary(&c), "dev · Erebus · main@348c109 · 2 changed · +1/-0");
        c.project.as_mut().unwrap().git = None;
        assert_eq!(summary(&c), "dev · Erebus · no git");
        c.project = None;
        assert_eq!(summary(&c), "dev · no project");
    }
}
