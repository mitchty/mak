use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The subset of ECA's `preToolCall` stdin JSON that ecahook cares about, unknown fields/inputs are ignored.
#[derive(Debug, Deserialize)]
pub struct HookInput {
    pub tool_input: ToolInput,
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
pub struct ToolInput {
    pub command: String,
    pub working_directory: Option<PathBuf>,
}

impl HookInput {
    pub fn parse(raw: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(raw)
    }

    /// Directory for thed command: `working_directory`, relative inputs resolve against `cwd`, otherwise else `cwd`.
    pub fn start_dir(&self) -> Option<PathBuf> {
        let wd = non_empty(self.tool_input.working_directory.as_deref());
        let cwd = non_empty(self.cwd.as_deref());
        match (wd, cwd) {
            // Path::join with an absolute `wd` replaces `cwd`
            (Some(wd), Some(cwd)) => Some(cwd.join(wd)),
            (Some(wd), None) => Some(wd.to_path_buf()),
            (None, cwd) => cwd.map(Path::to_path_buf),
        }
    }
}

fn non_empty(p: Option<&Path>) -> Option<&Path> {
    p.filter(|p| !p.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_input_and_ignores_unknown_fields() {
        let raw = r#"{"hook_name":"rtk-wrap","tool_name":"shell_command","tool_input":{"command":"ls","timeout":5},"cwd":"/w"}"#;
        let input = HookInput::parse(raw).expect("parses");
        assert_eq!(input.tool_input.command, "ls");
        assert_eq!(input.cwd.as_deref(), Some(Path::new("/w")));
    }

    #[test]
    fn rejects_input_without_a_command() {
        assert!(HookInput::parse(r#"{"tool_input":{}}"#).is_err());
        assert!(HookInput::parse("nope").is_err());
    }

    #[test]
    fn null_and_empty_working_directory_fall_back_to_cwd() {
        for wd in ["null", r#""""#] {
            let raw = format!(
                r#"{{"tool_input":{{"command":"ls","working_directory":{wd}}},"cwd":"/w"}}"#
            );
            let input = HookInput::parse(&raw).expect("parses");
            assert_eq!(input.start_dir(), Some(PathBuf::from("/w")));
        }
    }

    #[test]
    fn absolute_working_directory_wins_over_cwd() {
        let raw = r#"{"tool_input":{"command":"ls","working_directory":"/other"},"cwd":"/w"}"#;
        let input = HookInput::parse(raw).expect("parses");
        assert_eq!(input.start_dir(), Some(PathBuf::from("/other")));
    }

    #[test]
    fn relative_working_directory_resolves_against_cwd() {
        let raw = r#"{"tool_input":{"command":"ls","working_directory":"sub/dir"},"cwd":"/w"}"#;
        let input = HookInput::parse(raw).expect("parses");
        assert_eq!(input.start_dir(), Some(PathBuf::from("/w/sub/dir")));
    }

    #[test]
    fn no_directories_means_no_start_dir() {
        let input = HookInput::parse(r#"{"tool_input":{"command":"ls"}}"#).expect("parses");
        assert_eq!(input.start_dir(), None);
    }
}
