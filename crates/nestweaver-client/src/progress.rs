//! Write-path visibility for long-running daemon commands.
//!
//! A3. During the 2026-08-11 incident a `nestweaver index` sat blocked behind a
//! 12-hour embed and printed **nothing at all** for ten minutes, which read as
//! a hang. Leaving long-running RPCs uncapped is a deliberate design decision;
//! being silent about them was not part of that decision.
//!
//! Everything here adds a MESSAGE, never a cap. Nothing in this module
//! cancels, times out, or otherwise shortens the RPC it accompanies — the
//! pollers only read `brain_status`, which does not take the daemon write lock,
//! and print to stderr.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nestweaver_proto::{BrainStatusResponse, EmbeddingStatus};

use crate::DaemonClient;

/// Wait this long before the first notice, so ordinary fast writes stay quiet.
const DEFAULT_FIRST_NOTICE: Duration = Duration::from_secs(3);
/// Cadence of "you are blocked" notices — frequent, because the operator is
/// staring at a command that looks hung.
const WAITING_REPEAT: Duration = Duration::from_secs(15);
/// Cadence of embed progress notices. Coarser: a 12-hour pass at 15s would be
/// ~2,900 lines of scrollback for no extra information.
const EMBED_REPEAT: Duration = Duration::from_secs(60);

/// Render `3661` seconds as `1h1m`, `95` as `1m35s`, `7` as `7s`.
pub fn format_duration_secs(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m}m")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

/// Render `88131` as `88,131`. Status output is read by humans under pressure.
pub fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One line describing an in-flight embedding pass, or `None` when no pass is
/// running. Shared by `brain status` and the CLI's live embed poller so both
/// render the same numbers the same way.
///
/// Example: `41,230 / 88,131 (47%) — 12m elapsed, 3.1k/h, ETA 4h2m`
pub fn format_embed_progress(status: &EmbeddingStatus) -> Option<String> {
    if !status.pass_active {
        return None;
    }
    let processed = status.pass_processed;
    let total = status.pass_total;
    let elapsed = if status.pass_started_at > 0 {
        (now_unix() - status.pass_started_at).max(0) as u64
    } else {
        0
    };

    // Preflight has not finished counting yet; say so rather than divide by
    // zero and invent a percentage.
    if total == 0 {
        return Some(format!(
            "{} embedded, total not yet counted (preflight) — {} elapsed",
            format_count(processed),
            format_duration_secs(elapsed),
        ));
    }

    let percent = (processed as f64 / total as f64 * 100.0).round() as u64;
    let mut line = format!(
        "{} / {} ({percent}%) — {} elapsed",
        format_count(processed),
        format_count(total),
        format_duration_secs(elapsed),
    );
    // Rate and ETA need a real sample; before that they would be noise.
    if elapsed >= 10 && processed > 0 {
        let per_hour = processed as f64 * 3600.0 / elapsed as f64;
        let remaining = total.saturating_sub(processed) as f64;
        line.push_str(&format!(", {}/h", format_rate(per_hour)));
        if per_hour > 0.0 && remaining > 0.0 {
            let eta_secs = (remaining / per_hour * 3600.0) as u64;
            line.push_str(&format!(", ETA {}", format_duration_secs(eta_secs)));
        }
    }
    Some(line)
}

fn format_rate(per_hour: f64) -> String {
    if per_hour >= 1000.0 {
        format!("{:.1}k", per_hour / 1000.0)
    } else {
        format!("{:.0}", per_hour)
    }
}

/// The daemon write lock's state, as one clause: `` `embed` has held it for
/// 12m32s `` — with embed progress appended only when the holder actually IS
/// an embed, so an index's wait is never annotated with an embed's counters.
///
/// `None` when the lock is free AND nothing is queued. A non-empty
/// `write_queue_depth` with an empty holder is still reported, because
/// "somebody is blocked but status cannot name what on" is exactly the silence
/// this module exists to break.
fn format_write_lock_clause(status: &BrainStatusResponse) -> Option<String> {
    let queued = status.write_queue_depth.max(0);
    if status.write_holder.is_empty() && queued == 0 {
        return None;
    }
    let held = format_duration_secs(status.write_holder_seconds.max(0) as u64);
    let mut clause = if status.write_holder.is_empty() {
        // Should not happen now that every writer stamps the gate, but a
        // daemon older than that change reports exactly this shape.
        "the daemon write lock is held by something that did not identify itself".to_string()
    } else {
        format!("`{}` has held it for {held}", status.write_holder)
    };
    // Only an embed's own progress may be attributed to an embed.
    if status.write_holder == "embed"
        && let Some(progress) = status
            .embedding_status
            .as_ref()
            .and_then(format_embed_progress)
    {
        clause.push_str(&format!(" ({progress})"));
    }
    Some(clause)
}

/// One line telling a blocked caller what is in front of it, or `None` when
/// nothing is.
pub fn format_write_holder(status: &BrainStatusResponse) -> Option<String> {
    let clause = format_write_lock_clause(status)?;
    let mut line = format!("waiting for the daemon write lock; {clause}");
    // `write_queue_depth` counts every waiter including this one, so only
    // mention the queue when somebody else is also in it.
    let others = status.write_queue_depth.saturating_sub(1);
    if others > 0 {
        line.push_str(&format!(" — {others} other write command(s) also queued"));
    }
    Some(line)
}

/// The same facts stated without claiming the caller is the one waiting.
///
/// Used when the holder's RPC name equals the caller's own: `write_holder` is
/// a name, not an identity, so two concurrent `nestweaver index` runs are
/// indistinguishable from one command looking at itself. Rather than stay
/// silent (which left a genuinely blocked client with no output at all) or
/// assert "waiting" (which would be false for the holder), report what status
/// actually knows.
pub fn format_write_lock_contention(status: &BrainStatusResponse) -> Option<String> {
    let clause = format_write_lock_clause(status)?;
    let queued = status.write_queue_depth.max(0);
    if queued == 0 {
        return None;
    }
    Some(format!(
        "the daemon write lock is contended; {clause} — {queued} write command(s) queued behind it"
    ))
}

/// What a poller prints on each tick.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    /// "I am blocked; here is what is in front of me." For write commands.
    ///
    /// `own_rpc` is the daemon RPC this command turns into, so the poller goes
    /// quiet once *we* are the holder rather than reporting the caller as
    /// blocked on itself.
    Waiting { own_rpc: &'static str },
    /// "My own embed pass is at N of M." For the `embed` daemon route.
    EmbedProgress,
}

/// Background poller that prints periodic stderr notices while a long daemon
/// command runs. Dropping it stops the poller; it never affects the command.
pub struct StatusNotifier {
    cancel: Arc<AtomicBool>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl StatusNotifier {
    /// Spawn on the current tokio runtime. `command` names the caller for the
    /// notice prefix (e.g. `nestweaver index`).
    pub fn spawn(db_path: &Path, command: &str, kind: NoticeKind) -> Self {
        let repeat = match kind {
            NoticeKind::Waiting { .. } => WAITING_REPEAT,
            NoticeKind::EmbedProgress => EMBED_REPEAT,
        };
        Self::spawn_with(db_path, command, kind, DEFAULT_FIRST_NOTICE, repeat)
    }

    pub fn spawn_with(
        db_path: &Path,
        command: &str,
        kind: NoticeKind,
        first_notice: Duration,
        repeat: Duration,
    ) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let handle = tokio::spawn(poll_loop(
            db_path.to_path_buf(),
            command.to_string(),
            kind,
            first_notice,
            repeat,
            Arc::clone(&cancel),
        ));
        Self {
            cancel,
            handle: Some(handle),
        }
    }
}

impl Drop for StatusNotifier {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

async fn poll_loop(
    db_path: PathBuf,
    command: String,
    kind: NoticeKind,
    first_notice: Duration,
    repeat: Duration,
    cancel: Arc<AtomicBool>,
) {
    tokio::time::sleep(first_notice).await;
    // A second connection: the caller's client is busy inside its own RPC, and
    // `brain_status` is a read that does not contend for the write lock.
    let mut client = match DaemonClient::connect_existing(&db_path).await {
        Ok(client) => client,
        Err(error) => {
            // Silent failure here would recreate the exact problem this
            // module exists to fix, so say why no progress will appear.
            eprintln!(
                "{command}: cannot report progress — status connection failed ({error:#}). \
                 The command itself is unaffected and is still running."
            );
            return;
        }
    };
    let mut first = true;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        match client.brain_status().await {
            Ok(status) => {
                let line = match kind {
                    // Our own RPC name in `write_holder` is ambiguous: either
                    // we hold the lock, or another client running the same
                    // command does and we are queued behind it. Say what is
                    // true either way rather than going silent on a blocked
                    // client (which is the bug this module exists to fix).
                    NoticeKind::Waiting { own_rpc } if status.write_holder == own_rpc => {
                        format_write_lock_contention(&status)
                    }
                    NoticeKind::Waiting { .. } => format_write_holder(&status),
                    NoticeKind::EmbedProgress => status
                        .embedding_status
                        .as_ref()
                        .and_then(format_embed_progress)
                        .map(|p| format!("embedding: {p}"))
                        .or_else(|| format_write_holder(&status)),
                };
                if let Some(line) = line {
                    if first {
                        eprintln!("{command}: {line} (no timeout — still running)");
                        first = false;
                    } else {
                        eprintln!("{command}: {line}");
                    }
                }
            }
            Err(error) => {
                tracing::debug!("progress poll failed: {error:#}");
            }
        }
        tokio::time::sleep(repeat).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active_pass(processed: u64, total: u64, started_secs_ago: i64) -> EmbeddingStatus {
        EmbeddingStatus {
            state: "embedding".to_string(),
            pass_active: true,
            pass_processed: processed,
            pass_total: total,
            pass_started_at: now_unix() - started_secs_ago,
            pass_scope: "all".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn durations_and_counts_render_for_humans() {
        assert_eq!(format_duration_secs(7), "7s");
        assert_eq!(format_duration_secs(95), "1m35s");
        assert_eq!(format_duration_secs(3661), "1h1m");
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(88131), "88,131");
        assert_eq!(format_count(1234567), "1,234,567");
    }

    #[test]
    fn an_idle_daemon_reports_no_pass_line() {
        assert_eq!(format_embed_progress(&EmbeddingStatus::default()), None);
    }

    #[test]
    fn a_running_pass_reports_count_percent_and_eta() {
        // 3600 nodes in one hour against a 7200 total: 50%, 3.6k/h, ETA 1h.
        let line = format_embed_progress(&active_pass(3600, 7200, 3600)).expect("pass line");
        assert!(line.contains("3,600 / 7,200 (50%)"), "{line}");
        assert!(line.contains("1h0m elapsed"), "{line}");
        assert!(line.contains("3.6k/h"), "{line}");
        assert!(line.contains("ETA 1h0m"), "{line}");
    }

    #[test]
    fn a_pass_still_in_preflight_says_so_instead_of_faking_a_percentage() {
        let line = format_embed_progress(&active_pass(0, 0, 5)).expect("pass line");
        assert!(line.contains("total not yet counted"), "{line}");
        assert!(!line.contains('%'), "{line}");
    }

    #[test]
    fn a_free_write_lock_produces_no_waiting_line() {
        assert_eq!(
            format_write_holder(&BrainStatusResponse::default()),
            None,
            "an idle daemon must not claim a client is blocked"
        );
    }

    /// Review finding: embed progress was appended unconditionally, so a
    /// writer blocked behind an INDEX could be shown an embed's counters as if
    /// they described the index.
    #[test]
    fn a_non_embed_holder_is_never_annotated_with_embed_progress() {
        let status = BrainStatusResponse {
            write_holder: "index_repo".to_string(),
            write_holder_seconds: 300,
            write_queue_depth: 1,
            // A pass genuinely is running — it just is not what holds the lock.
            embedding_status: Some(active_pass(3600, 7200, 300)),
            ..Default::default()
        };
        let line = format_write_holder(&status).expect("waiting line");
        assert!(line.contains("`index_repo` has held it for 5m0s"), "{line}");
        assert!(
            !line.contains("3,600 / 7,200"),
            "an index's wait must not be annotated with an embed's progress: {line}"
        );
    }

    /// Review finding: a non-RPC writer (worker pool, web admin) used to leave
    /// `write_holder` empty, and the notifier then printed nothing at all —
    /// reproducing the exact silence this module exists to remove. Current
    /// daemons always stamp, but an older one still reports this shape.
    #[test]
    fn a_queued_writer_is_told_something_even_when_the_holder_is_unnamed() {
        let status = BrainStatusResponse {
            write_holder: String::new(),
            write_queue_depth: 1,
            ..Default::default()
        };
        let line =
            format_write_holder(&status).expect("a queued writer must never be left in silence");
        assert!(line.contains("waiting for the daemon write lock"), "{line}");
        assert!(line.contains("did not identify itself"), "{line}");
    }

    /// Review finding: `write_holder` is a NAME, not an identity. Two
    /// concurrent `nestweaver index` runs made the blocked one match its own
    /// `own_rpc` and go silent for its entire wait.
    #[test]
    fn same_named_contention_is_reported_rather_than_silenced() {
        let contended = BrainStatusResponse {
            write_holder: "index_repo".to_string(),
            write_holder_seconds: 90,
            write_queue_depth: 1,
            ..Default::default()
        };
        let line = format_write_lock_contention(&contended)
            .expect("a contended lock must produce a line even under a matching name");
        assert!(line.contains("contended"), "{line}");
        assert!(
            line.contains("`index_repo` has held it for 1m30s"),
            "{line}"
        );
        assert!(
            !line.contains("waiting for"),
            "we cannot prove WE are the waiter, so do not claim it: {line}"
        );

        // Sole holder, nothing queued: stay quiet, the command has its own output.
        let uncontended = BrainStatusResponse {
            write_queue_depth: 0,
            ..contended
        };
        assert_eq!(format_write_lock_contention(&uncontended), None);
    }

    #[test]
    fn a_blocked_writer_is_told_what_holds_the_lock_and_how_far_along_it_is() {
        let status = BrainStatusResponse {
            write_holder: "embed".to_string(),
            write_holder_seconds: 752,
            write_queue_depth: 1,
            embedding_status: Some(active_pass(3600, 7200, 752)),
            ..Default::default()
        };
        let line = format_write_holder(&status).expect("waiting line");
        assert!(line.contains("`embed` has held it for 12m32s"), "{line}");
        assert!(line.contains("3,600 / 7,200 (50%)"), "{line}");
        // Depth 1 is this waiter alone — do not report "0 others queued".
        assert!(!line.contains("also queued"), "{line}");

        let contended = BrainStatusResponse {
            write_queue_depth: 3,
            ..status
        };
        let line = format_write_holder(&contended).expect("waiting line");
        assert!(
            line.contains("2 other write command(s) also queued"),
            "{line}"
        );
    }
}
