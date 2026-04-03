use std::{fs, path::Path};

use anyhow::{Context, Result, anyhow};
use tempfile::NamedTempFile;

pub fn atomic_write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> Result<()> {
    let path = path.as_ref();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));

    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create parent directory for {}", path.display()))?;

    let temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temp file for {}", path.display()))?;
    fs::write(temp.path(), contents.as_ref())
        .with_context(|| format!("failed to stage {}", path.display()))?;
    temp.persist(path)
        .map_err(|error| anyhow!(error.error))
        .with_context(|| format!("failed to persist {}", path.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::atomic_write;

    #[test]
    fn creates_missing_parent_directories() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nested/config.toml");

        atomic_write(&path, "value = 1\n").unwrap();

        assert_eq!(fs::read_to_string(path).unwrap(), "value = 1\n");
    }

    #[test]
    fn overwrites_existing_files() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("prompt.md");
        fs::write(&path, "old\n").unwrap();

        atomic_write(&path, "new\n").unwrap();

        assert_eq!(fs::read_to_string(path).unwrap(), "new\n");
    }
}
