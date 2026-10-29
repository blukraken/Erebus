//! Reads a project's git state by running `git status --porcelain=v2 --branch`.

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use pinnos_protocol::{ChangedFile, FileStatus, GitState, MAX_CHANGED_FILES};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The state of the repo at `path`, or `None` when git is missing or `path` is not a repo.
///
/// `--no-optional-locks` keeps `status` from rewriting `.git/index`, which the watcher would
/// otherwise see as a change and loop on.
pub fn status(path: &Path) -> Option<GitState> {
    let out = Command::new("git")
        .args(["--no-optional-locks", "-c", "core.quotePath=false", "status", "--porcelain=v2", "--branch"])
        .current_dir(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(parse(&String::from_utf8_lossy(&out.stdout)))
}

/// Parses `--porcelain=v2 --branch` output.
///
/// `changed` counts every entry git reports. `files` holds the first `MAX_CHANGED_FILES` by
/// path. An untracked directory is one entry whose path ends in `/`.
pub fn parse(text: &str) -> GitState {
    let mut state = GitState { branch: None, head: None, changed: 0, files: Vec::new(), ahead: None, behind: None };
    for line in text.lines() {
        if let Some(h) = line.strip_prefix("# branch.") {
            let (key, val) = h.split_once(' ').unwrap_or((h, ""));
            match key {
                "oid" if val != "(initial)" => state.head = Some(val.chars().take(7).collect()),
                "head" if val != "(detached)" => state.branch = Some(val.to_owned()),
                "ab" => {
                    let mut it = val.split(' ');
                    let num = |s: Option<&str>, sign: char| s?.strip_prefix(sign)?.parse::<u32>().ok();
                    state.ahead = num(it.next(), '+');
                    state.behind = num(it.next(), '-');
                }
                _ => {}
            }
        } else if let Some(file) = entry(line) {
            state.changed += 1;
            state.files.push(file);
        }
    }
    state.files.sort_by(|a, b| a.path.cmp(&b.path));
    state.files.truncate(MAX_CHANGED_FILES);
    state
}

/// One changed-file entry (`1`, `2`, `u` or `?`), or `None` for any other line.
fn entry(line: &str) -> Option<ChangedFile> {
    let (kind, rest) = line.split_once(' ')?;
    let (status, path) = match kind {
        // `1 XY sub mH mI mW hH hI path`
        "1" => {
            let mut f = rest.splitn(8, ' ');
            let xy = f.next()?;
            let path = f.nth(6)?;
            let status = if xy.contains('D') {
                FileStatus::Deleted
            } else if xy.starts_with('A') {
                FileStatus::Added
            } else {
                FileStatus::Modified
            };
            (status, path)
        }
        // `2 XY sub mH mI mW hH hI score newpath<TAB>oldpath`
        "2" => (FileStatus::Renamed, rest.splitn(9, ' ').nth(8)?.split('\t').next()?),
        // `u XY sub m1 m2 m3 mW h1 h2 h3 path`
        "u" => (FileStatus::Conflicted, rest.splitn(10, ' ').nth(9)?),
        "?" => (FileStatus::Untracked, rest),
        _ => return None,
    };
    Some(ChangedFile { path: path.to_owned(), status })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OID: &str = "348c109aa1f3d6b1e7c0f0b4d5a6e7f8091a2b3c";

    #[test]
    fn clean_branch() {
        let s = parse(&format!("# branch.oid {OID}\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +0 -0\n"));
        let want = GitState {
            branch: Some("main".into()),
            head: Some("348c109".into()),
            changed: 0,
            files: vec![],
            ahead: Some(0),
            behind: Some(0),
        };
        assert_eq!(s, want);
    }

    fn cf(path: &str, status: FileStatus) -> ChangedFile {
        ChangedFile { path: path.into(), status }
    }

    fn one(xy: &str, path: &str) -> String {
        format!("1 {xy} N... 100644 100644 100644 aaaa bbbb {path}\n")
    }

    #[test]
    fn mixed_statuses() {
        let s = parse(&format!(
            "# branch.oid {OID}\n# branch.head dev\n\
             1 .M N... 100644 100644 100644 aaaa bbbb src/a.rs\n\
             1 A. N... 000000 100644 100644 0000 cccc src/new.rs\n\
             1 .D N... 100644 100644 000000 aaaa aaaa gone.rs\n\
             2 R. N... 100644 100644 100644 dddd eeee R100 moved.rs\told.rs\n\
             u UU N... 100644 100644 100644 100644 1 2 3 conflict.rs\n\
             ? untracked.txt\n! ignored.log\n"
        ));
        assert_eq!(s.changed, 6);
        assert_eq!(s.branch.as_deref(), Some("dev"));
        assert_eq!(
            s.files,
            vec![
                cf("conflict.rs", FileStatus::Conflicted),
                cf("gone.rs", FileStatus::Deleted),
                cf("moved.rs", FileStatus::Renamed),
                cf("src/a.rs", FileStatus::Modified),
                cf("src/new.rs", FileStatus::Added),
                cf("untracked.txt", FileStatus::Untracked),
            ]
        );
    }

    #[test]
    fn xy_mapping() {
        let status = |xy: &str| parse(&one(xy, "f")).files[0].status;
        assert_eq!(status("MM"), FileStatus::Modified);
        assert_eq!(status("M."), FileStatus::Modified);
        assert_eq!(status("AM"), FileStatus::Added);
        assert_eq!(status("AD"), FileStatus::Deleted);
        assert_eq!(status("D."), FileStatus::Deleted);
        assert_eq!(status(".D"), FileStatus::Deleted);
        assert_eq!(status(".A"), FileStatus::Modified);
    }

    #[test]
    fn staged_then_modified_is_modified() {
        let s = parse(&one("MM", "src/lib.rs"));
        assert_eq!(s.files, vec![cf("src/lib.rs", FileStatus::Modified)]);
    }

    #[test]
    fn paths_with_spaces_survive() {
        let s = parse(&format!("{}2 R. N... 1 2 3 d e R90 new name.rs\told name.rs\n? a b/\n", one(".M", "my file.rs")));
        let paths: Vec<_> = s.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a b/", "my file.rs", "new name.rs"]);
    }

    #[test]
    fn files_are_sorted_by_path() {
        let s = parse("? z.txt\n? a.txt\n? m/\n? B.txt\n");
        let paths: Vec<_> = s.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["B.txt", "a.txt", "m/", "z.txt"]);
    }

    #[test]
    fn files_are_capped_but_changed_is_not() {
        let text: String = (0..100).map(|i| format!("? f{i:03}.txt\n")).collect();
        let s = parse(&text);
        assert_eq!(s.changed, 100);
        assert_eq!(s.files.len(), MAX_CHANGED_FILES);
        assert_eq!(s.files[0].path, "f000.txt");
        assert_eq!(s.files[MAX_CHANGED_FILES - 1].path, format!("f{:03}.txt", MAX_CHANGED_FILES - 1));
    }

    #[test]
    fn cap_keeps_the_first_by_path_not_by_arrival() {
        let mut text: String = (0..MAX_CHANGED_FILES).map(|i| format!("? m{i:03}.txt\n")).collect();
        text.push_str("? a-late.txt\n");
        let s = parse(&text);
        assert_eq!(s.files[0].path, "a-late.txt");
        assert_eq!(s.files.len(), MAX_CHANGED_FILES);
    }

    #[test]
    fn untracked_directory_is_one_entry() {
        let s = parse("# branch.oid (initial)\n# branch.head main\n? newdir/\n? top.txt\n");
        assert_eq!(s.changed, 2);
        assert_eq!(s.files, vec![cf("newdir/", FileStatus::Untracked), cf("top.txt", FileStatus::Untracked)]);
    }

    #[test]
    fn detached_head() {
        let s = parse(&format!("# branch.oid {OID}\n# branch.head (detached)\n"));
        assert_eq!(s.branch, None);
        assert_eq!(s.head.as_deref(), Some("348c109"));
    }

    #[test]
    fn no_upstream() {
        let s = parse(&format!("# branch.oid {OID}\n# branch.head main\n? x\n"));
        assert_eq!((s.ahead, s.behind, s.changed), (None, None, 1));
    }

    #[test]
    fn ahead_and_behind() {
        let s = parse(&format!("# branch.oid {OID}\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +3 -12\n"));
        assert_eq!((s.ahead, s.behind), (Some(3), Some(12)));
    }

    #[test]
    fn initial_repo_without_commits() {
        let s = parse("# branch.oid (initial)\n# branch.head main\n? first.txt\n");
        let want = GitState {
            branch: Some("main".into()),
            head: None,
            changed: 1,
            files: vec![cf("first.txt", FileStatus::Untracked)],
            ahead: None,
            behind: None,
        };
        assert_eq!(s, want);
    }

    #[test]
    fn missing_dir_gives_none() {
        let dir = std::env::temp_dir().join(format!("pinnos-nogit-{}-missing", std::process::id()));
        assert!(status(&dir).is_none());
    }
}
