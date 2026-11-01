/// Bourne shell builtins and reserved words can't be exec'd by `rtk <word>`, so they need a shell/sh -c wrapper. Though rtk cd doesn't make sense methinks.
const SHELL_ONLY: &[&str] = &[
    "!", "{", "}", "[[", ".", "alias", "bg", "break", "builtin", "case", "cd", "command",
    "continue", "declare", "do", "done", "elif", "else", "esac", "eval", "exec", "exit", "export",
    "fg", "fi", "for", "function", "if", "in", "jobs", "let", "local", "popd", "pushd", "read",
    "readonly", "return", "select", "set", "shift", "source", "then", "time", "trap", "typeset",
    "ulimit", "umask", "unalias", "unset", "until", "wait", "while",
];

/// Quote input as a single shell word, words made only of safe characters stay plain for logs.
pub fn shell_quote(s: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if !s.is_empty() && s.chars().all(safe) {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// True only when `cmd` is a single external command with no shell syntax that rtk would freak over.
///
/// False negatives are fine as they just use `sh -c`. A false positive would let an
/// operator like `&&` escape wrongly, so anything not for sure correct counts as not simple.
pub fn is_simple(cmd: &str) -> bool {
    let Some(first) = cmd.split_whitespace().next() else {
        return false;
    };
    !SHELL_ONLY.contains(&first) && !is_assignment(first) && !has_shell_syntax(cmd)
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Scan for anything outside of quotes that needs a real shell. Unterminated quotes count as syntax error.
fn has_shell_syntax(cmd: &str) -> bool {
    let mut it = cmd.chars().peekable();
    let mut word_start = true;
    while let Some(c) = it.next() {
        match c {
            '\'' => loop {
                match it.next() {
                    Some('\'') => break,
                    Some(_) => {}
                    None => return true,
                }
            },
            '"' => loop {
                match it.next() {
                    Some('"') => break,
                    Some('\\') => {
                        it.next();
                    }
                    Some('`') => return true,
                    Some('$') if it.peek() == Some(&'(') => return true,
                    Some(_) => {}
                    None => return true,
                }
            },
            '\\' => {
                if it.next().is_none_or(|n| n == '\n') {
                    return true;
                }
            }
            ';' | '&' | '|' | '<' | '>' | '(' | ')' | '`' | '\n' | '\r' => return true,
            '$' if it.peek() == Some(&'(') => return true,
            '#' if word_start => return true,
            _ => {}
        }
        word_start = c.is_whitespace();
    }
    false
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn through_sh(quoted: &str) -> String {
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {quoted}"))
            .output()
            .expect("sh runs");
        String::from_utf8(out.stdout).expect("utf8")
    }

    #[test]
    fn safe_words_stay_bare() {
        assert_eq!(
            shell_quote("/nix/store/abc-rtk/bin/rtk"),
            "/nix/store/abc-rtk/bin/rtk"
        );
        assert_eq!(shell_quote("--command"), "--command");
    }

    #[test]
    fn empty_and_spaced_words_get_quoted() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn quoting_round_trips_through_a_real_shell() {
        for s in [
            "",
            "plain",
            "a b",
            "it's",
            "'",
            "''",
            "$HOME",
            "`date`",
            "$(date)",
            "back\\slash",
            "*",
            "a;b && c | d",
            "line1\nline2",
            "\"dq\"",
            "tab\there",
            "ünïcode",
        ] {
            assert_eq!(through_sh(&shell_quote(s)), s, "round trip of {s:?}");
        }
    }

    #[test]
    fn plain_commands_are_simple() {
        for cmd in [
            "git status",
            "ls -la /tmp",
            "grep foo file.txt",
            "echo 'a; b && c'",
            r#"echo "a && b | c""#,
            "echo $HOME",
            "  cargo test  ",
        ] {
            assert!(is_simple(cmd), "{cmd:?} should be simple");
        }
    }

    #[test]
    fn shell_syntax_is_not_simple() {
        for cmd in [
            "",
            "   ",
            "cd x && foo",
            "a | b",
            "a; b",
            "ls &",
            "echo hi > f",
            "cat < f",
            "echo $(date)",
            "echo `date`",
            r#"echo "$(date)""#,
            r#"echo "`date`""#,
            "(a)",
            "a\nb",
            "# comment",
            "echo 'unterminated",
            "echo \"unterminated",
            "echo trailing\\",
        ] {
            assert!(!is_simple(cmd), "{cmd:?} should not be simple");
        }
    }

    #[test]
    fn builtins_and_assignments_need_a_shell() {
        for cmd in [
            "cd /tmp",
            "export A=1",
            "source env.sh",
            ". env.sh",
            "FOO=1 make",
            "_x9=y cmd",
            "if true",
            "time ls",
        ] {
            assert!(!is_simple(cmd), "{cmd:?} should not be simple");
        }
    }

    #[test]
    fn equals_in_later_arguments_is_not_an_assignment() {
        assert!(is_simple("make FOO=1"));
        assert!(is_simple("env -i ls"));
    }
}
