//! TLS certificate generation for NestWeaver server mode.
//!
//! Generates a self-signed CA certificate, a server certificate signed by
//! that CA, and optionally a client certificate for mTLS. Uses the `rcgen`
//! crate to produce PEM files compatible with `--tls-cert` / `--tls-key`.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use time::{Duration, OffsetDateTime};

/// Bundle of generated TLS certificates and keys in PEM format.
pub struct TlsBundle {
    pub ca_cert_pem: String,
    pub ca_key_pem: String,
    pub server_cert_pem: String,
    pub server_key_pem: String,
    pub client_cert_pem: Option<String>,
    pub client_key_pem: Option<String>,
}

/// Maximum accepted certificate validity. 100 years keeps the `time` crate
/// date math far from its representable range (huge day counts overflowed the
/// not-after computation and panicked). The CLI flag enforces the same
/// 1..=36500 range.
pub const MAX_VALIDITY_DAYS: u32 = 36500;

/// Generate a complete TLS bundle: CA, server cert, and optionally a client
/// cert for mTLS.
///
/// `server_names` are Subject Alternative Names — hostnames and/or IP
/// addresses. Defaults to `["localhost", "127.0.0.1"]` if empty.
///
/// `validity_days` must be in `1..=MAX_VALIDITY_DAYS`; anything else is an
/// error (never a panic).
pub fn generate_tls_bundle(
    server_names: &[String],
    validity_days: u32,
    generate_client: bool,
) -> Result<TlsBundle> {
    if validity_days == 0 || validity_days > MAX_VALIDITY_DAYS {
        anyhow::bail!("validity_days must be in 1..={MAX_VALIDITY_DAYS}, got {validity_days}");
    }
    let names: Vec<String> = if server_names.is_empty() {
        vec!["localhost".into(), "127.0.0.1".into()]
    } else {
        server_names.to_vec()
    };

    // ── CA certificate ───────────────────────────────────────────────

    let ca_key = KeyPair::generate().context("generate CA key pair")?;

    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "NestWeaver CA");
    ca_params
        .distinguished_name
        .push(DnType::OrganizationName, "NestWeaver");
    ca_params.not_before = OffsetDateTime::now_utc() - Duration::days(1);
    ca_params.not_after = OffsetDateTime::now_utc() + Duration::days(i64::from(validity_days) * 3);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    // Self-issued AKI (= own SKI): strict verifiers (e.g. Python 3.13+
    // VERIFY_X509_STRICT) want the extension present even on roots.
    ca_params.use_authority_key_identifier_extension = true;

    let ca = CertifiedIssuer::self_signed(ca_params, ca_key).context("self-sign CA certificate")?;

    // ── Server certificate ───────────────────────────────────────────

    let server_key = KeyPair::generate().context("generate server key pair")?;

    let mut server_sans: Vec<SanType> = Vec::new();
    for name in &names {
        if let Ok(ip) = name.parse::<IpAddr>() {
            server_sans.push(SanType::IpAddress(ip));
        } else {
            server_sans.push(SanType::DnsName(
                name.clone()
                    .try_into()
                    .context(format!("invalid DNS name: {name}"))?,
            ));
        }
    }

    let mut server_params = CertificateParams::default();
    server_params.subject_alt_names = server_sans;
    server_params
        .distinguished_name
        .push(DnType::CommonName, "NestWeaver Server");
    server_params
        .distinguished_name
        .push(DnType::OrganizationName, "NestWeaver");
    server_params.not_before = OffsetDateTime::now_utc() - Duration::days(1);
    server_params.not_after = OffsetDateTime::now_utc() + Duration::days(i64::from(validity_days));
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    // Extensions required by strict verifiers (Python 3.13+ VERIFY_X509_STRICT
    // rejects the handshake with "Missing Authority Key Identifier"):
    // - AKI naming the issuing CA,
    // - explicit basicConstraints CA:FALSE (critical) + SKI via ExplicitNoCa,
    // - key usage appropriate for a TLS server key.
    server_params.use_authority_key_identifier_extension = true;
    server_params.is_ca = IsCa::ExplicitNoCa;
    server_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];

    let server_cert = server_params
        .signed_by(&server_key, &*ca)
        .context("sign server certificate with CA")?;

    // ── Client certificate (optional) ────────────────────────────────

    let (client_cert_pem, client_key_pem) = if generate_client {
        let client_key = KeyPair::generate().context("generate client key pair")?;

        let mut client_params = CertificateParams::default();
        client_params
            .distinguished_name
            .push(DnType::CommonName, "NestWeaver Client");
        client_params
            .distinguished_name
            .push(DnType::OrganizationName, "NestWeaver");
        client_params.not_before = OffsetDateTime::now_utc() - Duration::days(1);
        client_params.not_after =
            OffsetDateTime::now_utc() + Duration::days(i64::from(validity_days));
        client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        // Same strict-verifier extensions as the server cert: AKI naming the
        // issuing CA, explicit basicConstraints CA:FALSE (critical) + SKI via
        // ExplicitNoCa, and a key usage appropriate for a TLS client key.
        client_params.use_authority_key_identifier_extension = true;
        client_params.is_ca = IsCa::ExplicitNoCa;
        client_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];

        let client_cert = client_params
            .signed_by(&client_key, &*ca)
            .context("sign client certificate with CA")?;

        (Some(client_cert.pem()), Some(client_key.serialize_pem()))
    } else {
        (None, None)
    };

    let ca_cert_ref: &rcgen::Certificate = ca.as_ref();
    Ok(TlsBundle {
        ca_cert_pem: ca_cert_ref.pem(),
        ca_key_pem: ca.key().serialize_pem(),
        server_cert_pem: server_cert.pem(),
        server_key_pem: server_key.serialize_pem(),
        client_cert_pem,
        client_key_pem,
    })
}

// ── Installing a bundle into a directory ─────────────────────────────────
//
// `server init-tls` is a KEY-LIFECYCLE operation, not idempotent
// initialization. Re-running it mints a NEW CA and destroys the old CA's
// private key, so every certificate the old CA signed stops verifying. The
// machinery below exists because the per-file `fs::write` loop this replaced
// left five separate holes, all measured on 8.0.0:
//
//  1. PARTIAL DIRECTORIES. The caller's guard tested `ca.pem` alone, so a
//     directory holding `client.pem` + `client-key.pem` and no `ca.pem`
//     (an interrupted run, or a hand-deleted root) was silently overwritten
//     with NO warning at all.
//  2. INTERRUPTED WRITES. Six independent `fs::write` calls have five
//     failure points between them. An `EACCES` on `ca-key.pem` (an operator
//     who chmod'ed their key to 0400) left the NEW `ca.pem` beside the OLD
//     `ca-key.pem` — a certificate and a private key from different key
//     pairs — and exited 1 with the old CA already gone.
//  3. CONCURRENCY. Two simultaneous invocations interleaved their writes and
//     both exited 0 over a directory whose `ca.pem` and `ca-key.pem` came
//     from different processes.
//  4. SYMLINKS. `Path::exists()` follows links, so a DANGLING `ca.pem`
//     symlink read as "no CA present", and `fs::write` then followed it and
//     wrote the CA private key to whatever path the link named — outside
//     `--output-dir` entirely.
//  5. MODES. `fs::write` creates with `0666 & ~umask` and the `chmod` to
//     0600 lands afterwards, so a freshly created private key is readable by
//     the whole machine for the width of that window.
//
// The design: one exclusive `flock` per output directory, a COMPLETE bundle
// staged with final modes and fsynced before anything existing is touched, a
// journal that makes an interrupted install recoverable, and installation by
// `rename` only — which replaces a symlink instead of following it.

/// The files `server init-tls` owns inside an output directory, paired with
/// the mode each must carry, in INSTALL order: the trust root first, then the
/// leaves it signs.
///
/// The order is load-bearing, not cosmetic. Installing root-first (and
/// retiring leaf-first, its reverse) means that at NO instant during a
/// replacement does the directory hold a leaf certificate signed by a CA
/// other than the `ca.pem` sitting beside it. The window contains only
/// ABSENCE, which fails closed, never a silently wrong trust bundle.
///
/// This list is also the definition of "an existing bundle": ANY of these
/// names present means the directory is already a bundle. Testing `ca.pem`
/// alone is exactly what let a partial directory be overwritten in silence.
pub const MANAGED_FILES: &[(&str, u32)] = &[
    ("ca.pem", 0o644),
    ("ca-key.pem", 0o600),
    ("server.pem", 0o644),
    ("server-key.pem", 0o600),
    ("client.pem", 0o644),
    ("client-key.pem", 0o600),
];

/// Held for the lifetime of one `init-tls` run; serialises replacements.
const LOCK_FILE: &str = ".nestweaver-tls.lock";
/// Where a complete new bundle is built before anything existing is touched.
const STAGING_DIR: &str = ".nestweaver-tls.staging";
/// Where the bundle being replaced is moved during an install.
const RETIRED_DIR: &str = ".nestweaver-tls.retired";
/// Present only while an install is between "old bundle moved aside" and
/// "new bundle fully in place". Its existence is what makes an interrupted
/// install recoverable rather than merely broken.
const JOURNAL_FILE: &str = ".nestweaver-tls.journal";
/// The previous bundle, retained after a successful replacement so the
/// destroyed CA private key is recoverable. Dot-prefixed so it stays out of
/// `*.pem` globs and out of `ls`.
pub const BACKUP_DIR: &str = ".nestweaver-tls.backup";

/// Which managed files a directory currently holds.
#[derive(Debug, Default, Clone)]
pub struct DirState {
    /// Managed names present, in [`MANAGED_FILES`] order. A dangling symlink
    /// counts as present — it is a name this command owns and would clobber.
    pub present: Vec<&'static str>,
    /// The subset of `present` that are symlinks rather than regular files.
    pub symlinked: Vec<&'static str>,
}

/// What an install actually did.
#[derive(Debug)]
pub struct InstallReport {
    /// Managed names the new bundle put in place.
    pub installed: Vec<&'static str>,
    /// Managed names that existed, are not part of the new bundle, and were
    /// therefore retired — they were signed by the CA this run destroyed and
    /// cannot survive it.
    pub removed: Vec<&'static str>,
    /// Where the replaced bundle was kept, when there was one.
    pub backup_dir: Option<PathBuf>,
}

/// An exclusively locked TLS output directory.
///
/// [`TlsDir::open`] takes the lock and rolls back any interrupted install
/// BEFORE the caller inspects the directory, so what [`TlsDir::state`]
/// reports is a settled bundle rather than a half-replaced one. The lock is
/// released when this value is dropped.
pub struct TlsDir {
    dir: PathBuf,
    /// Held open purely to hold the `flock`; closing it releases the lock.
    _lock: File,
    recovered: Vec<&'static str>,
}

impl TlsDir {
    /// Create `dir` if needed, take the exclusive install lock, and recover
    /// from any interrupted previous install.
    ///
    /// Fails rather than waits when another process holds the lock: an
    /// `init-tls` that blocked would sit silently behind a peer that is
    /// mid-replacement, and the honest answer to "two processes are rotating
    /// the same CA" is to stop, not to queue.
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("create output dir: {}", dir.display()))?;

        let lock_path = dir.join(LOCK_FILE);
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let lock = opts
            .open(&lock_path)
            .with_context(|| format!("open install lock: {}", lock_path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                    anyhow::bail!(
                        "another `nestweaver server init-tls` is already installing into {} \
                         (it holds {}). Wait for it to finish, then re-run — two concurrent \
                         runs would leave a CA certificate and a private key from different \
                         key pairs.",
                        dir.display(),
                        lock_path.display()
                    );
                }
                return Err(anyhow::Error::new(err)
                    .context(format!("lock {} for install", lock_path.display())));
            }
        }

        let mut this = Self {
            dir: dir.to_path_buf(),
            _lock: lock,
            recovered: Vec::new(),
        };
        this.recover_interrupted_install()?;
        Ok(this)
    }

    /// Managed names restored by rolling back an interrupted install, empty
    /// when the directory was already settled. The CLI discloses this: a
    /// recovery that happened in silence would be indistinguishable from
    /// nothing having gone wrong.
    pub fn recovered(&self) -> &[&'static str] {
        &self.recovered
    }

    /// Which managed files the directory holds right now.
    ///
    /// Uses `symlink_metadata`, so a DANGLING symlink counts as present.
    /// `Path::exists()` follows links and reported such a name as absent,
    /// which is how a symlinked `ca.pem` slipped past the old guard and got
    /// written THROUGH.
    pub fn state(&self) -> Result<DirState> {
        let mut state = DirState::default();
        for (name, _) in MANAGED_FILES {
            let path = self.dir.join(name);
            match fs::symlink_metadata(&path) {
                Ok(meta) => {
                    state.present.push(name);
                    if meta.file_type().is_symlink() {
                        state.symlinked.push(name);
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(anyhow::Error::new(err).context(format!("stat {}", path.display())));
                }
            }
        }
        Ok(state)
    }

    /// Stage `bundle` completely, then install it as a whole.
    ///
    /// Managed files that exist and are NOT part of `bundle` are retired, not
    /// left behind: they were signed by the CA this call destroys, and a
    /// client certificate sitting beside a trust root that cannot vouch for
    /// it is the exact split bundle this command used to produce.
    ///
    /// Nothing existing is touched until the whole new bundle is on disk with
    /// its final modes and fsynced. From that point on every step is a
    /// `rename`, which is atomic per file and replaces a symlink rather than
    /// following it.
    pub fn install(&self, bundle: &TlsBundle) -> Result<InstallReport> {
        let members = bundle_members(bundle);
        let installing: Vec<&'static str> = members.iter().map(|(n, _, _)| *n).collect();
        let retiring = self.state()?.present;
        let removed: Vec<&'static str> = retiring
            .iter()
            .filter(|n| !installing.contains(n))
            .copied()
            .collect();

        let staging = self.dir.join(STAGING_DIR);
        let retired = self.dir.join(RETIRED_DIR);
        let journal = self.dir.join(JOURNAL_FILE);

        // ── Stage a COMPLETE bundle ──────────────────────────────────────
        remove_any(&staging)?;
        create_private_dir(&staging)?;
        for (name, pem, mode) in &members {
            write_private_file(&staging.join(name), pem.as_bytes(), *mode)?;
        }
        sync_dir(&staging)?;

        remove_any(&retired)?;
        create_private_dir(&retired)?;
        sync_dir(&retired)?;

        // ── Declare intent, durably, before moving anything ──────────────
        let record = serde_json::json!({
            "retiring": retiring,
            "installing": installing,
        });
        write_private_file(&journal, record.to_string().as_bytes(), 0o600)?;
        sync_dir(&self.dir)?;

        // ── Retire leaf-first, install root-first ────────────────────────
        // `retiring` is in MANAGED_FILES (install) order, so reversing it
        // removes the leaves before the CA that signed them.
        for name in retiring.iter().rev() {
            let from = self.dir.join(name);
            let to = retired.join(name);
            fs::rename(&from, &to)
                .with_context(|| format!("retire {} to {}", from.display(), to.display()))?;
        }
        for name in &installing {
            let from = staging.join(name);
            let to = self.dir.join(name);
            fs::rename(&from, &to).with_context(|| format!("install {}", to.display()))?;
        }
        sync_dir(&self.dir)?;

        // ── Commit: removing the journal is the point of no return ───────
        fs::remove_file(&journal)
            .with_context(|| format!("clear install journal {}", journal.display()))?;
        sync_dir(&self.dir)?;

        // ── Retain the replaced bundle ───────────────────────────────────
        let backup_dir = self.promote_retired_to_backup()?;
        remove_any(&staging)?;
        sync_dir(&self.dir)?;

        Ok(InstallReport {
            installed: installing,
            removed,
            backup_dir,
        })
    }

    /// Move `.nestweaver-tls.retired` to `.nestweaver-tls.backup`, replacing
    /// any previous backup. Returns `None` when nothing was replaced.
    ///
    /// Exactly one generation is kept. Keeping none makes `--force` an
    /// irreversible destruction of a private key; keeping all of them
    /// accumulates unbounded copies of retired key material.
    fn promote_retired_to_backup(&self) -> Result<Option<PathBuf>> {
        let retired = self.dir.join(RETIRED_DIR);
        let backup = self.dir.join(BACKUP_DIR);
        let empty = fs::read_dir(&retired)
            .with_context(|| format!("read {}", retired.display()))?
            .next()
            .is_none();
        if empty {
            remove_any(&retired)?;
            return Ok(None);
        }
        remove_any(&backup)?;
        fs::rename(&retired, &backup)
            .with_context(|| format!("retain replaced bundle at {}", backup.display()))?;
        Ok(Some(backup))
    }

    /// Roll an interrupted install BACK to the bundle that preceded it.
    ///
    /// Back rather than forward: the staged bundle belongs to a run whose
    /// SANs and validity this invocation knows nothing about, while the
    /// retired bundle is the last state the operator actually had. Restoring
    /// it is also what makes the refusal below able to describe reality.
    ///
    /// Correct from every crash point. The journal is fsynced before the
    /// first rename, so a journal that exists and parses means the retire
    /// phase had begun; one that exists and does NOT parse means the crash
    /// landed inside the journal write itself, before anything moved.
    fn recover_interrupted_install(&mut self) -> Result<()> {
        let journal = self.dir.join(JOURNAL_FILE);
        let staging = self.dir.join(STAGING_DIR);
        let retired = self.dir.join(RETIRED_DIR);

        let journal_present = fs::symlink_metadata(&journal).is_ok();
        if !journal_present {
            // No install was in flight. Anything left here is debris from a
            // crash before the journal, or from after the commit point — in
            // the latter case `retired` still holds the previous bundle and
            // is worth keeping as the backup.
            if fs::symlink_metadata(&retired).is_ok() {
                self.promote_retired_to_backup()?;
            }
            remove_any(&staging)?;
            return Ok(());
        }

        // An unparseable journal means the crash landed inside the journal
        // write itself, which is fsynced BEFORE the first rename — so nothing
        // had moved and both lists are correctly empty.
        let record: serde_json::Value = fs::read(&journal)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(serde_json::Value::Null);
        let names_at = |key: &str| -> Vec<String> {
            record
                .get(key)
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        let retiring = names_at("retiring");
        let installing = names_at("installing");

        // A name the crashed run was installing that it never had a
        // predecessor for can only be a half-installed new file, so it goes.
        // A name that WAS being retired but is not in `retired/` is the
        // opposite case: the retire phase had not reached it, so the file
        // sitting there is the old one and must be left exactly alone. The
        // install phase cannot have run — it starts only once retiring is
        // complete.
        for (name, _) in MANAGED_FILES {
            let has_predecessor = retiring.iter().any(|n| n == name);
            if installing.iter().any(|n| n == name) && !has_predecessor {
                remove_any(&self.dir.join(name))?;
            }
        }
        // Everything the crashed run moved aside goes back where it was.
        for (name, _) in MANAGED_FILES {
            let saved = retired.join(name);
            if fs::symlink_metadata(&saved).is_ok() {
                let target = self.dir.join(name);
                remove_any(&target)?;
                fs::rename(&saved, &target)
                    .with_context(|| format!("restore {}", target.display()))?;
                self.recovered.push(name);
            }
        }

        fs::remove_file(&journal)
            .with_context(|| format!("clear install journal {}", journal.display()))?;
        remove_any(&retired)?;
        remove_any(&staging)?;
        sync_dir(&self.dir)?;
        Ok(())
    }
}

/// `(filename, PEM, mode)` for every file this bundle provides.
fn bundle_members(bundle: &TlsBundle) -> Vec<(&'static str, &str, u32)> {
    let mut members: Vec<(&'static str, &str, u32)> = vec![
        ("ca.pem", bundle.ca_cert_pem.as_str(), 0o644),
        ("ca-key.pem", bundle.ca_key_pem.as_str(), 0o600),
        ("server.pem", bundle.server_cert_pem.as_str(), 0o644),
        ("server-key.pem", bundle.server_key_pem.as_str(), 0o600),
    ];
    if let (Some(cert), Some(key)) = (&bundle.client_cert_pem, &bundle.client_key_pem) {
        members.push(("client.pem", cert.as_str(), 0o644));
        members.push(("client-key.pem", key.as_str(), 0o600));
    }
    members
}

/// Create `path` with `mode` set AT CREATION, write, and fsync.
///
/// `create_new` plus `OpenOptionsExt::mode` is the whole point: `fs::write`
/// followed by `chmod` publishes a private key at `0666 & ~umask` for the
/// width of the window between them. Mirrors `AtomicDirCache::write_atomic`
/// in `nestweaver-daemon/src/acme.rs`, which fixed the same hazard for the
/// ACME key cache.
fn write_private_file(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode);
    }
    let mut file = opts
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("fsync {}", path.display()))?;
    drop(file);
    // Belt and braces against an inherited umask on platforms where the
    // creation mode is advisory.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .with_context(|| format!("set permissions on {}", path.display()))?;
    }
    let _ = mode;
    Ok(())
}

/// Create a directory only this user can traverse. Staging, retired and
/// backup directories all hold private keys.
fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir(path).with_context(|| format!("create {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("set permissions on {}", path.display()))?;
    }
    Ok(())
}

/// Remove `path` whatever it is — file, directory, or symlink — without ever
/// following a link.
fn remove_any(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(anyhow::Error::new(err).context(format!("stat {}", path.display()))),
        Ok(meta) => {
            let result = if meta.file_type().is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            };
            result.with_context(|| format!("remove {}", path.display()))
        }
    }
}

/// fsync a directory so the renames inside it are durable before the next
/// phase begins. Without this the journal could outlive the renames it
/// describes, or vice versa, and recovery would be reasoning about a state
/// the disk never held.
fn sync_dir(path: &Path) -> Result<()> {
    let dir = File::open(path).with_context(|| format!("open {}", path.display()))?;
    // Directory fsync is not supported on every filesystem; a rejection here
    // is not a reason to fail a bundle that is otherwise correctly written.
    let _ = dir.sync_all();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_default_bundle() {
        let bundle = generate_tls_bundle(&[], 365, false).unwrap();
        assert!(bundle.ca_cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(bundle.ca_key_pem.contains("BEGIN PRIVATE KEY"));
        assert!(bundle.server_cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(bundle.server_key_pem.contains("BEGIN PRIVATE KEY"));
        assert!(bundle.client_cert_pem.is_none());
        assert!(bundle.client_key_pem.is_none());
    }

    #[test]
    fn generate_with_client_cert() {
        let bundle = generate_tls_bundle(&[], 365, true).unwrap();
        assert!(bundle.client_cert_pem.is_some());
        assert!(bundle.client_key_pem.is_some());
        assert!(
            bundle
                .client_cert_pem
                .unwrap()
                .contains("BEGIN CERTIFICATE")
        );
    }

    #[test]
    fn generate_with_custom_sans() {
        let names = vec![
            "localhost".into(),
            "127.0.0.1".into(),
            "nestweaver.internal".into(),
            "10.0.1.50".into(),
        ];
        let bundle = generate_tls_bundle(&names, 30, false).unwrap();
        assert!(bundle.server_cert_pem.contains("BEGIN CERTIFICATE"));
    }

    /// Strict verifiers (Python 3.13+ `VERIFY_X509_STRICT`) reject handshakes
    /// whose certificates lack an Authority Key Identifier. Parse the
    /// generated certs and assert the extensions they require are present.
    #[test]
    fn certs_carry_extensions_required_by_strict_verifiers() {
        use x509_parser::extensions::ParsedExtension;
        use x509_parser::prelude::*;

        fn key_identifier(cert: &X509Certificate<'_>, authority: bool) -> Option<Vec<u8>> {
            cert.extensions()
                .iter()
                .find_map(|ext| match (ext.parsed_extension(), authority) {
                    (ParsedExtension::AuthorityKeyIdentifier(aki), true) => {
                        aki.key_identifier.as_ref().map(|ki| ki.0.to_vec())
                    }
                    (ParsedExtension::SubjectKeyIdentifier(ki), false) => Some(ki.0.to_vec()),
                    _ => None,
                })
        }

        let bundle = generate_tls_bundle(&[], 365, true).unwrap();
        let (_, ca_pem) =
            x509_parser::pem::parse_x509_pem(bundle.ca_cert_pem.as_bytes()).expect("parse CA PEM");
        let (_, ca) = X509Certificate::from_der(&ca_pem.contents).expect("parse CA DER");
        let (_, server_pem) = x509_parser::pem::parse_x509_pem(bundle.server_cert_pem.as_bytes())
            .expect("parse server PEM");
        let (_, server) =
            X509Certificate::from_der(&server_pem.contents).expect("parse server DER");

        // CA: self-issued AKI, SKI, critical CA:TRUE basicConstraints,
        // keyCertSign key usage.
        let ca_ski =
            key_identifier(&ca, false).expect("CA cert must carry a Subject Key Identifier");
        assert_eq!(
            key_identifier(&ca, true).as_deref(),
            Some(ca_ski.as_slice()),
            "CA cert must carry an Authority Key Identifier naming its own SKI"
        );
        let ca_bc = ca
            .basic_constraints()
            .unwrap()
            .expect("CA basicConstraints");
        assert!(ca_bc.critical && ca_bc.value.ca, "CA must be a critical CA");
        let ca_ku = ca.key_usage().unwrap().expect("CA key usage");
        assert!(ca_ku.value.key_cert_sign(), "CA must allow keyCertSign");

        // Server: AKI naming the CA's SKI, critical CA:FALSE basicConstraints,
        // SKI, digitalSignature key usage, serverAuth EKU.
        assert_eq!(
            key_identifier(&server, true).as_deref(),
            Some(ca_ski.as_slice()),
            "server cert must carry an Authority Key Identifier naming the issuing CA"
        );
        assert!(
            key_identifier(&server, false).is_some(),
            "server cert must carry a Subject Key Identifier"
        );
        let server_bc = server
            .basic_constraints()
            .unwrap()
            .expect("server basicConstraints");
        assert!(
            server_bc.critical && !server_bc.value.ca,
            "server cert must assert CA:FALSE"
        );
        let server_ku = server.key_usage().unwrap().expect("server key usage");
        assert!(
            server_ku.value.digital_signature(),
            "server key usage must include digitalSignature"
        );
        let server_eku = server
            .extended_key_usage()
            .unwrap()
            .expect("server extended key usage");
        assert!(
            server_eku.value.server_auth,
            "server EKU must include serverAuth"
        );

        // Client (mTLS): same strict-verifier extensions as the server cert —
        // AKI naming the CA's SKI, critical CA:FALSE basicConstraints, SKI,
        // digitalSignature key usage — plus clientAuth EKU.
        let client_pem_str = bundle.client_cert_pem.expect("client cert generated");
        let (_, client_pem) =
            x509_parser::pem::parse_x509_pem(client_pem_str.as_bytes()).expect("parse client PEM");
        let (_, client) =
            X509Certificate::from_der(&client_pem.contents).expect("parse client DER");
        assert_eq!(
            key_identifier(&client, true).as_deref(),
            Some(ca_ski.as_slice()),
            "client cert must carry an Authority Key Identifier naming the issuing CA"
        );
        assert!(
            key_identifier(&client, false).is_some(),
            "client cert must carry a Subject Key Identifier"
        );
        let client_bc = client
            .basic_constraints()
            .unwrap()
            .expect("client basicConstraints");
        assert!(
            client_bc.critical && !client_bc.value.ca,
            "client cert must assert CA:FALSE"
        );
        let client_ku = client.key_usage().unwrap().expect("client key usage");
        assert!(
            client_ku.value.digital_signature(),
            "client key usage must include digitalSignature"
        );
        let client_eku = client
            .extended_key_usage()
            .unwrap()
            .expect("client extended key usage");
        assert!(
            client_eku.value.client_auth,
            "client EKU must include clientAuth"
        );
    }

    #[test]
    fn rejects_invalid_validity_days() {
        // 0 and absurdly large day counts must be a clean error, not a
        // panic from overflowing the not-after date computation.
        let err = generate_tls_bundle(&[], 0, false)
            .err()
            .expect("0 must fail");
        assert!(err.to_string().contains("validity_days"), "{err}");
        let err = generate_tls_bundle(&[], 999_999_999, false)
            .err()
            .expect("huge day count must fail");
        assert!(err.to_string().contains("999999999"), "{err}");
        // The boundary itself stays valid.
        assert!(generate_tls_bundle(&[], MAX_VALIDITY_DAYS, false).is_ok());
    }

    // ── Install safety ───────────────────────────────────────────────────
    //
    // Every test below asserts a property the per-file `fs::write` loop this
    // replaced did NOT have, each one measured failing on 8.0.0.

    /// A bundle directory is COHERENT when `ca.pem` is the certificate of the
    /// key in `ca-key.pem`, and every leaf certificate present names that CA
    /// as its issuer. This is the property `openssl verify -CAfile ca.pem`
    /// checks, expressed without shelling out.
    fn assert_coherent(dir: &Path) {
        use x509_parser::extensions::ParsedExtension;
        use x509_parser::prelude::*;

        fn parse(path: &Path) -> Vec<u8> {
            let pem = fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let (_, p) = x509_parser::pem::parse_x509_pem(&pem)
                .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
            p.contents
        }
        fn key_id(der: &[u8], authority: bool) -> Option<Vec<u8>> {
            let (_, cert) = X509Certificate::from_der(der).unwrap();
            cert.extensions()
                .iter()
                .find_map(|ext| match (ext.parsed_extension(), authority) {
                    (ParsedExtension::AuthorityKeyIdentifier(aki), true) => {
                        aki.key_identifier.as_ref().map(|ki| ki.0.to_vec())
                    }
                    (ParsedExtension::SubjectKeyIdentifier(ki), false) => Some(ki.0.to_vec()),
                    _ => None,
                })
        }

        let ca_der = parse(&dir.join("ca.pem"));
        let (_, ca) = X509Certificate::from_der(&ca_der).unwrap();

        // The CA certificate and the CA private key must be one key pair.
        // Splitting these is exactly what an interrupted per-file write and
        // two concurrent runs both produced on 8.0.0.
        let ca_key_pem = fs::read_to_string(dir.join("ca-key.pem")).expect("ca-key.pem");
        let ca_key = rcgen::KeyPair::from_pem(&ca_key_pem).expect("parse ca-key.pem");
        assert_eq!(
            ca.public_key().raw,
            rcgen::PublicKeyData::subject_public_key_info(&ca_key).as_slice(),
            "ca.pem is not the certificate of the key in ca-key.pem — split bundle in {}",
            dir.display()
        );

        let ca_ski = key_id(&ca_der, false).expect("CA SKI");
        for leaf in ["server.pem", "client.pem"] {
            let path = dir.join(leaf);
            if !path.exists() {
                continue;
            }
            let der = parse(&path);
            assert_eq!(
                key_id(&der, true),
                Some(ca_ski.clone()),
                "{leaf} in {} is signed by a CA other than the ca.pem beside it",
                dir.display()
            );
        }
    }

    fn install(dir: &Path, client: bool) -> InstallReport {
        let bundle = generate_tls_bundle(&["localhost".into()], 365, client).unwrap();
        TlsDir::open(dir).unwrap().install(&bundle).unwrap()
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// The reported defect, at the layer that performs it: a second install
    /// over an existing bundle must not silently produce a directory whose
    /// client certificate no longer verifies.
    ///
    /// On 8.0.0 `write_tls_bundle` replaced `ca.pem` and `ca-key.pem`, left
    /// `client.pem` byte-identical, and returned `Ok(())`.
    #[test]
    fn replacing_a_ca_cannot_leave_certificates_it_signed_behind() {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path(), true);
        assert_coherent(dir.path());
        let old_ca_key = fs::read(dir.path().join("ca-key.pem")).unwrap();
        let old_client = fs::read(dir.path().join("client.pem")).unwrap();

        // The original sequence: re-run WITHOUT --client.
        let report = install(dir.path(), false);

        assert_ne!(
            fs::read(dir.path().join("ca-key.pem")).unwrap(),
            old_ca_key,
            "the CA was supposed to be replaced"
        );
        assert_eq!(
            report.removed,
            vec!["client.pem", "client-key.pem"],
            "certificates the destroyed CA signed must be retired with it"
        );
        assert!(
            !dir.path().join("client.pem").exists(),
            "client.pem signed by the destroyed CA was left beside the new trust root"
        );
        assert!(!dir.path().join("client-key.pem").exists());
        assert_coherent(dir.path());

        // The destroyed key material is recoverable, not gone.
        let backup = report.backup_dir.expect("previous bundle retained");
        assert_eq!(fs::read(backup.join("ca-key.pem")).unwrap(), old_ca_key);
        assert_eq!(fs::read(backup.join("client.pem")).unwrap(), old_client);
        assert_coherent(&backup);
    }

    /// "An existing bundle" is ANY managed name, not `ca.pem`. Testing
    /// `ca.pem` alone is why a partial directory was overwritten in silence.
    #[test]
    fn partial_directories_count_as_an_existing_bundle() {
        // CA present, client absent.
        let ca_only = tempfile::tempdir().unwrap();
        install(ca_only.path(), false);
        let state = TlsDir::open(ca_only.path()).unwrap().state().unwrap();
        assert_eq!(
            state.present,
            vec!["ca.pem", "ca-key.pem", "server.pem", "server-key.pem"]
        );

        // Client present, CA absent — the case that produced NO warning at
        // all on 8.0.0, because the only thing checked was the file that had
        // been removed.
        let no_ca = tempfile::tempdir().unwrap();
        install(no_ca.path(), true);
        fs::remove_file(no_ca.path().join("ca.pem")).unwrap();
        fs::remove_file(no_ca.path().join("ca-key.pem")).unwrap();
        let state = TlsDir::open(no_ca.path()).unwrap().state().unwrap();
        assert_eq!(
            state.present,
            vec![
                "server.pem",
                "server-key.pem",
                "client.pem",
                "client-key.pem"
            ],
            "a rootless bundle is still a bundle"
        );
        assert!(state.symlinked.is_empty());

        // A wholly empty directory is not a bundle.
        let empty = tempfile::tempdir().unwrap();
        assert!(
            TlsDir::open(empty.path())
                .unwrap()
                .state()
                .unwrap()
                .present
                .is_empty()
        );
    }

    /// A symlinked managed name must be seen (so it cannot be clobbered in
    /// silence) and must never be written THROUGH.
    ///
    /// On 8.0.0 a dangling `ca.pem` symlink read as absent — `Path::exists()`
    /// follows links — and `fs::write` then created the CA private key at
    /// whatever path the link named, outside `--output-dir` entirely.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_member_is_seen_and_never_written_through() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("tls");
        let elsewhere = root.path().join("elsewhere");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&elsewhere).unwrap();
        let hijack = elsewhere.join("hijacked-ca-key.pem");
        std::os::unix::fs::symlink(&hijack, dir.join("ca-key.pem")).unwrap();

        let state = TlsDir::open(&dir).unwrap().state().unwrap();
        assert_eq!(
            state.present,
            vec!["ca-key.pem"],
            "a dangling symlink is a present member"
        );
        assert_eq!(state.symlinked, vec!["ca-key.pem"]);

        let report = install(&dir, false);

        assert!(
            !hijack.exists(),
            "the CA private key was written through the symlink to {}",
            hijack.display()
        );
        assert!(
            !fs::symlink_metadata(dir.join("ca-key.pem"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "the installed key must be a regular file, not the link it replaced"
        );
        assert_coherent(&dir);
        // The link itself was retired, not followed.
        let backup = report.backup_dir.expect("the link was retired");
        assert!(
            fs::symlink_metadata(backup.join("ca-key.pem"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    /// No private key this command writes is ever readable by anyone else —
    /// installed, staged or RETAINED — whatever umask it runs under.
    ///
    /// Two properties, one hazard. `fs::write` creates with `0666 & ~umask`
    /// and the `chmod` to 0600 lands afterwards, so the key is published for
    /// the width of that window; `create_new` + `OpenOptionsExt::mode` closes
    /// it. And a replacement now RETAINS the CA private key it destroys, so
    /// there is a second copy of live key material on disk that has to be
    /// just as private as the first.
    #[cfg(unix)]
    #[test]
    fn no_key_this_command_writes_is_readable_by_anyone_else() {
        let dir = tempfile::tempdir().unwrap();
        // 0 means "grant everything the creation mode allows", so the mode
        // set at creation is the only thing standing between the key and the
        // rest of the machine.
        let previous = unsafe { libc::umask(0) };
        install(dir.path(), true);
        // A second pass rewrites over the first; nothing may be inherited.
        install(dir.path(), true);
        unsafe { libc::umask(previous) };

        for (name, mode) in MANAGED_FILES {
            assert_eq!(
                mode_of(&dir.path().join(name)),
                *mode,
                "{name} must be {mode:o}"
            );
        }

        // The bundle this run destroyed is kept, and it holds a CA private
        // key. It must be no more readable than the one that replaced it.
        let backup = dir.path().join(BACKUP_DIR);
        assert!(
            backup.is_dir(),
            "the replaced bundle must be retained at {}",
            backup.display()
        );
        assert_eq!(mode_of(&backup), 0o700, "the retained bundle directory");
        for (name, mode) in MANAGED_FILES {
            assert_eq!(mode_of(&backup.join(name)), *mode, "retained {name}");
        }
    }

    /// Two simultaneous installs must not interleave into a split bundle.
    ///
    /// On 8.0.0 two concurrent runs over one directory both exited 0 and left
    /// a `ca.pem` and `ca-key.pem` from different processes.
    #[test]
    fn concurrent_installs_are_serialised_not_interleaved() {
        let dir = tempfile::tempdir().unwrap();
        let held = TlsDir::open(dir.path()).unwrap();
        let err = match TlsDir::open(dir.path()) {
            Ok(_) => panic!("a second open of a locked directory must be refused"),
            Err(err) => err,
        };
        let text = format!("{err:#}");
        assert!(
            text.contains("already installing into"),
            "refusal must name the condition: {text}"
        );
        drop(held);
        // The lock is released with the handle.
        TlsDir::open(dir.path()).expect("lock released on drop");

        // Under real contention every winner leaves a coherent bundle and
        // every loser leaves nothing at all.
        let path = dir.path().to_path_buf();
        let done: Vec<bool> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let path = path.clone();
                    scope.spawn(move || {
                        let bundle = generate_tls_bundle(&[], 365, true).unwrap();
                        match TlsDir::open(&path) {
                            Ok(dir) => dir.install(&bundle).is_ok(),
                            Err(_) => false,
                        }
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(done.iter().any(|ok| *ok), "at least one install must win");
        assert_coherent(dir.path());
    }

    /// An install that cannot proceed must leave the previous bundle exactly
    /// as it was, not half-replaced.
    ///
    /// This is the 8.0.0 failure verbatim: an operator who hardened
    /// `ca-key.pem` to 0400 got an `EACCES` partway through the six writes,
    /// after the new `ca.pem` had already landed — leaving a certificate and
    /// a private key from different key pairs, and exit 1.
    #[cfg(unix)]
    #[test]
    fn a_failed_install_leaves_the_previous_bundle_untouched() {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path(), true);
        let before: Vec<(String, Vec<u8>)> = MANAGED_FILES
            .iter()
            .map(|(n, _)| ((*n).to_string(), fs::read(dir.path().join(n)).unwrap()))
            .collect();

        // Staging cannot be created, so nothing existing is ever touched.
        let staging_blocker = dir.path().join(STAGING_DIR);
        fs::create_dir(&staging_blocker).unwrap();
        fs::write(staging_blocker.join("occupied"), b"x").unwrap();
        let mut perms = fs::metadata(dir.path()).unwrap().permissions();
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o500);
        }
        fs::set_permissions(dir.path(), perms).unwrap();

        let bundle = generate_tls_bundle(&[], 365, true).unwrap();
        let outcome = TlsDir::open(dir.path()).and_then(|d| d.install(&bundle));

        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert!(
            outcome.is_err(),
            "a read-only output dir must fail the install"
        );
        for (name, contents) in &before {
            assert_eq!(
                &fs::read(dir.path().join(name)).unwrap(),
                contents,
                "{name} changed during a failed install"
            );
        }
        assert_coherent(dir.path());
    }

    /// An install interrupted between "old bundle moved aside" and "new
    /// bundle fully in place" is rolled back to the previous bundle by the
    /// next `TlsDir::open`, from EVERY point it can be interrupted at.
    ///
    /// The states are constructed from the module's own constants so they
    /// cannot drift from what `install` actually writes, and the recovery
    /// runs through the real `TlsDir::open` path.
    #[test]
    fn every_interrupted_install_recovers_to_a_coherent_bundle() {
        // `install` retires leaf-first and installs root-first, so the
        // reachable interruption points are: k files retired (0..=6), then
        // j files installed (0..=6).
        for retired_count in 0..=MANAGED_FILES.len() {
            for installed_count in 0..=MANAGED_FILES.len() {
                if installed_count > 0 && retired_count < MANAGED_FILES.len() {
                    // Installing never starts before retiring finishes.
                    continue;
                }
                let dir = tempfile::tempdir().unwrap();
                install(dir.path(), true);
                let original: Vec<(String, Vec<u8>)> = MANAGED_FILES
                    .iter()
                    .map(|(n, _)| ((*n).to_string(), fs::read(dir.path().join(n)).unwrap()))
                    .collect();

                // Build the exact on-disk state a crash at this point leaves.
                let replacement = generate_tls_bundle(&[], 365, true).unwrap();
                let staging = dir.path().join(STAGING_DIR);
                let retired = dir.path().join(RETIRED_DIR);
                create_private_dir(&staging).unwrap();
                create_private_dir(&retired).unwrap();
                for (name, pem, mode) in bundle_members(&replacement) {
                    write_private_file(&staging.join(name), pem.as_bytes(), mode).unwrap();
                }
                let names: Vec<&str> = MANAGED_FILES.iter().map(|(n, _)| *n).collect();
                write_private_file(
                    &dir.path().join(JOURNAL_FILE),
                    serde_json::json!({ "retiring": names, "installing": names })
                        .to_string()
                        .as_bytes(),
                    0o600,
                )
                .unwrap();
                for name in names.iter().rev().take(retired_count) {
                    fs::rename(dir.path().join(name), retired.join(name)).unwrap();
                }
                for name in names.iter().take(installed_count) {
                    fs::rename(staging.join(name), dir.path().join(name)).unwrap();
                }

                let recovered = TlsDir::open(dir.path()).unwrap();
                assert_coherent(dir.path());
                for (name, contents) in &original {
                    assert_eq!(
                        &fs::read(dir.path().join(name)).unwrap(),
                        contents,
                        "{name} was not restored after an interrupt at \
                         retired={retired_count} installed={installed_count}"
                    );
                }
                assert!(!dir.path().join(JOURNAL_FILE).exists());
                assert!(!dir.path().join(STAGING_DIR).exists());
                assert!(!dir.path().join(RETIRED_DIR).exists());
                assert_eq!(recovered.recovered().len(), retired_count);
            }
        }
    }

    #[test]
    fn write_bundle_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = generate_tls_bundle(&[], 365, true).unwrap();
        TlsDir::open(dir.path()).unwrap().install(&bundle).unwrap();

        assert!(dir.path().join("ca.pem").exists());
        assert!(dir.path().join("ca-key.pem").exists());
        assert!(dir.path().join("server.pem").exists());
        assert!(dir.path().join("server-key.pem").exists());
        assert!(dir.path().join("client.pem").exists());
        assert!(dir.path().join("client-key.pem").exists());

        // Verify key file permissions on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let key_perms = fs::metadata(dir.path().join("server-key.pem"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(key_perms & 0o777, 0o600);

            let cert_perms = fs::metadata(dir.path().join("server.pem"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(cert_perms & 0o777, 0o644);
        }
    }
}
