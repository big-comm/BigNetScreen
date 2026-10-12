//! Where the application keeps its files, and atomic private writes there.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// An XDG base directory.
#[derive(Clone, Copy, Debug)]
pub enum BaseDir {
    Config,
    Data,
    State,
}

/// This application's directory under `base`: `$XDG_*_HOME/bignetscreen`,
/// or the default under `$HOME`. A relative `$XDG_*_HOME` is ignored, as the
/// XDG specification requires.
pub fn app_dir(base: BaseDir) -> Option<PathBuf> {
    let (variable, fallback) = match base {
        BaseDir::Config => ("XDG_CONFIG_HOME", ".config"),
        BaseDir::Data => ("XDG_DATA_HOME", ".local/share"),
        BaseDir::State => ("XDG_STATE_HOME", ".local/state"),
    };
    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(fallback)))
        .map(|dir| dir.join("bignetscreen"))
}

pub fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    let mut dir = std::fs::DirBuilder::new();
    dir.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        dir.mode(0o700);
    }
    dir.create(parent)?;
    let temporary = parent.join(format!(
        ".bignetscreen-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        std::fs::File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replacement_is_private_and_complete() {
        let dir = std::env::temp_dir().join(format!("nd-private-write-{}", std::process::id()));
        let path = dir.join("token");
        write_private(&path, b"old").unwrap();
        write_private(&path, b"new secret").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
