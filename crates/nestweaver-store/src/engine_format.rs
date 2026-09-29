//! Storage-engine format marker.
//!
//! LadybugDB 0.21 changed how text keys are hashed (bytes at or above 0x80 on
//! signed-char platforms such as macOS and x86-64 Linux) and did NOT bump its
//! storage version: both 0.20 and 0.21 report storage version 47, so neither
//! engine refuses the other's file. An older file opened by the newer engine
//! silently misses primary-key lookups on non-ASCII keys. NestWeaver therefore
//! records the engine format itself, twice:
//!
//! * a `Meta` row, [`ENGINE_FORMAT_META_KEY`] = [`ENGINE_FORMAT_VALUE`],
//!   written by every writable open, which travels inside the database file;
//! * a durable sidecar, `<db>.engine-format`, written the moment a fresh file
//!   is created, which can be checked WITHOUT opening the engine.
//!
//! The check has to happen before `lbug::Database::new`, because a writable
//! 0.21 open replays the write-ahead log and checkpoints it into the file,
//! rewriting pages with the new hash. See [`check`].

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// `Meta` key recording which string-hash format built this database.
pub const ENGINE_FORMAT_META_KEY: &str = "storage.string_hash";
/// Value for the LadybugDB 0.21 string hash (bytes are zero-extended, not
/// sign-extended, before hashing).
pub const ENGINE_FORMAT_VALUE: &str = "zero-extend-v1";
/// Suffix of the sidecar beside the database file.
pub const ENGINE_FORMAT_SIDECAR_SUFFIX: &str = ".engine-format";

/// `<db>.engine-format`.
pub fn sidecar_path(db_path: &Path) -> PathBuf {
    let mut name = db_path.as_os_str().to_owned();
    name.push(ENGINE_FORMAT_SIDECAR_SUFFIX);
    PathBuf::from(name)
}

/// Write the sidecar durably (temp file, fsync, rename, fsync the directory).
pub fn write_sidecar(db_path: &Path) -> std::io::Result<()> {
    crate::durable_sidecar::atomic_replace_file(&sidecar_path(db_path), |file| {
        file.write_all(ENGINE_FORMAT_VALUE.as_bytes())?;
        file.write_all(b"\n")
    })
}

/// What the pre-open check concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Precheck {
    /// No database file, or a zero-byte one: whatever the engine writes now is
    /// in the current format, so the caller stamps the sidecar after a
    /// successful writable open.
    Fresh,
    /// The sidecar names the current format.
    Current,
    /// Not a regular file (a directory, say). The engine reports that itself.
    NotAFile,
    /// No sidecar and no crash debris: the caller must probe the `Meta` row
    /// with a read-only open before any writable one.
    NeedsProbe,
}

/// Crash debris beside a database: any of these means the engine would replay
/// a log on open, so an unverified file must not be opened at all.
fn debris_paths(db_path: &Path) -> [PathBuf; 3] {
    let with = |suffix: &str| {
        let mut name = db_path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    [with(".wal"), with(".wal.checkpoint"), with(".shadow")]
}

/// Decide, without opening the engine, whether `db_path` may be opened.
///
/// * missing or zero-byte file: [`Precheck::Fresh`];
/// * sidecar present and current: [`Precheck::Current`];
/// * sidecar present with any other content: refused;
/// * sidecar absent and a `.wal`, `.wal.checkpoint` or `.shadow` present (or
///   their existence cannot be determined): refused, because even a read-only
///   open would replay a log whose format is unknown;
/// * sidecar absent, no debris: [`Precheck::NeedsProbe`].
pub(crate) fn check(db_path: &Path) -> Result<Precheck, crate::StoreError> {
    let metadata = match std::fs::metadata(db_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Precheck::Fresh),
        // Let the engine report an unreadable path in its own words.
        Err(_) => return Ok(Precheck::NotAFile),
    };
    if !metadata.is_file() {
        return Ok(Precheck::NotAFile);
    }
    if metadata.len() == 0 {
        return Ok(Precheck::Fresh);
    }
    let sidecar = sidecar_path(db_path);
    match std::fs::read_to_string(&sidecar) {
        Ok(content) if content.trim() == ENGINE_FORMAT_VALUE => return Ok(Precheck::Current),
        Ok(content) => {
            return Err(rebuild_required(
                db_path,
                format!(
                    "carries an engine-format marker this build does not recognise ({:?} in {})",
                    content.trim(),
                    sidecar.display()
                ),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(crate::StoreError::Database(format!(
                "cannot read the engine-format marker {}: {error}",
                sidecar.display()
            )));
        }
    }
    for debris in debris_paths(db_path) {
        // `try_exists` distinguishes "absent" from "could not tell"; an
        // undecidable answer is treated as present, which refuses.
        if !matches!(debris.try_exists(), Ok(false)) {
            return Err(rebuild_required(
                db_path,
                format!(
                    "has no engine-format marker and has crash debris beside it ({}), so its \
                     format cannot be verified without replaying a log this engine may not \
                     be able to read",
                    debris.display()
                ),
            ));
        }
    }
    Ok(Precheck::NeedsProbe)
}

/// The refusal for a database built by an older engine.
pub(crate) fn rebuild_required(db_path: &Path, reason: impl Into<String>) -> crate::StoreError {
    crate::StoreError::RebuildRequired {
        path: Box::new(db_path.to_path_buf()),
        reason: reason.into().into_boxed_str(),
    }
}

/// The reason given when the probe found no marker.
pub(crate) const NO_MARKER_REASON: &str =
    "was built by a storage engine older than LadybugDB 0.21 (it has no engine-format marker)";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_and_empty_files_are_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.lbug");
        assert_eq!(check(&db).unwrap(), Precheck::Fresh);
        std::fs::write(&db, b"").unwrap();
        assert_eq!(check(&db).unwrap(), Precheck::Fresh);
    }

    #[test]
    fn a_current_sidecar_admits_and_a_foreign_one_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.lbug");
        std::fs::write(&db, b"not empty").unwrap();
        write_sidecar(&db).unwrap();
        assert_eq!(check(&db).unwrap(), Precheck::Current);
        std::fs::write(sidecar_path(&db), b"sign-extend-v0\n").unwrap();
        assert!(check(&db).unwrap_err().is_rebuild_required());
    }

    #[test]
    fn debris_without_a_sidecar_refuses_and_its_absence_asks_for_a_probe() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.lbug");
        std::fs::write(&db, b"not empty").unwrap();
        assert_eq!(check(&db).unwrap(), Precheck::NeedsProbe);
        for suffix in [".wal", ".wal.checkpoint", ".shadow"] {
            let debris = dir.path().join(format!("g.lbug{suffix}"));
            std::fs::write(&debris, b"x").unwrap();
            let error = check(&db).unwrap_err();
            assert!(error.is_rebuild_required(), "{suffix}: {error}");
            assert!(error.to_string().contains(suffix), "{error}");
            std::fs::remove_file(&debris).unwrap();
        }
    }
}
