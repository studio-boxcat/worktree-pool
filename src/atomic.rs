//! Atomic file write via tempfile in the same parent dir + rename.
//! `NamedTempFile::new_in` keeps the temp on the same filesystem so `persist`
//! is a true rename rather than copy+unlink (which loses atomicity).
use anyhow::{Context, Result};
use std::path::Path;

/// Write `content` to `path` atomically. Creates parent dirs if missing.
/// Clobbers existing target.
pub fn write(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().with_context(|| {
        format!(
            "atomic write target has no parent: {}",
            path.display()
        )
    })?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("mkdir -p {}", parent.display()))?;

    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating tempfile in {}", parent.display()))?;
    use std::io::Write;
    tmp.write_all(content.as_bytes())
        .with_context(|| format!("writing tempfile for {}", path.display()))?;
    tmp.as_file()
        .sync_data()
        .with_context(|| format!("fsync tempfile for {}", path.display()))?;
    tmp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("renaming tempfile to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nested/deep/file.txt");
        write(&target, "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
    }

    #[test]
    fn write_clobbers_existing() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("file.txt");
        std::fs::write(&target, "old").unwrap();
        write(&target, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    }
}
