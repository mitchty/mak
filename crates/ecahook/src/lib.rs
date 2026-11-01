use std::ffi::OsStr;

use input::HookInput;
use rewrite::Decision;
use tools::Tools;

pub mod history;
pub mod input;
pub mod quote;
pub mod rewrite;
pub mod tools;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid hook input: {0}")]
    Input(#[from] serde_json::Error),
    #[error("rtk not found on PATH")]
    RtkMissing,
}

/// `path` is the hook's own PATH, passed in so tests don't need the hook's process state/PATH.
pub fn run(raw: &str, path: &OsStr) -> Result<(Decision, String), Error> {
    let input = HookInput::parse(raw)?;
    let tools = Tools::from_path(path).ok_or(Error::RtkMissing)?;
    let decision = rewrite::rewrite(&input, &tools);
    let out = decision.to_hook_json();
    Ok((decision, out))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn path_with_rtk() -> (tempfile::TempDir, OsString) {
        let dir = tempfile::tempdir().expect("tempdir");
        let rtk = dir.path().join("rtk");
        fs::write(&rtk, "#!/bin/sh\nexec \"$@\"\n").expect("write rtk");
        fs::set_permissions(&rtk, fs::Permissions::from_mode(0o755)).expect("chmod rtk");
        let path = dir.path().as_os_str().to_owned();
        (dir, path)
    }

    #[test]
    fn run_emits_updated_input_json() {
        let (_dir, path) = path_with_rtk();
        let raw = r#"{"tool_input":{"command":"git status"},"cwd":"/nonexistent"}"#;

        let (decision, out) = run(raw, &path).expect("run succeeds");

        assert!(matches!(decision, Decision::Rewritten { .. }));
        let v: serde_json::Value = serde_json::from_str(&out).expect("stdout is json");
        let cmd = v["updatedInput"]["command"]
            .as_str()
            .expect("command string");
        assert!(cmd.ends_with("/rtk git status"), "got {cmd}");
    }

    #[test]
    fn run_rejects_malformed_input() {
        let (_dir, path) = path_with_rtk();
        assert!(matches!(run("not json", &path), Err(Error::Input(_))));
    }

    #[test]
    fn run_fails_open_when_rtk_is_missing() {
        let raw = r#"{"tool_input":{"command":"ls"}}"#;
        assert!(matches!(run(raw, OsStr::new("")), Err(Error::RtkMissing)));
    }
}
