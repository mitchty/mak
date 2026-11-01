use std::ffi::OsStr;
use std::path::PathBuf;

/// Resolved once from the hook's PATH so the full command not depend nor needthe PATH of
/// whatever eventually runs it.
#[derive(Debug, Clone)]
pub struct Tools {
    pub rtk: PathBuf,
    pub nix: Option<PathBuf>,
}

impl Tools {
    /// rtk is required, nix is optional and only needed for devshells. I should relax this but its setup in my flake right so whatever.
    pub fn from_path(path: &OsStr) -> Option<Self> {
        Some(Self {
            rtk: resolve("rtk", path)?,
            nix: resolve("nix", path),
        })
    }
}

/// First executable of `name` in `path`, canonicalized so symlinks are resolved to
/// full `/nix/store/...` paths.
pub fn resolve(name: &str, path: &OsStr) -> Option<PathBuf> {
    let found = which::which_in_global(name, Some(path)).ok()?.next()?;
    std::fs::canonicalize(found).ok()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::path::Path;

    use super::*;

    fn make_file(dir: &Path, name: &str, mode: u32) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, "#!/bin/sh\n").expect("write");
        fs::set_permissions(&p, fs::Permissions::from_mode(mode)).expect("chmod");
        p
    }

    fn join(dirs: &[&Path]) -> std::ffi::OsString {
        std::env::join_paths(dirs).expect("join_paths")
    }

    #[test]
    fn finds_executable_and_follows_symlinks() {
        let store = tempfile::tempdir().expect("tempdir");
        let profile = tempfile::tempdir().expect("tempdir");
        let real = make_file(store.path(), "rtk-real", 0o755);
        symlink(&real, profile.path().join("rtk")).expect("symlink");

        let found = resolve("rtk", &join(&[profile.path()])).expect("found");

        assert_eq!(found, fs::canonicalize(real).expect("canonical"));
    }

    #[test]
    fn skips_files_that_are_not_executable() {
        let dir = tempfile::tempdir().expect("tempdir");
        make_file(dir.path(), "rtk", 0o644);

        assert_eq!(resolve("rtk", &join(&[dir.path()])), None);
    }

    #[test]
    fn earlier_path_entry_wins() {
        let first = tempfile::tempdir().expect("tempdir");
        let second = tempfile::tempdir().expect("tempdir");
        let want = make_file(first.path(), "rtk", 0o755);
        make_file(second.path(), "rtk", 0o755);

        let found = resolve("rtk", &join(&[first.path(), second.path()])).expect("found");

        assert_eq!(found, fs::canonicalize(want).expect("canonical"));
    }

    #[test]
    fn missing_name_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(resolve("rtk", &join(&[dir.path()])), None);
        assert_eq!(resolve("rtk", OsStr::new("")), None);
    }

    #[test]
    fn rtk_is_required_but_nix_is_optional() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(Tools::from_path(&join(&[dir.path()])).is_none());

        make_file(dir.path(), "rtk", 0o755);
        let tools = Tools::from_path(&join(&[dir.path()])).expect("rtk found");
        assert!(tools.nix.is_none());

        make_file(dir.path(), "nix", 0o755);
        let tools = Tools::from_path(&join(&[dir.path()])).expect("both found");
        assert!(tools.nix.is_some());
    }
}
