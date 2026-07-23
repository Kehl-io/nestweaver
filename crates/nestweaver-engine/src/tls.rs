//! TLS certificate generation for NestWeaver server mode.
//!
//! Generates a self-signed CA certificate, a server certificate signed by
//! that CA, and optionally a client certificate for mTLS. Uses the `rcgen`
//! crate to produce PEM files compatible with `--tls-cert` / `--tls-key`.

use std::fs;
use std::net::IpAddr;
use std::path::Path;

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
/// not-after computation and panicked — F-23). The CLI flag enforces the same
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

/// Write the TLS bundle to PEM files in the specified directory.
///
/// Creates:
/// - `ca.pem` / `ca-key.pem`
/// - `server.pem` / `server-key.pem`
/// - `client.pem` / `client-key.pem` (if present)
///
/// Key files are set to mode 0600; cert files to 0644.
pub fn write_tls_bundle(output_dir: &Path, bundle: &TlsBundle) -> Result<()> {
    fs::create_dir_all(output_dir)
        .with_context(|| format!("create output dir: {}", output_dir.display()))?;

    write_cert(output_dir, "ca.pem", &bundle.ca_cert_pem)?;
    write_key(output_dir, "ca-key.pem", &bundle.ca_key_pem)?;
    write_cert(output_dir, "server.pem", &bundle.server_cert_pem)?;
    write_key(output_dir, "server-key.pem", &bundle.server_key_pem)?;

    if let (Some(cert), Some(key)) = (&bundle.client_cert_pem, &bundle.client_key_pem) {
        write_cert(output_dir, "client.pem", cert)?;
        write_key(output_dir, "client-key.pem", key)?;
    }

    Ok(())
}

fn write_cert(dir: &Path, name: &str, pem: &str) -> Result<()> {
    let path = dir.join(name);
    fs::write(&path, pem).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    set_permissions(&path, 0o644)?;
    Ok(())
}

fn write_key(dir: &Path, name: &str, pem: &str) -> Result<()> {
    let path = dir.join(name);
    fs::write(&path, pem).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    set_permissions(&path, 0o600)?;
    Ok(())
}

#[cfg(unix)]
fn set_permissions(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = fs::Permissions::from_mode(mode);
    fs::set_permissions(path, perms)
        .with_context(|| format!("set permissions on {}", path.display()))?;
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

    #[test]
    fn rejects_invalid_validity_days() {
        // F-23: 0 and absurdly large day counts must be a clean error, not a
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

    #[test]
    fn write_bundle_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = generate_tls_bundle(&[], 365, true).unwrap();
        write_tls_bundle(dir.path(), &bundle).unwrap();

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
