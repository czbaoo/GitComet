use super::GixRepo;
use crate::util::{run_git_capture_bytes, run_git_with_output};
use gitcomet_core::domain::CommitId;
use gitcomet_core::error::{Error, ErrorKind};
use gitcomet_core::services::{CommandOutput, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

/// `patch` in a temp file for `git apply`, which reads it by path.
pub(super) fn write_patch_file(patch: &[u8]) -> Result<NamedTempFile> {
    let mut file = NamedTempFile::new().map_err(|e| Error::new(ErrorKind::Io(e.kind())))?;
    file.write_all(patch)
        .map_err(|e| Error::new(ErrorKind::Io(e.kind())))?;
    Ok(file)
}

/// NUL-terminated paths for `--pathspec-from-file` with `--pathspec-file-nul`;
/// a long path list would overflow a Windows command line.
pub(super) fn write_pathspec_file<'a>(
    paths: impl IntoIterator<Item = &'a [u8]>,
) -> Result<NamedTempFile> {
    let mut bytes = Vec::new();
    for path in paths {
        bytes.extend_from_slice(path);
        bytes.push(0);
    }
    write_patch_file(&bytes)
}

impl GixRepo {
    pub(super) fn export_patch_with_output_impl(
        &self,
        commit_id: &CommitId,
        dest: &Path,
    ) -> Result<CommandOutput> {
        let sha = commit_id.as_ref();
        let mut cmd = self.git_workdir_cmd();
        cmd.arg("format-patch")
            .arg("-1")
            .arg(sha)
            .arg("--stdout")
            .arg("--binary");
        // The bytes as git wrote them: file content in any encoding must reach
        // the patch file unchanged, or `git am` cannot apply it.
        let patch = run_git_capture_bytes(cmd, &format!("git format-patch -1 {sha} --stdout"))?;
        std::fs::write(dest, patch).map_err(|e| Error::new(ErrorKind::Io(e.kind())))?;
        Ok(CommandOutput {
            command: format!("Export patch {sha}"),
            stdout: format!("Saved patch to {}", dest.display()),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }

    pub(super) fn apply_patch_with_output_impl(&self, patch: &Path) -> Result<CommandOutput> {
        let mut cmd = self.git_workdir_cmd();
        cmd.arg("am").arg("--3way").arg("--").arg(patch);
        run_git_with_output(cmd, &format!("git am --3way {}", patch.display()))
    }

    pub(super) fn apply_unified_patch_to_index_with_output_impl(
        &self,
        patch: &[u8],
        reverse: bool,
    ) -> Result<CommandOutput> {
        let tmp_file = write_patch_file(patch)?;
        let tmp_path = tmp_file.path();

        let mut cmd = self.git_workdir_cmd();
        cmd.arg("apply")
            .arg("--cached")
            .arg("--recount")
            .arg("--whitespace=nowarn");
        if reverse {
            cmd.arg("--reverse");
        }
        cmd.arg(tmp_path);

        let label = if reverse {
            format!("git apply --cached --reverse {}", tmp_path.display())
        } else {
            format!("git apply --cached {}", tmp_path.display())
        };

        let result = run_git_with_output(cmd, &label)?;
        // The index changed (if the apply succeeded), so the next status reload
        // can update the staged cache incrementally instead of re-walking the
        // whole tree↔index diff. Record the touched path(s). Quoted paths
        // (spaces / special characters) are left to the full recompute.
        if result.exit_code == Some(0) {
            if let Some(paths) = affected_paths_from_patch(patch) {
                *self
                    .pending_affected_paths
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(paths);
            }
        }
        Ok(result)
    }

    pub(super) fn apply_unified_patch_to_worktree_with_output_impl(
        &self,
        patch: &[u8],
        reverse: bool,
    ) -> Result<CommandOutput> {
        let tmp_file = write_patch_file(patch)?;
        let tmp_path = tmp_file.path();

        let mut cmd = self.git_workdir_cmd();
        cmd.arg("apply").arg("--recount").arg("--whitespace=nowarn");
        if reverse {
            cmd.arg("--reverse");
        }
        cmd.arg(tmp_path);

        let label = if reverse {
            format!("git apply --reverse {}", tmp_path.display())
        } else {
            format!("git apply {}", tmp_path.display())
        };

        run_git_with_output(cmd, &label)
    }
}

/// Extracts the `a/` and `b/` paths from a `diff --git a/X b/Y` header so a
/// stage/unstage reload can update exactly those paths in the staged cache. Both
/// sides are returned (they differ for a rename, which the incremental update
/// reports as Added + Deleted rather than Renamed — content-correct). Returns
/// `None` for quoted paths or unparseable input, which falls back to a full
/// staged recompute.
fn affected_paths_from_patch(patch: &[u8]) -> Option<Vec<PathBuf>> {
    let text = std::str::from_utf8(patch).ok()?;
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut saw_quoted = false;
    for line in text.lines() {
        if !line.starts_with("diff --git ") {
            continue;
        }
        let rest = &line["diff --git ".len()..];
        // Quoted paths (spaces / special characters) can't be split reliably on
        // spaces, and we can't tell whether other headers were also quoted, so
        // bail and let the caller do a full staged recompute.
        if rest.contains('"') {
            saw_quoted = true;
            continue;
        }
        let mut parts = rest.split(' ');
        let (Some(a), Some(b)) = (parts.next(), parts.next()) else {
            continue;
        };
        let a = a.strip_prefix("a/").unwrap_or(a);
        let b = b.strip_prefix("b/").unwrap_or(b);
        paths.push(PathBuf::from(b));
        if a != b {
            paths.push(PathBuf::from(a));
        }
    }
    // We skipped at least one path we couldn't parse; don't claim to know every
    // touched path, or an unrecorded path would go stale until the next full
    // reload.
    if saw_quoted {
        return None;
    }
    if paths.is_empty() {
        return None;
    }
    paths.sort();
    paths.dedup();
    Some(paths)
}

#[cfg(test)]
mod tests {
    use super::affected_paths_from_patch;

    fn paths_of(patch: &str) -> Option<Vec<String>> {
        affected_paths_from_patch(patch.as_bytes()).map(|ps| {
            let mut v: Vec<String> = ps.iter().map(|p| p.to_string_lossy().into_owned()).collect();
            v.sort();
            v
        })
    }

    #[test]
    fn single_file_returns_both_sides_when_renamed() {
        let patch = "diff --git a/old.txt b/new.txt\n--- a/old.txt\n+++ b/new.txt\n@@ -1 +1 @@\n-old\n+new\n";
        let got = paths_of(patch);
        assert_eq!(got, Some(vec!["new.txt".to_string(), "old.txt".to_string()]));
    }

    #[test]
    fn plain_modified_file_returns_single_path() {
        let patch = "diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(paths_of(patch), Some(vec!["src/main.rs".to_string()]));
    }

    #[test]
    fn multi_file_patch_records_every_touched_path() {
        // Regression: a bulk stage produced a multi-file patch; the parser must
        // record all of them, not just the first, or the rest go stale.
        let patch = concat!(
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-x\n+y\n",
            "diff --git a/b.txt b/b.txt\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-x\n+y\n",
            "diff --git a/c.txt b/c.txt\n--- a/c.txt\n+++ b/c.txt\n@@ -1 +1 @@\n-x\n+y\n",
        );
        let mut got = paths_of(patch).unwrap();
        got.sort();
        assert_eq!(
            got,
            vec!["a.txt".to_string(), "b.txt".to_string(), "c.txt".to_string()]
        );
    }

    #[test]
    fn quoted_path_falls_back_to_full_recompute() {
        let patch = "diff --git \"a/with space.txt\" \"b/with space.txt\"\n";
        assert_eq!(paths_of(patch), None);
    }

    #[test]
    fn non_patch_input_returns_none() {
        assert_eq!(paths_of("just some text\nno diff headers\n"), None);
        assert_eq!(paths_of(""), None);
    }
}
