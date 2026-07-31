use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Eq, PartialEq)]
pub enum ReplaceOutcome {
    Unchanged,
    Replaced,
    Conflict,
}

pub fn atomic_replace_text(
    path: &Path,
    expected: Option<&str>,
    updated: &str,
    mode: u32,
) -> Result<ReplaceOutcome> {
    if expected == Some(updated) {
        return Ok(ReplaceOutcome::Unchanged);
    }

    let parent = path
        .parent()
        .with_context(|| format!("path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;

    let (temp_path, mut temp) = create_temp(path)?;
    let result = (|| {
        temp.write_all(updated.as_bytes())?;
        temp.sync_all()?;
        temp.set_permissions(Permissions::from_mode(mode & 0o777))?;

        if let Some(original) = expected
            && fs::read_to_string(path).ok().as_deref() != Some(original)
        {
            return Ok(ReplaceOutcome::Conflict);
        }

        fs::rename(&temp_path, path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        Ok(ReplaceOutcome::Replaced)
    })();

    if temp_path.exists() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn create_temp(path: &Path) -> Result<(PathBuf, File)> {
    let parent = path.parent().expect("validated by caller");
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    for _ in 0..16 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), sequence));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("failed to create temporary file"),
        }
    }
    anyhow::bail!("failed to allocate a temporary file for {}", path.display())
}
