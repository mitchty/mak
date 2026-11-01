use std::path::{Path, PathBuf};

use serde_json::json;

use crate::input::HookInput;
use crate::quote::{is_simple, shell_quote};
use crate::tools::Tools;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Rewritten { command: String, notes: Vec<String> },
    Unchanged { reason: String },
}

impl Decision {
    /// Stdout for eca to work, `{}` means "no changes" without being an explicit error.
    pub fn to_hook_json(&self) -> String {
        match self {
            Self::Rewritten { command, .. } => {
                json!({ "updatedInput": { "command": command } }).to_string()
            }
            Self::Unchanged { .. } => "{}".to_owned(),
        }
    }
}

/// Wrap the command in rtk and uses the flake devshell when the working directory's repo has one.
///
/// Anything that isn't a single plain command goes through `sh -c` as a chungus quoted argument, so
/// `a && b` can't escape rtk or the devshell the way a bare string prefix would.
///
/// TODO: Future enchancements might be to have rtk wrap more of this stuff but I need to think on it more.
pub fn rewrite(input: &HookInput, tools: &Tools) -> Decision {
    let cmd = input.tool_input.command.as_str();
    if cmd.trim().is_empty() {
        return Decision::Unchanged {
            reason: "empty command".to_owned(),
        };
    }

    let rtk_path = tools.rtk.to_string_lossy();
    if already_wrapped(cmd, &rtk_path) {
        return Decision::Unchanged {
            reason: "already runs through rtk".to_owned(),
        };
    }

    let rtk = shell_quote(&rtk_path);
    let inner = if is_simple(cmd) {
        format!("{rtk} {cmd}")
    } else {
        format!("{rtk} sh -c {}", shell_quote(cmd))
    };

    let mut notes = Vec::new();
    let command = match input.start_dir().as_deref().and_then(find_flake_root) {
        Some(root) => match &tools.nix {
            Some(nix) => format!(
                "{} develop {} --command {inner}",
                shell_quote(&nix.to_string_lossy()),
                shell_quote(&root.to_string_lossy()),
            ),
            None => {
                notes.push(format!(
                    "flake.nix at {} but nix not on PATH, devshell skipped",
                    root.display()
                ));
                inner
            }
        },
        None => inner,
    };

    Decision::Rewritten { command, notes }
}

fn already_wrapped(cmd: &str, rtk_path: &str) -> bool {
    cmd.contains(rtk_path)
        || cmd
            .split_whitespace()
            .next()
            .and_then(|w| Path::new(w).file_name())
            .is_some_and(|name| name == "rtk")
}

/// The git root of `start`, iff contains `flake.nix`.
///
/// `.git` may be a file in say worktrees or submodules, so this checks existence rather than if its a directory.
pub fn find_flake_root(start: &Path) -> Option<PathBuf> {
    let root = start.ancestors().find(|dir| dir.join(".git").exists())?;
    root.join("flake.nix").is_file().then(|| root.to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    const RTK: &str = "/nix/store/aaa-rtk/bin/rtk";
    const NIX: &str = "/nix/store/bbb-nix/bin/nix";

    fn tools() -> Tools {
        Tools {
            rtk: RTK.into(),
            nix: Some(NIX.into()),
        }
    }

    fn input(cmd: &str, wd: Option<&Path>, cwd: Option<&Path>) -> HookInput {
        let v = json!({
            "cwd": cwd,
            "tool_input": { "command": cmd, "working_directory": wd },
        });
        HookInput::parse(&v.to_string()).expect("test input parses")
    }

    fn repo(flake: bool) -> TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join(".git")).expect("mkdir .git");
        if flake {
            fs::write(dir.path().join("flake.nix"), "{}").expect("write flake");
        }
        dir
    }

    fn command(d: Decision) -> String {
        match d {
            Decision::Rewritten { command, .. } => command,
            other => panic!("expected a rewrite, got {other:?}"),
        }
    }

    fn quoted(p: &Path) -> String {
        shell_quote(&p.to_string_lossy())
    }

    #[test]
    fn simple_command_without_flake_gets_a_bare_prefix() {
        let d = repo(false);
        let got = command(rewrite(
            &input("git status", Some(d.path()), None),
            &tools(),
        ));
        assert_eq!(got, format!("{RTK} git status"));
    }

    #[test]
    fn simple_command_in_flake_repo_runs_under_nix_develop() {
        let d = repo(true);
        let got = command(rewrite(
            &input("git status", Some(d.path()), None),
            &tools(),
        ));
        assert_eq!(
            got,
            format!(
                "{NIX} develop {} --command {RTK} git status",
                quoted(d.path())
            )
        );
    }

    #[test]
    fn compound_command_is_one_quoted_argument() {
        let d = repo(false);
        let got = command(rewrite(
            &input("cd x && foo", Some(d.path()), None),
            &tools(),
        ));
        assert_eq!(got, format!("{RTK} sh -c 'cd x && foo'"));
    }

    #[test]
    fn compound_command_stays_inside_the_devshell() {
        let d = repo(true);
        let got = command(rewrite(&input("a | b", Some(d.path()), None), &tools()));
        assert_eq!(
            got,
            format!(
                "{NIX} develop {} --command {RTK} sh -c 'a | b'",
                quoted(d.path())
            )
        );
    }

    #[test]
    fn builtins_and_assignments_take_the_shell_path() {
        let d = repo(false);
        let cd = command(rewrite(&input("cd foo", Some(d.path()), None), &tools()));
        assert_eq!(cd, format!("{RTK} sh -c 'cd foo'"));
        let env = command(rewrite(
            &input("FOO=1 make", Some(d.path()), None),
            &tools(),
        ));
        assert_eq!(env, format!("{RTK} sh -c 'FOO=1 make'"));
    }

    #[test]
    fn working_directory_beats_cwd_when_picking_the_flake() {
        let plain = repo(false);
        let flaked = repo(true);
        let got = command(rewrite(
            &input("ls", Some(flaked.path()), Some(plain.path())),
            &tools(),
        ));
        assert_eq!(
            got,
            format!("{NIX} develop {} --command {RTK} ls", quoted(flaked.path()))
        );
    }

    #[test]
    fn relative_working_directory_resolves_against_cwd() {
        let d = repo(true);
        fs::create_dir(d.path().join("sub")).expect("mkdir sub");
        let got = command(rewrite(
            &input("ls", Some(Path::new("sub")), Some(d.path())),
            &tools(),
        ));
        assert_eq!(
            got,
            format!("{NIX} develop {} --command {RTK} ls", quoted(d.path()))
        );
    }

    #[test]
    fn nested_directories_find_the_repo_root() {
        let d = repo(true);
        let deep = d.path().join("a/b/c");
        fs::create_dir_all(&deep).expect("mkdir deep");
        assert_eq!(find_flake_root(&deep), Some(d.path().to_path_buf()));
    }

    #[test]
    fn git_worktree_style_dot_git_file_counts_as_a_root() {
        let d = tempfile::tempdir().expect("tempdir");
        fs::write(d.path().join(".git"), "gitdir: /elsewhere").expect("write .git");
        fs::write(d.path().join("flake.nix"), "{}").expect("write flake");
        assert_eq!(find_flake_root(d.path()), Some(d.path().to_path_buf()));
    }

    #[test]
    fn repo_without_flake_has_no_flake_root() {
        let d = repo(false);
        assert_eq!(find_flake_root(d.path()), None);
    }

    #[test]
    fn flake_without_git_is_not_a_root() {
        let d = tempfile::tempdir().expect("tempdir");
        fs::write(d.path().join("flake.nix"), "{}").expect("write flake");
        assert_eq!(find_flake_root(d.path()), None);
    }

    #[test]
    fn missing_nix_skips_the_devshell_and_says_so() {
        let d = repo(true);
        let tools = Tools {
            rtk: RTK.into(),
            nix: None,
        };
        let got = rewrite(&input("ls", Some(d.path()), None), &tools);
        match got {
            Decision::Rewritten { command, notes } => {
                assert_eq!(command, format!("{RTK} ls"));
                assert_eq!(notes.len(), 1);
                assert!(notes[0].contains("devshell skipped"), "{notes:?}");
            }
            other => panic!("expected a rewrite, got {other:?}"),
        }
    }

    #[test]
    fn already_wrapped_commands_are_left_alone() {
        let d = repo(true);
        for cmd in [
            format!("{RTK} git status"),
            "rtk git status".to_owned(),
            "/some/other/store/rtk ls".to_owned(),
            format!("{NIX} develop . --command {RTK} ls"),
        ] {
            let got = rewrite(&input(&cmd, Some(d.path()), None), &tools());
            assert!(
                matches!(got, Decision::Unchanged { .. }),
                "{cmd:?} -> {got:?}"
            );
        }
    }

    #[test]
    fn empty_commands_are_left_alone() {
        let got = rewrite(&input("  ", None, None), &tools());
        assert!(matches!(got, Decision::Unchanged { .. }));
    }

    #[test]
    fn hook_json_matches_the_eca_contract() {
        let rewritten = Decision::Rewritten {
            command: "rtk ls".to_owned(),
            notes: vec![],
        };
        let v: serde_json::Value = serde_json::from_str(&rewritten.to_hook_json()).expect("json");
        assert_eq!(v, json!({ "updatedInput": { "command": "rtk ls" } }));

        let unchanged = Decision::Unchanged {
            reason: "x".to_owned(),
        };
        assert_eq!(unchanged.to_hook_json(), "{}");
    }

    #[test]
    fn rewritten_compound_command_runs_end_to_end() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = dir.path().join("rtk");
        fs::write(&fake, "#!/bin/sh\nexec \"$@\"\n").expect("write fake rtk");
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).expect("chmod");
        let tools = Tools {
            rtk: fake,
            nix: None,
        };

        let cmd = command(rewrite(
            &input("echo one && cd / && pwd", Some(dir.path()), None),
            &tools,
        ));
        let out = Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .output()
            .expect("sh runs");

        assert_eq!(String::from_utf8_lossy(&out.stdout), "one\n/\n");
    }
}
