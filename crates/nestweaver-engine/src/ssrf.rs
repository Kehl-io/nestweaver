//! SSRF prevention for repo URLs — shared by the admin add-repo gate and the
//! clone/fetch-time guard.
//!
//! Two layers live here:
//! - **Synchronous, pure checks** ([`validate_repo_url`]) — scheme allowlist plus
//!   literal/alternate-encoded/IPv6-embedded internal-IP detection. No I/O.
//! - **Resolve-time enforcement** ([`guard_git_url`]) — resolves the host, rejects
//!   if any address is internal, and (for http/https) pins the validated public IP
//!   via `git -c http.curloptResolve=…` so git can't re-resolve to an internal
//!   target between our check and its connect (DNS-rebinding defense).
//!
//! The engine already depends on `url` + `std::net`; no extra dependency.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// True if a V4 address is an internal/non-routable target we must never reach
/// (loopback, RFC1918 private, link-local, CGNAT/RFC 6598 `100.64.0.0/10`, or
/// the unspecified `0.0.0.0`, which routes to loopback on some platforms).
pub fn v4_is_internal(v4: Ipv4Addr) -> bool {
    let octets = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        // CGNAT / Shared Address Space (RFC 6598): 100.64.0.0/10
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
}

/// Extract an embedded IPv4 from the IPv6 transition forms that can smuggle an
/// internal V4 target past a naive IPv6-only check:
/// - NAT64 well-known prefix `64:ff9b::/96` (embedded V4 in the low 32 bits)
/// - 6to4 `2002:V4::/16` (embedded V4 in segments 1..3)
/// - IPv4-compatible `::a.b.c.d` (low 32 bits when the high 96 bits are zero and
///   it is not the IPv4-mapped `::ffff:a.b.c.d` form, which is handled separately)
pub fn embedded_ipv4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let seg = v6.segments();
    let v4_from =
        |hi: u16, lo: u16| Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
    // NAT64 64:ff9b::/96 — IPv4 in the low 32 bits.
    if seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2..6] == [0, 0, 0, 0] {
        return Some(v4_from(seg[6], seg[7]));
    }
    // 6to4 2002:V4::/16 — IPv4 in segments 1..3.
    if seg[0] == 0x2002 {
        return Some(v4_from(seg[1], seg[2]));
    }
    // IPv4-compatible ::a.b.c.d — high 96 bits zero (excludes ::ffff:a.b.c.d,
    // whose segment 5 is 0xffff), and not the all-zero unspecified address.
    if seg[0..6] == [0, 0, 0, 0, 0, 0] && (seg[6] != 0 || seg[7] != 0) {
        return Some(v4_from(seg[6], seg[7]));
    }
    None
}

/// True if `ip` is an internal/private target SSRF must never reach. Handles
/// raw V4/V6, IPv4-mapped (`::ffff:a.b.c.d`), and the IPv6 transition forms
/// (NAT64 / 6to4 / v4-compatible) that can embed an internal V4. Pure — no I/O.
pub fn ip_is_internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_is_internal(v4),
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            // IPv4-mapped (::ffff:a.b.c.d) hiding an internal V4 target.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4_is_internal(v4);
            }
            // Unique-local fc00::/7 (is_unique_local is nightly-only).
            if (v6.segments()[0] & 0xfe00) == 0xfc00 {
                return true;
            }
            // Link-local fe80::/10 (is_unicast_link_local is nightly-only).
            if (v6.segments()[0] & 0xffc0) == 0xfe80 {
                return true;
            }
            // NAT64 / 6to4 / v4-compatible embedding an internal V4 target.
            embedded_ipv4(v6).is_some_and(v4_is_internal)
        }
    }
}

/// Parse one IPv4 "part" using the inet_aton number grammar that alternate-
/// encoding SSRF payloads abuse: hex (`0x`/`0X` prefix), octal (leading `0`),
/// or decimal. Returns the numeric value, which the caller range-checks.
pub fn parse_ipv4_part(part: &str) -> Option<u64> {
    if part.is_empty() {
        return None;
    }
    let (radix, digits) =
        if let Some(hex) = part.strip_prefix("0x").or_else(|| part.strip_prefix("0X")) {
            (16, hex)
        } else if part.len() > 1 && part.starts_with('0') {
            (8, &part[1..])
        } else {
            (10, part)
        };
    if digits.is_empty() {
        return None;
    }
    u64::from_str_radix(digits, radix).ok()
}

/// Parse an alternate-encoded IPv4 host into an `Ipv4Addr`. Covers the inet_aton
/// forms that `url::Url` leaves as opaque domain strings for non-special schemes
/// (git/ssh): single decimal (`2130706433`), hex (`0x7f000001`), octal
/// (`017700000001`), and 2–4 dotted parts in any of those bases. Returns `None`
/// for anything that isn't a fully-numeric host (e.g. a real DNS name).
pub fn parse_numeric_ipv4(host: &str) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let nums: Vec<u64> = parts
        .iter()
        .map(|p| parse_ipv4_part(p))
        .collect::<Option<Vec<_>>>()?;
    let addr: u32 = match nums.as_slice() {
        // a — the whole 32-bit address.
        [a] => u32::try_from(*a).ok()?,
        // a.b — a is the top octet, b the low 24 bits.
        [a, b] => {
            if *a > 0xff || *b > 0x00ff_ffff {
                return None;
            }
            ((*a as u32) << 24) | (*b as u32)
        }
        // a.b.c — a, b are octets, c the low 16 bits.
        [a, b, c] => {
            if *a > 0xff || *b > 0xff || *c > 0xffff {
                return None;
            }
            ((*a as u32) << 24) | ((*b as u32) << 16) | (*c as u32)
        }
        // a.b.c.d — standard dotted quad.
        [a, b, c, d] => {
            if [a, b, c, d].iter().any(|n| **n > 0xff) {
                return None;
            }
            ((*a as u32) << 24) | ((*b as u32) << 16) | ((*c as u32) << 8) | (*d as u32)
        }
        _ => return None,
    };
    Some(Ipv4Addr::from(addr))
}

/// Parse a URL host string into an `IpAddr` if it denotes a literal address —
/// including bracketed IPv6 (`[::1]`) and the alternate IPv4 encodings
/// (decimal/hex/octal) that bypass `url::Url`'s IP detection on non-special
/// schemes. Returns `None` for genuine DNS hostnames. Pure — no DNS lookup.
pub fn parse_host_as_ip(host: &str) -> Option<IpAddr> {
    // `host_str()` brackets IPv6 literals (e.g. `[::1]`); strip them so the
    // address parses.
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);

    // Standard dotted-decimal IPv4 or an IPv6 literal.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(ip);
    }

    // Alternate IPv4 encodings left as opaque domain strings by `url::Url`.
    parse_numeric_ipv4(host).map(IpAddr::V4)
}

/// True if ANY resolved address is internal. Split out from the DNS lookup so it
/// can be unit-tested with synthetic address lists (no live DNS). See
/// `resolve_host` for the blocking lookup that feeds this.
pub fn any_resolved_ip_is_internal(addrs: &[IpAddr]) -> bool {
    addrs.iter().copied().any(ip_is_internal)
}

/// Blocking DNS resolution of a hostname to every address it maps to, via the
/// system resolver (`std::net`). Returns an error on resolution failure so
/// callers fail closed — an unresolvable host is treated as potentially
/// internal rather than silently allowed through. MUST run on a blocking
/// thread (it blocks).
pub fn resolve_host(host: &str) -> Result<Vec<IpAddr>, String> {
    use std::net::ToSocketAddrs;
    match (host, 0u16).to_socket_addrs() {
        Ok(iter) => Ok(iter.map(|sa| sa.ip()).collect()),
        Err(e) => {
            tracing::warn!("DNS resolution failed for host '{host}': {e}");
            Err(format!("DNS resolution failed for '{host}': {e}"))
        }
    }
}

/// Extract the DNS hostname from a repo URL that still needs resolve-time SSRF
/// validation — i.e. a host that is NOT a literal/encoded IP (those are checked
/// synchronously in `validate_repo_url`). Returns `None` for IP literals or URLs
/// without a host.
pub fn host_to_resolve(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if parse_host_as_ip(host).is_some() {
        return None;
    }
    Some(host.to_string())
}

/// Validate a repo URL's scheme and host to prevent SSRF.
///
/// Rejects unsupported schemes (only https/http/ssh are allowed) and any
/// host that resolves to an internal/private target — `localhost`, the cloud
/// metadata endpoint, and private/loopback/link-local/unique-local IP ranges
/// (OWASP SSRF Prevention Cheat Sheet). Literal, alternate-encoded
/// (decimal/hex/octal), and IPv6-embedded IPv4 forms are all covered here
/// synchronously; DNS hostnames are additionally resolve-checked at add-time
/// (see `add_repo`) and at clone/fetch-time (see [`guard_git_url`]). The
/// returned `Err` is the user-facing message.
pub fn validate_repo_url(url: &str) -> Result<(), String> {
    // A legitimate git remote URL never contains raw whitespace or control
    // characters. Reject them at the boundary so a malformed URL (e.g. one with
    // an embedded shell fragment like `.../y; rm -rf /`) can't be accepted and
    // persisted into the config as a permanently-failing repo. Defense-in-depth
    // only: the clone runs via argv with a `--` separator (no shell), so such a
    // URL is inert regardless — this just keeps garbage out of the config.
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "invalid URL '{url}': contains whitespace or control characters"
        ));
    }

    // Validate URL scheme to prevent SSRF via file:// or other unexpected schemes.
    let allowed_schemes = ["https", "http", "ssh"];
    let parsed = match url::Url::parse(url) {
        Ok(p) if allowed_schemes.contains(&p.scheme()) => p,
        Ok(parsed) => {
            return Err(format!(
                "unsupported URL scheme '{}': allowed schemes are {}",
                parsed.scheme(),
                allowed_schemes.join(", ")
            ));
        }
        Err(e) => {
            return Err(format!("invalid URL '{url}': {e}"));
        }
    };

    // SSRF prevention: reject internal/private targets (OWASP SSRF Prevention
    // Cheat Sheet). `parse_host_as_ip` also catches alternate IPv4 encodings
    // (decimal/hex/octal) that `url::Url` leaves as opaque domain strings, and
    // `ip_is_internal` unwraps IPv6-embedded IPv4 (mapped/NAT64/6to4/compatible).
    if let Some(host) = parsed.host_str() {
        if host == "localhost" || host == "metadata.google.internal" {
            return Err(format!(
                "rejected hostname '{host}': internal addresses not allowed"
            ));
        }
        if let Some(ip) = parse_host_as_ip(host)
            && ip_is_internal(ip)
        {
            return Err(format!(
                "rejected IP '{ip}': private/loopback addresses not allowed"
            ));
        }
    }

    Ok(())
}

/// Whether a config-declared repo URL is safe to enqueue during a reload.
///
/// Applies the same synchronous SSRF checks as `add_repo` (`validate_repo_url`)
/// so repos loaded from `instance.toml` can't smuggle in an internal/private
/// target that the add-repo API would reject. DNS resolution is intentionally
/// not performed here — config reload must stay non-blocking — so only the
/// synchronous checks (scheme, literal/encoded internal IPs, localhost/metadata
/// hostnames) apply; that matches add_repo's pre-resolution gate. The
/// clone/fetch-time [`guard_git_url`] performs the resolve-time enforcement.
pub fn config_repo_url_allowed(url: &str) -> bool {
    validate_repo_url(url).is_ok()
}

// ── Clone/fetch-time guard ──────────────────────────────────────────────────

/// Error returned by [`guard_git_url`] when a URL must not be cloned/fetched.
#[derive(Debug, Clone)]
pub enum SsrfError {
    /// The URL could not be parsed.
    InvalidUrl(String),
    /// The scheme is not one we allow over the network (git:// and friends).
    UnsupportedScheme(String),
    /// A synchronous [`validate_repo_url`] check rejected the URL.
    Rejected(String),
    /// The host resolved (or is a literal) to an internal/private address.
    ResolvesInternal(String),
}

impl std::fmt::Display for SsrfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SsrfError::InvalidUrl(m) => write!(f, "invalid repo URL: {m}"),
            SsrfError::UnsupportedScheme(s) => write!(
                f,
                "unsupported URL scheme '{s}': only https/http/ssh remotes may be cloned"
            ),
            SsrfError::Rejected(m) => write!(f, "{m}"),
            SsrfError::ResolvesInternal(h) => write!(
                f,
                "rejected host '{h}': resolves to an internal/private address"
            ),
        }
    }
}

impl std::error::Error for SsrfError {}

/// The git config args a guarded clone/fetch must prepend (`-c key=val` pairs).
///
/// For http/https these pin the validated public IP via libcurl's `--resolve`
/// equivalent and disable redirects; for ssh and file:// it is empty.
#[derive(Debug, Clone, Default)]
pub struct GitNetGuard {
    /// Args to prepend before the git subcommand (e.g. `["-c", "http.curloptResolve=…"]`).
    pub config_args: Vec<String>,
}

/// Build the `git -c http.curloptResolve=…` value that pins `host:port` to a
/// specific connect IP (libcurl `--resolve` equivalent). git connects to exactly
/// this IP and skips its own DNS; TLS SNI/cert validation still use `host`.
/// IPv6 addresses are bracketed per the curl resolve-host grammar.
pub fn resolve_config_arg(host: &str, port: u16, ip: IpAddr) -> String {
    let ip_str = match ip {
        IpAddr::V6(v6) => format!("[{v6}]"),
        IpAddr::V4(v4) => v4.to_string(),
    };
    format!("http.curloptResolve={host}:{port}:{ip_str}")
}

/// Pick the address to pin from a resolved set, preferring IPv4 (its
/// curloptResolve form is unambiguous across git versions) and falling back to
/// the first address otherwise.
fn pick_pin_ip(addrs: &[IpAddr]) -> Option<IpAddr> {
    addrs
        .iter()
        .copied()
        .find(IpAddr::is_ipv4)
        .or_else(|| addrs.first().copied())
}

/// Validate a repo URL immediately before a git clone/fetch and return the git
/// config args to pin the connection (DNS-rebinding defense).
///
/// Behavior by scheme:
/// - `https`/`http` — runs [`validate_repo_url`], resolves the host, rejects if
///   any resolved address is internal, then pins the validated public IP via
///   `-c http.curloptResolve=host:port:ip` **and** `-c http.followRedirects=false`
///   (block redirect-based rebinding).
/// - `ssh` — runs the same validate + resolve-and-reject, but cannot pin via the
///   CLI, so `config_args` is empty (residual sub-second TOCTOU; documented).
/// - `file://`, `git://`, and any other scheme — **rejected**. `file://` is a
///   local clone source (arbitrary-repo disclosure) and `git://` is
///   plaintext/un-pinnable; neither may be reached over the clone/fetch path.
///   This guard is the *last* line of defense — it must reject non-remote
///   schemes itself, not rely on the entry-point allowlist having run first.
///   (Hermetic tests that clone on-disk `file://` fixtures opt in via a
///   `cfg(test)`-only allowance in `bare_clone`, never in production.)
///
/// Fails **closed** on DNS resolution error: an unresolvable host is rejected
/// (treated as potentially internal), matching the add-time gate.
pub fn guard_git_url(url: &str) -> Result<GitNetGuard, SsrfError> {
    let parsed =
        url::Url::parse(url).map_err(|e| SsrfError::InvalidUrl(format!("'{url}': {e}")))?;
    let scheme = parsed.scheme();

    // Only schemes we can either pin (http/https) or validate-immediately (ssh)
    // are allowed over the network. file:// (local-repo disclosure) and git://
    // (plaintext, un-pinnable) are rejected here as the last line of defense.
    if !matches!(scheme, "https" | "http" | "ssh") {
        return Err(SsrfError::UnsupportedScheme(scheme.to_string()));
    }

    // Synchronous checks: scheme allowlist + literal/encoded/embedded internal IP.
    validate_repo_url(url).map_err(SsrfError::Rejected)?;

    let host = parsed
        .host_str()
        .ok_or_else(|| SsrfError::InvalidUrl(format!("'{url}': missing host")))?;

    // Resolve the host (literal IP → use it directly; DNS name → look it up) and
    // reject if anything it points at is internal. DNS failure = fail closed.
    let resolved: Vec<IpAddr> = match parse_host_as_ip(host) {
        Some(ip) => vec![ip],
        None => resolve_host(host).map_err(|_| SsrfError::ResolvesInternal(host.to_string()))?,
    };
    if any_resolved_ip_is_internal(&resolved) {
        return Err(SsrfError::ResolvesInternal(host.to_string()));
    }

    // ssh can't pin the connect IP via the CLI — validate-immediately only.
    if scheme == "ssh" {
        return Ok(GitNetGuard::default());
    }

    // http/https: pin the validated public IP and disable redirects.
    let port = parsed.port_or_known_default().unwrap_or(443);
    let mut config_args = Vec::new();
    if let Some(ip) = pick_pin_ip(&resolved) {
        config_args.push("-c".to_string());
        config_args.push(resolve_config_arg(host, port, ip));
    }
    config_args.push("-c".to_string());
    config_args.push("http.followRedirects=false".to_string());
    Ok(GitNetGuard { config_args })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_repo_url_rejects_internal_targets() {
        // Bare IPv4 hosts, wrapped in an allowed scheme, must be rejected.
        for host in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "169.254.1.1",
            "0.0.0.0",
            "100.64.0.1",      // CGNAT (RFC 6598)
            "100.127.255.254", // CGNAT upper bound
        ] {
            let url = format!("https://{host}/repo");
            assert!(
                validate_repo_url(&url).is_err(),
                "expected {url} to be rejected"
            );
        }

        // Bare IPv6 hosts (bracketed in URLs) must be rejected: loopback,
        // link-local, unique-local, and IPv4-mapped private.
        for host in ["::1", "fe80::1", "fd00::1", "fc00::1", "::ffff:192.168.1.1"] {
            let url = format!("https://[{host}]/repo");
            assert!(
                validate_repo_url(&url).is_err(),
                "expected {url} to be rejected"
            );
        }

        // Full URLs: unspecified IPv6, disallowed scheme, internal hostname.
        for url in ["http://[::]/", "file:///etc/passwd", "git://localhost/repo"] {
            assert!(
                validate_repo_url(url).is_err(),
                "expected {url} to be rejected"
            );
        }
    }

    #[test]
    fn validate_repo_url_accepts_public_https() {
        for url in [
            "https://github.com/acme/api.git",
            "https://gitlab.com/acme/widgets.git",
        ] {
            assert!(
                validate_repo_url(url).is_ok(),
                "expected {url} to be accepted, got {:?}",
                validate_repo_url(url)
            );
        }
    }

    #[test]
    fn validate_repo_url_rejects_whitespace_and_control_chars() {
        for url in [
            "https://github.com/x/y; rm -rf /", // embedded space
            "https://github.com/a b/c",         // space in path
            "https://github.com/x/y\n",         // trailing newline
            "https://github.com/x/y\ttab",      // tab
        ] {
            assert!(
                validate_repo_url(url).is_err(),
                "expected {url:?} to be rejected for whitespace/control chars"
            );
        }
        // A well-formed URL is still accepted.
        assert!(validate_repo_url("https://github.com/acme/api").is_ok());
    }

    #[test]
    fn parse_numeric_ipv4_decodes_alternate_encodings() {
        // Loopback in decimal / hex / octal — the classic SSRF bypasses.
        assert_eq!(
            parse_numeric_ipv4("2130706433"),
            Some(Ipv4Addr::new(127, 0, 0, 1))
        );
        assert_eq!(
            parse_numeric_ipv4("0x7f000001"),
            Some(Ipv4Addr::new(127, 0, 0, 1))
        );
        assert_eq!(
            parse_numeric_ipv4("017700000001"),
            Some(Ipv4Addr::new(127, 0, 0, 1))
        );
        // A public address in hex stays public.
        assert_eq!(
            parse_numeric_ipv4("0x08080808"),
            Some(Ipv4Addr::new(8, 8, 8, 8))
        );
        // Mixed-base dotted (inet_aton) forms.
        assert_eq!(
            parse_numeric_ipv4("0x7f.0.0.1"),
            Some(Ipv4Addr::new(127, 0, 0, 1))
        );
        assert_eq!(
            parse_numeric_ipv4("127.1"),
            Some(Ipv4Addr::new(127, 0, 0, 1))
        );
        // Genuine DNS names and out-of-range numbers are not IPs.
        assert_eq!(parse_numeric_ipv4("github.com"), None);
        assert_eq!(parse_numeric_ipv4("0x1_0000_0000"), None);
        assert_eq!(parse_numeric_ipv4("1.2.3.4.5"), None);
        assert_eq!(parse_numeric_ipv4("256.0.0.1"), None);
    }

    #[test]
    fn parse_host_as_ip_handles_literals_and_encodings() {
        // Alternate-encoded loopback resolves to an IpAddr we can range-check.
        for host in ["2130706433", "0x7f000001", "017700000001"] {
            assert_eq!(
                parse_host_as_ip(host),
                Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))),
                "host {host} should decode to 127.0.0.1"
            );
        }
        // Public hex encoding decodes to the real public address.
        assert_eq!(
            parse_host_as_ip("0x08080808"),
            Some(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)))
        );
        // Bracketed IPv6 literal (as `host_str()` returns it).
        assert_eq!(
            parse_host_as_ip("[::1]"),
            Some(IpAddr::V6(Ipv6Addr::LOCALHOST))
        );
        // Plain dotted quad.
        assert_eq!(
            parse_host_as_ip("10.0.0.1"),
            Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)))
        );
        // A real DNS name is not an IP literal.
        assert_eq!(parse_host_as_ip("github.com"), None);
    }

    #[test]
    fn ip_is_internal_flags_encoded_and_embedded_loopback() {
        // Alternate-encoded loopback (decimal/hex/octal) → internal.
        for host in ["2130706433", "0x7f000001", "017700000001"] {
            let ip = parse_host_as_ip(host).expect("decodes to an IP");
            assert!(ip_is_internal(ip), "{host} ({ip}) should be internal");
        }

        // IPv6 transition forms embedding an internal V4 → internal.
        for s in [
            "64:ff9b::7f00:1",    // NAT64 -> 127.0.0.1
            "2002:7f00:1::",      // 6to4 -> 127.0.0.1
            "::127.0.0.1",        // v4-compatible -> 127.0.0.1
            "64:ff9b::a00:1",     // NAT64 -> 10.0.0.1 (private)
            "2002:c0a8:101::",    // 6to4 -> 192.168.1.1 (private)
            "::ffff:192.168.1.1", // v4-mapped private
        ] {
            let ip: IpAddr = s.parse().expect("valid IPv6 literal");
            assert!(ip_is_internal(ip), "{s} should be internal");
        }

        // Raw internal V4/V6 literals stay flagged.
        for s in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.1.1",
            "0.0.0.0",
            "100.64.0.1",    // CGNAT (RFC 6598)
            "100.100.100.1", // CGNAT mid-range
            "::1",
            "fe80::1",
            "fc00::1",
        ] {
            let ip: IpAddr = s.parse().expect("valid IP literal");
            assert!(ip_is_internal(ip), "{s} should be internal");
        }
    }

    #[test]
    fn ip_is_internal_allows_public_addresses() {
        // Public V4/V6 and the embedded forms carrying public V4 are allowed.
        for s in [
            "8.8.8.8",
            "1.1.1.1",
            "100.128.0.1",          // 100.128.x.x is outside CGNAT /10, public
            "100.63.255.255",       // just below CGNAT range, public
            "2001:4860:4860::8888", // Google public DNS, IPv6
            "64:ff9b::808:808",     // NAT64 -> 8.8.8.8 (public)
            "2002:0808:0808::",     // 6to4 -> 8.8.8.8 (public)
            "::8.8.8.8",            // v4-compatible -> 8.8.8.8 (public)
        ] {
            let ip: IpAddr = s.parse().expect("valid IP literal");
            assert!(!ip_is_internal(ip), "{s} should be allowed (public)");
        }
        // The decimal/hex public encoding too.
        assert!(!ip_is_internal(
            parse_host_as_ip("0x08080808").expect("decodes")
        ));
    }

    #[test]
    fn validate_repo_url_rejects_alternate_encoded_internal_hosts() {
        // git/ssh are non-special schemes, so `url::Url` keeps these numeric
        // hosts as opaque strings — exactly the bypass these checks close.
        for url in [
            "git://2130706433/repo",
            "ssh://0x7f000001/repo",
            "git://017700000001/repo",
            "git://[64:ff9b::7f00:1]/repo",
            "git://[2002:7f00:1::]/repo",
            "git://[::127.0.0.1]/repo",
        ] {
            assert!(
                validate_repo_url(url).is_err(),
                "expected {url} to be rejected"
            );
        }
    }

    #[test]
    fn any_resolved_ip_is_internal_checks_each_address() {
        // Mixed list with one internal address → rejected.
        let mixed = [
            IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        ];
        assert!(any_resolved_ip_is_internal(&mixed));

        // All-public list → allowed.
        let public = [
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            IpAddr::V6("2001:4860:4860::8888".parse().unwrap()),
        ];
        assert!(!any_resolved_ip_is_internal(&public));

        // Empty list (no addresses) → not internal by itself, but callers
        // should never reach this state: resolve_host now returns Err on
        // failure (fail-closed), so an empty vec only arises from a literal IP.
        assert!(!any_resolved_ip_is_internal(&[]));
    }

    #[test]
    fn host_to_resolve_skips_ip_literals() {
        // DNS names need resolve-time validation.
        assert_eq!(
            host_to_resolve("https://github.com/acme/api.git"),
            Some("github.com".to_string())
        );
        // Literal and alternate-encoded IPs are already checked synchronously,
        // so they need no DNS lookup.
        assert_eq!(host_to_resolve("https://93.184.216.34/repo"), None);
        assert_eq!(host_to_resolve("git://2130706433/repo"), None);
        assert_eq!(host_to_resolve("https://[2606:2800:220:1::1]/repo"), None);
    }

    #[test]
    fn config_repo_url_allowed_rejects_internal_and_accepts_public() {
        // Internal/private targets declared in config must be skipped by reload.
        assert!(!config_repo_url_allowed("http://localhost/repo.git"));
        assert!(!config_repo_url_allowed("https://127.0.0.1/repo.git"));
        assert!(!config_repo_url_allowed(
            "http://169.254.169.254/latest/meta-data"
        ));
        assert!(!config_repo_url_allowed("https://10.0.0.5/internal.git"));
        // Alternate-encoded loopback (decimal) must also be rejected.
        assert!(!config_repo_url_allowed("git://2130706433/repo"));
        // Public HTTPS repos remain allowed.
        assert!(config_repo_url_allowed("https://github.com/acme/api.git"));
    }

    // ── guard_git_url (offline, IP-literal hosts only — no live DNS) ─────────

    #[test]
    fn guard_git_url_rejects_file_scheme() {
        // Defense-in-depth: the last-line-of-defense guard must itself reject
        // file:// (arbitrary local-repo disclosure), not fail open and rely on
        // the entry-point scheme allowlist. Any future caller reaching the guard
        // without the entry-point check must still be stopped here.
        match guard_git_url("file:///tmp/some/repo") {
            Err(SsrfError::UnsupportedScheme(s)) => assert_eq!(s, "file"),
            other => panic!("expected UnsupportedScheme(\"file\"), got {other:?}"),
        }
    }

    #[test]
    fn guard_git_url_rejects_internal_ip_literals() {
        for url in [
            "https://127.0.0.1/x",
            "http://10.0.0.1/x",
            "https://[::1]/x",
            "ssh://192.168.1.1/x",
            "https://2130706433/x", // alternate-encoded loopback
        ] {
            assert!(guard_git_url(url).is_err(), "expected {url} to be rejected");
        }
    }

    #[test]
    fn guard_git_url_rejects_unsupported_schemes() {
        for url in ["git://github.com/x", "ftp://example.com/x"] {
            match guard_git_url(url) {
                Err(SsrfError::UnsupportedScheme(_)) => {}
                other => panic!("expected UnsupportedScheme for {url}, got {other:?}"),
            }
        }
    }

    #[test]
    fn guard_git_url_pins_public_ip_literal() {
        // A public IP literal host resolves to itself → Ok with pin args.
        let guard = guard_git_url("https://93.184.216.34/x").expect("public IP literal allowed");
        assert_eq!(
            guard.config_args,
            vec![
                "-c".to_string(),
                "http.curloptResolve=93.184.216.34:443:93.184.216.34".to_string(),
                "-c".to_string(),
                "http.followRedirects=false".to_string(),
            ]
        );

        // Explicit port is reflected in the pin.
        let guard =
            guard_git_url("http://93.184.216.34:8080/x").expect("public IP literal allowed");
        assert_eq!(
            guard.config_args,
            vec![
                "-c".to_string(),
                "http.curloptResolve=93.184.216.34:8080:93.184.216.34".to_string(),
                "-c".to_string(),
                "http.followRedirects=false".to_string(),
            ]
        );
    }

    #[test]
    fn resolve_config_arg_formats_v4_and_v6() {
        assert_eq!(
            resolve_config_arg(
                "github.com",
                443,
                IpAddr::V4(Ipv4Addr::new(140, 82, 112, 3))
            ),
            "http.curloptResolve=github.com:443:140.82.112.3"
        );
        // IPv6 addresses are bracketed.
        let v6: IpAddr = "2606:2800:220:1::1".parse().unwrap();
        assert_eq!(
            resolve_config_arg("example.com", 443, v6),
            "http.curloptResolve=example.com:443:[2606:2800:220:1::1]"
        );
    }
}
