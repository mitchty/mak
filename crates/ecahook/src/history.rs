use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Serialize;
use serde_json::Value;

use crate::rewrite::Decision;

#[derive(Debug, Serialize)]
pub struct Entry {
    pub ts: String,
    pub version: &'static str,
    pub input: Value,
    pub outcome: &'static str,
    pub command: Option<String>,
    pub notes: Vec<String>,
}

impl Entry {
    pub fn from_decision(raw: &str, decision: &Decision) -> Self {
        match decision {
            Decision::Rewritten { command, notes } => {
                Self::new(raw, "rewritten", Some(command.clone()), notes.clone())
            }
            Decision::Unchanged { reason } => {
                Self::new(raw, "unchanged", None, vec![reason.clone()])
            }
        }
    }

    pub fn from_error(raw: &str, err: &str) -> Self {
        Self::new(raw, "error", None, vec![err.to_owned()])
    }

    fn new(raw: &str, outcome: &'static str, command: Option<String>, notes: Vec<String>) -> Self {
        Self {
            ts: humantime::format_rfc3339_seconds(SystemTime::now()).to_string(),
            version: env!("CARGO_PKG_VERSION"),
            input: serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned())),
            outcome,
            command,
            notes,
        }
    }
}

/// `$XDG_CACHE_HOME/ecahook/history.log` or  `~/.cache/ecahook/history.log`. Creates parent directories if missing.
pub fn history_path() -> io::Result<PathBuf> {
    xdg::BaseDirectories::with_prefix("ecahook").place_cache_file("history.log")
}

/// One `O_APPEND` write per entry and commands can hold secrets so the file is 0600 though not sure it'll matter much.
pub fn append(path: &Path, entry: &Entry) -> io::Result<()> {
    let mut line = serde_json::to_string(entry).map_err(io::Error::other)?;
    line.push('\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(line.as_bytes())
}

/// Write failures don't fail the hook wholesale.
pub fn record(entry: &Entry) {
    if let Err(e) = history_path().and_then(|p| append(&p, entry)) {
        eprintln!("ecahook: history not written: {e}");
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn appends_one_json_object_per_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.log");
        let raw = r#"{"tool_input":{"command":"ls"},"cwd":"/w"}"#;
        let rewritten = Decision::Rewritten {
            command: "rtk ls".to_owned(),
            notes: vec![],
        };

        append(&path, &Entry::from_decision(raw, &rewritten)).expect("first append");
        append(&path, &Entry::from_error("garbage", "invalid hook input")).expect("second append");

        let text = fs::read_to_string(&path).expect("read log");
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).expect("each line is json"))
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["outcome"], "rewritten");
        assert_eq!(lines[0]["command"], "rtk ls");
        assert_eq!(lines[0]["input"]["tool_input"]["command"], "ls");
        assert_eq!(lines[1]["outcome"], "error");
        assert_eq!(lines[1]["input"], "garbage");
    }

    #[test]
    fn log_file_is_private() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.log");
        append(&path, &Entry::from_error("x", "e")).expect("append");

        let mode = fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn unchanged_decisions_keep_their_reason() {
        let d = Decision::Unchanged {
            reason: "already runs through rtk".to_owned(),
        };
        let e = Entry::from_decision("{}", &d);
        assert_eq!(e.outcome, "unchanged");
        assert_eq!(e.command, None);
        assert_eq!(e.notes, vec!["already runs through rtk".to_owned()]);
    }
}
