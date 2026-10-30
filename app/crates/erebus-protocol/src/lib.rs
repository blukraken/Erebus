//! Messages between `erebus-core` and `erebus-face`.
//!
//! Transport: the Windows named pipe at [`PIPE_NAME`]. Each frame is a 4-byte little-endian
//! length followed by that many bytes of UTF-8 JSON. The face connects as a client; the core
//! serves. Either side may send at any time.

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

pub const PIPE_NAME: &str = r"\\.\pipe\erebus-core";

/// Bumped on any breaking change. Peers with different versions refuse each other.
pub const PROTOCOL_VERSION: u32 = 7;

/// Most changed files listed in [`GitState::files`]; the rest only count toward `changed`.
pub const MAX_CHANGED_FILES: usize = 64;

/// Frames larger than this are rejected to bound memory on a corrupt stream.
pub const MAX_FRAME: u32 = 4 * 1024 * 1024;

/// How often the core sends [`CoreMsg::Heartbeat`]. The face treats the link as lost after
/// three missed beats.
pub const HEARTBEAT_MS: u64 = 1000;

/// Vitals samples the core keeps and replays to a face on connect. One sample per second.
pub const VITALS_HISTORY: usize = 90;

/// Feed events the core keeps and replays to a face on connect.
pub const FEED_HISTORY: usize = 32;

/// Longest [`FeedEvent::phrase`], in characters.
pub const MAX_PHRASE: usize = 40;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum CoreMsg {
    /// First frame the core sends on a new connection.
    Hello { protocol: u32, core_version: String, pid: u32, started_unix_ms: u64 },
    Heartbeat { seq: u64, unix_ms: u64 },
    /// Show and focus the window (tray "Open Erebus" or icon double-click).
    Show,
    /// Exit the face process. The core keeps running.
    Exit,
    /// The current context. Sent after `Hello` and again whenever any field changes.
    Context(Context),
    /// System vitals. Sent after `Hello` with up to [`VITALS_HISTORY`] samples, then once a
    /// second with the newest sample only.
    Vitals(Vitals),
    /// Every live agent session on the machine. Sent after `Hello` and again whenever any
    /// session starts, ends or changes.
    Sessions { sessions: Vec<SessionInfo> },
    /// Session events, oldest first. Sent after `Hello` with up to [`FEED_HISTORY`] events,
    /// then as they happen.
    Feed { events: Vec<FeedEvent> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// The harness's own session id.
    pub id: String,
    /// Short name the feed uses, e.g. "erebus" or "erebus-2".
    pub label: String,
    /// Working folder the session runs in.
    pub cwd: String,
    /// "claude" for Claude Code, "codex" for Codex.
    pub harness: String,
    /// Started by a script or tool, with no one at a terminal (`codex exec`).
    #[serde(default)]
    pub headless: bool,
    pub status: SessionStatus,
    pub started_unix_ms: u64,
    /// Workers the session spawned and still runs.
    pub workers: Vec<WorkerInfo>,
    /// A permission prompt waiting on the person, after a short settle.
    pub pending: Option<Pending>,
    /// Background shells and monitors the session runs, oldest first.
    #[serde(default)]
    pub tasks: Vec<TaskInfo>,
    /// How the last shown permission prompt was answered, for a few seconds after.
    #[serde(default)]
    pub decided: Option<Decided>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decided {
    pub tool_use_id: String,
    pub granted: bool,
    pub unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskInfo {
    /// The tool call that started it.
    pub id: String,
    pub kind: TaskKind,
    /// The program a shell runs, or what a monitor watches.
    pub label: String,
    pub started_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Shell,
    Monitor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Busy,
    Idle,
    /// Between turns, waiting on an outside program the session started.
    Waiting,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerInfo {
    /// The harness's agent id.
    pub id: String,
    /// Short name in spawn order, e.g. "w1".
    pub label: String,
    /// Agent type, e.g. "Explore".
    pub kind: String,
    pub started_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub tool_use_id: String,
    /// Tool name, e.g. "Bash".
    pub tool: String,
    /// The worker that asked, or `None` for the session itself.
    pub worker: Option<String>,
    pub since_unix_ms: u64,
    /// What the call asks to do, on one line: the command, the file or the host.
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedEvent {
    pub unix_ms: u64,
    pub kind: FeedKind,
    /// `<who> · <what>`, at most [`MAX_PHRASE`] characters; never a full command or file
    /// contents.
    pub phrase: String,
    pub session: String,
    /// The worker it came from, or `None` for the session itself.
    pub worker: Option<String>,
    /// Read from history when the core first saw the session; drawn without motion.
    pub backlog: bool,
}

/// What a feed line reports; each kind has its own colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedKind {
    /// A session started or ended.
    Session,
    /// A worker was spawned.
    Spawn,
    /// A tool call started.
    Tool,
    /// A tool call returned.
    Result,
    /// A tool call failed or a turn was interrupted.
    Error,
    /// A permission was granted or denied.
    Perm,
    /// The context was compacted.
    Compact,
    /// A skill was invoked.
    Skill,
    /// A turn ended: idle when no worker or background task still runs.
    Turn,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Vitals {
    pub hw: Hardware,
    /// Oldest first.
    pub samples: Vec<Sample>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hardware {
    pub ram_total_mb: u32,
    /// Logical cores; the length of every [`Sample::cores`].
    pub cores: u16,
    /// `None` when NVML is unavailable.
    pub gpu: Option<GpuInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GpuInfo {
    pub name: String,
    pub vram_total_mb: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub unix_ms: u64,
    pub ram_used_mb: u32,
    /// All cores, 0..=100.
    pub cpu: f32,
    /// Per logical core, 0..=100.
    pub cores: Vec<u8>,
    /// `None` when NVML is unavailable or the read failed.
    pub gpu: Option<GpuSample>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuSample {
    /// 0..=100.
    pub util: u8,
    pub vram_used_mb: u32,
    pub temp_c: u8,
    pub power_w: u16,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Context {
    /// Active workspace name, e.g. "dev".
    pub workspace: String,
    /// All configured workspace names, in config order.
    pub workspaces: Vec<String>,
    /// Active project, if the workspace has one.
    pub project: Option<Project>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    pub path: String,
    /// `None` when the path is not a git repository or git is unavailable.
    pub git: Option<GitState>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GitState {
    /// Branch name, or `None` on a detached HEAD.
    pub branch: Option<String>,
    /// Abbreviated HEAD commit, or `None` before the first commit.
    pub head: Option<String>,
    /// Files with staged, unstaged or untracked changes.
    pub changed: u32,
    /// Up to [`MAX_CHANGED_FILES`] of those files, sorted by path.
    pub files: Vec<ChangedFile>,
    /// Commits ahead of upstream, or `None` without an upstream.
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    /// Repo-relative path with forward slashes. For renames, the new path.
    pub path: String,
    pub status: FileStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum FaceMsg {
    /// First frame the face sends on a new connection.
    Hello { protocol: u32, face_version: String, pid: u32 },
    /// The window is now hidden to the tray (close button, Ctrl+Q, Alt+F4).
    Hidden,
    /// The window is visible again.
    Shown,
}

pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    let len = u32::try_from(body.len()).map_err(io::Error::other)?;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip() {
        let msg = CoreMsg::Heartbeat { seq: 7, unix_ms: 42 };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let back: CoreMsg = read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn context_round_trip() {
        let msg = CoreMsg::Context(Context {
            workspace: "dev".into(),
            workspaces: vec!["dev".into(), "art".into()],
            project: Some(Project {
                name: "Erebus".into(),
                path: r"F:\Projects\Erebus".into(),
                git: Some(GitState {
                    branch: Some("main".into()),
                    head: Some("348c109".into()),
                    changed: 2,
                    files: vec![
                        ChangedFile { path: "docs/components.md".into(), status: FileStatus::Modified },
                        ChangedFile { path: "notes.txt".into(), status: FileStatus::Untracked },
                    ],
                    ahead: None,
                    behind: None,
                }),
            }),
        });
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let back: CoreMsg = read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn vitals_round_trip() {
        let msg = CoreMsg::Vitals(Vitals {
            hw: Hardware {
                ram_total_mb: 31_640,
                cores: 4,
                gpu: Some(GpuInfo { name: "NVIDIA GeForce RTX 5080".into(), vram_total_mb: 16_303 }),
            },
            samples: vec![
                Sample { unix_ms: 1_000, ram_used_mb: 14_200, cpu: 12.5, cores: vec![10, 20, 5, 15], gpu: None },
                Sample {
                    unix_ms: 2_000,
                    ram_used_mb: 14_250,
                    cpu: 22.0,
                    cores: vec![30, 20, 18, 20],
                    gpu: Some(GpuSample { util: 63, vram_used_mb: 9_100, temp_c: 71, power_w: 288 }),
                },
            ],
        });
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let back: CoreMsg = read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn sessions_and_feed_round_trip() {
        let session = SessionInfo {
            id: "aabbb6cd".into(),
            label: "erebus".into(),
            cwd: r"F:\Projects\Erebus".into(),
            harness: "claude".into(),
            headless: false,
            status: SessionStatus::Busy,
            started_unix_ms: 1_000,
            workers: vec![WorkerInfo { id: "a2f6".into(), label: "w1".into(), kind: "Explore".into(), started_unix_ms: 2_000 }],
            pending: Some(Pending { tool_use_id: "toolu_1".into(), tool: "Bash".into(), worker: None, since_unix_ms: 3_000, summary: Some("cargo test".into()) }),
            decided: Some(Decided { tool_use_id: "toolu_0".into(), granted: false, unix_ms: 2_900 }),
            tasks: vec![TaskInfo { id: "toolu_2".into(), kind: TaskKind::Monitor, label: "build log".into(), started_unix_ms: 2_500 }],
        };
        // A snapshot from before tasks still reads, with none.
        let mut old = serde_json::to_value(&session).unwrap();
        old.as_object_mut().unwrap().remove("tasks");
        old.as_object_mut().unwrap().remove("decided");
        old["pending"].as_object_mut().unwrap().remove("summary");
        let back: SessionInfo = serde_json::from_value(old).unwrap();
        assert!(back.tasks.is_empty() && back.decided.is_none() && back.pending.unwrap().summary.is_none());
        let event = FeedEvent {
            unix_ms: 4_000,
            kind: FeedKind::Tool,
            phrase: "erebus · Bash cargo".into(),
            session: "aabbb6cd".into(),
            worker: None,
            backlog: false,
        };
        for msg in [CoreMsg::Sessions { sessions: vec![session] }, CoreMsg::Feed { events: vec![event] }] {
            let mut buf = Vec::new();
            write_frame(&mut buf, &msg).unwrap();
            let back: CoreMsg = read_frame(&mut buf.as_slice()).unwrap();
            assert_eq!(back, msg);
        }
    }

    #[test]
    fn rejects_oversized_frame() {
        let mut buf = (MAX_FRAME + 1).to_le_bytes().to_vec();
        buf.extend([0u8; 8]);
        let err = read_frame::<_, CoreMsg>(&mut buf.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
