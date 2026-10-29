//! `config.toml`: the workspaces and their projects.
//!
//! The file lives at `%APPDATA%\Erebus\config.toml`; `EREBUS_CONFIG` overrides the path. A
//! missing file is created from [`DEFAULT_TOML`]. An invalid file is logged and the last good
//! config stays in force.

use std::fs;
use std::path::{Path, PathBuf};

use erebus_protocol::{Context, GitState, Project};
use serde::Deserialize;

use crate::log::{log_error, log_info, log_warn};

/// Written when no config exists. Parsed to produce the built-in defaults, so the two cannot drift.
pub const DEFAULT_TOML: &str = r#"# Erebus config. Edits apply live; an invalid file is ignored until fixed.
#
# active = "<workspace name>"        which workspace is current
# [[workspace]]                      one block per workspace, in display order
# name = "<name>"                    unique, non-empty
# [workspace.project]                optional; omit for a workspace with no project
# name = "<project name>"
# path = '<absolute path>'           use single quotes so backslashes stay literal

active = "dev"

[[workspace]]
name = "dev"
[workspace.project]
name = "Erebus"
path = 'F:\Projects\Erebus'

[[workspace]]
name = "reverse-eng"

[[workspace]]
name = "art"

[[workspace]]
name = "local-ai"
"#;

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub active: String,
    #[serde(rename = "workspace")]
    pub workspaces: Vec<Workspace>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    pub name: String,
    pub project: Option<ProjectCfg>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectCfg {
    pub name: String,
    pub path: String,
}

impl Config {
    pub fn parse(text: &str) -> Result<Config, String> {
        let cfg: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn defaults() -> Config {
        Config::parse(DEFAULT_TOML).expect("built-in config is valid")
    }

    fn validate(&self) -> Result<(), String> {
        if self.workspaces.is_empty() {
            return Err("no [[workspace]] defined".into());
        }
        for (i, w) in self.workspaces.iter().enumerate() {
            if w.name.trim().is_empty() {
                return Err(format!("workspace #{} has an empty name", i + 1));
            }
            if self.workspaces[..i].iter().any(|o| o.name == w.name) {
                return Err(format!("duplicate workspace \"{}\"", w.name));
            }
            if let Some(p) = &w.project {
                if p.name.trim().is_empty() || p.path.trim().is_empty() {
                    return Err(format!("workspace \"{}\" has a project with an empty name or path", w.name));
                }
            }
        }
        if !self.workspaces.iter().any(|w| w.name == self.active) {
            return Err(format!("active workspace \"{}\" is not defined", self.active));
        }
        Ok(())
    }

    pub fn active_project(&self) -> Option<&ProjectCfg> {
        self.workspaces.iter().find(|w| w.name == self.active)?.project.as_ref()
    }

    /// The context for this config, with `git` as the active project's state.
    pub fn context(&self, git: Option<GitState>) -> Context {
        Context {
            workspace: self.active.clone(),
            workspaces: self.workspaces.iter().map(|w| w.name.clone()).collect(),
            project: self.active_project().map(|p| Project { name: p.name.clone(), path: p.path.clone(), git }),
        }
    }
}

/// `EREBUS_CONFIG`, else `%APPDATA%\Erebus\config.toml`.
pub fn path() -> Option<PathBuf> {
    match std::env::var_os("EREBUS_CONFIG") {
        Some(p) if !p.is_empty() => Some(PathBuf::from(p)),
        _ => Some(PathBuf::from(std::env::var_os("APPDATA")?).join("Erebus").join("config.toml")),
    }
}

fn load(path: &Path) -> Result<Config, String> {
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    Config::parse(&text)
}

/// The config in force. Holds the last good value across failed reloads.
pub struct ConfigState {
    path: Option<PathBuf>,
    good: Config,
}

impl ConfigState {
    /// Loads `path`, creating it with defaults first when missing.
    pub fn open(path: Option<PathBuf>) -> ConfigState {
        let Some(p) = path else {
            log_warn!("config: no path (APPDATA unset); using defaults");
            return ConfigState { path: None, good: Config::defaults() };
        };
        if !p.exists() {
            let made = p.parent().map_or(Ok(()), fs::create_dir_all).and_then(|()| fs::write(&p, DEFAULT_TOML));
            match made {
                Ok(()) => log_info!("config: wrote defaults to {}", p.display()),
                Err(e) => log_warn!("config: cannot create {}: {e}", p.display()),
            }
        }
        let good = load(&p).unwrap_or_else(|e| {
            log_error!("config invalid at {}: {e}; using defaults", p.display());
            Config::defaults()
        });
        ConfigState { path: Some(p), good }
    }

    pub fn get(&self) -> &Config {
        &self.good
    }

    /// Rereads the file. On any error, logs it and keeps the last good config.
    pub fn reload(&mut self) {
        let Some(p) = &self.path else { return };
        match load(p) {
            Ok(c) if c != self.good => {
                log_info!("config: reloaded {}", p.display());
                self.good = c;
            }
            Ok(_) => {}
            Err(e) => log_error!("config invalid at {}: {e}; keeping last good", p.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh path inside a not-yet-created directory.
    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("erebus-cfg-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("sub").join("config.toml")
    }

    fn cleanup(p: &Path) {
        let _ = fs::remove_dir_all(p.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn defaults_match_the_spec() {
        let c = Config::defaults();
        assert_eq!(c.active, "dev");
        let names: Vec<_> = c.workspaces.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["dev", "reverse-eng", "art", "local-ai"]);
        let p = c.active_project().unwrap();
        assert_eq!((p.name.as_str(), p.path.as_str()), ("Erebus", r"F:\Projects\Erebus"));
        assert!(c.workspaces[1..].iter().all(|w| w.project.is_none()));
    }

    #[test]
    fn missing_file_is_created_with_defaults() {
        let p = temp("missing");
        let state = ConfigState::open(Some(p.clone()));
        assert_eq!(state.get(), &Config::defaults());
        assert_eq!(fs::read_to_string(&p).unwrap(), DEFAULT_TOML);
        cleanup(&p);
    }

    #[test]
    fn invalid_file_keeps_last_good() {
        let p = temp("invalid");
        let mut state = ConfigState::open(Some(p.clone()));
        fs::write(&p, "active = \"art\"\n[[workspace]]\nname = \"art\"\n").unwrap();
        state.reload();
        assert_eq!(state.get().active, "art");

        let bad = [
            "not toml [[[",
            "active = \"ghost\"\n[[workspace]]\nname = \"art\"\n",
            "active = \"art\"\nbogus = 1\n[[workspace]]\nname = \"art\"\n",
            "",
        ];
        for text in bad {
            fs::write(&p, text).unwrap();
            state.reload();
            assert_eq!(state.get().active, "art", "bad config replaced the good one: {text:?}");
        }
        fs::remove_file(&p).unwrap();
        state.reload();
        assert_eq!(state.get().active, "art");
        cleanup(&p);
    }

    #[test]
    fn invalid_at_startup_uses_defaults_without_overwriting() {
        let p = temp("badstart");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, "oops").unwrap();
        let state = ConfigState::open(Some(p.clone()));
        assert_eq!(state.get(), &Config::defaults());
        assert_eq!(fs::read_to_string(&p).unwrap(), "oops");
        cleanup(&p);
    }

    #[test]
    fn context_lists_workspaces_and_project() {
        let c = Config::defaults().context(None);
        assert_eq!(c.workspace, "dev");
        assert_eq!(c.workspaces.len(), 4);
        assert_eq!(c.project.unwrap().name, "Erebus");
    }
}
