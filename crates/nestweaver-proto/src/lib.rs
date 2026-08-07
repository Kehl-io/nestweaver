pub mod nestweaver_daemon_v1 {
    tonic::include_proto!("nestweaver.daemon.v1");
}

pub use nestweaver_daemon_v1::*;

#[cfg(test)]
mod embedding_telemetry_contract_tests {
    use super::*;

    #[test]
    fn status_and_hybrid_responses_expose_additive_embedding_telemetry() {
        let status = EmbeddingStatus {
            state: "ready".to_string(),
            backend: "local".to_string(),
            requested_device: "metal".to_string(),
            selected_device: "metal".to_string(),
            model_id: "model".to_string(),
            error: String::new(),
            metal_compiled: true,
            fallback_used: false,
        };
        let brain_status = BrainStatusResponse {
            embedding_status: Some(status),
            ..Default::default()
        };
        assert_eq!(
            brain_status.embedding_status.unwrap().selected_device,
            "metal"
        );

        let search = BrainSearchResponse {
            semantic_applied: false,
            degraded_components: Vec::new(),
            ..Default::default()
        };
        assert!(!search.semantic_applied);
        assert!(search.degraded_components.is_empty());

        let context = BrainContextResponse {
            result_json: "{}".to_string(),
            semantic_applied: false,
            degraded_components: vec!["semantic".to_string()],
        };
        assert_eq!(context.degraded_components, ["semantic"]);
    }
}

/// Fail-closed state machine for daemon indexing progress streams.
///
/// gRPC transport success does not imply that indexing succeeded: logical
/// failures are reported in-band as [`Phase::Error`]. Consumers must also
/// reject streams that end without [`Phase::Done`] and malformed streams that
/// continue after a terminal event.
#[derive(Debug, Default)]
pub struct IndexProgressTracker {
    seen_event: bool,
    last_phase: Option<i32>,
    last_message: String,
    terminal: Option<(Phase, String)>,
}

impl IndexProgressTracker {
    /// Record one in-band progress event.
    pub fn observe(&mut self, progress: &IndexProgress) -> Result<(), IndexProgressError> {
        if let Some((terminal, terminal_message)) = &self.terminal {
            return Err(IndexProgressError::AfterTerminal {
                terminal: match terminal {
                    Phase::Done => "Done",
                    Phase::Error => "Error",
                    _ => "unknown terminal phase",
                },
                terminal_message: terminal_message.clone(),
                phase: progress.phase,
                message: progress.message.clone(),
            });
        }

        self.seen_event = true;
        self.last_phase = Some(progress.phase);
        self.last_message.clone_from(&progress.message);

        if let Ok(phase) = Phase::try_from(progress.phase)
            && matches!(phase, Phase::Done | Phase::Error)
        {
            self.terminal = Some((phase, progress.message.clone()));
        }

        Ok(())
    }

    /// Classify end-of-stream. Only a terminal `Done` is successful.
    pub fn finish(self) -> Result<String, IndexProgressError> {
        match self.terminal {
            Some((Phase::Done, message)) => Ok(message),
            Some((Phase::Error, message)) => Err(IndexProgressError::Reported { message }),
            Some((phase, _)) => Err(IndexProgressError::Truncated {
                last_phase: phase as i32,
                last_message: self.last_message,
            }),
            None if !self.seen_event => Err(IndexProgressError::Empty),
            None => Err(IndexProgressError::Truncated {
                last_phase: self.last_phase.unwrap_or(-1),
                last_message: self.last_message,
            }),
        }
    }
}

/// Logical protocol failures detected while consuming an index stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexProgressError {
    Empty,
    Truncated {
        last_phase: i32,
        last_message: String,
    },
    Reported {
        message: String,
    },
    AfterTerminal {
        terminal: &'static str,
        terminal_message: String,
        phase: i32,
        message: String,
    },
}

impl std::fmt::Display for IndexProgressError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(formatter, "index progress stream was empty"),
            Self::Truncated {
                last_phase,
                last_message,
            } => write!(
                formatter,
                "index progress stream ended before completion (last phase {last_phase}: {last_message})"
            ),
            Self::Reported { message } => {
                write!(formatter, "daemon reported an index error: {message}")
            }
            Self::AfterTerminal {
                terminal,
                terminal_message,
                phase,
                message,
            } => write!(
                formatter,
                "index progress event after terminal {terminal} ({terminal_message}); late phase {phase}: {message}"
            ),
        }
    }
}

impl std::error::Error for IndexProgressError {}

/// Consume a daemon index stream to EOF while reporting each non-transport
/// event to the caller. This keeps terminal-state semantics identical for CLI
/// and MCP callers while allowing each frontend to render progress differently.
pub async fn consume_index_progress<S, F>(
    mut stream: S,
    mut on_progress: F,
) -> Result<String, IndexProgressStreamError>
where
    S: tokio_stream::Stream<Item = Result<IndexProgress, tonic::Status>> + Unpin,
    F: FnMut(&IndexProgress),
{
    use tokio_stream::StreamExt;

    let mut tracker = IndexProgressTracker::default();
    while let Some(progress) = stream.next().await {
        let progress = progress.map_err(IndexProgressStreamError::Transport)?;
        tracker
            .observe(&progress)
            .map_err(IndexProgressStreamError::Protocol)?;
        on_progress(&progress);
    }

    tracker.finish().map_err(IndexProgressStreamError::Protocol)
}

#[derive(Debug)]
pub enum IndexProgressStreamError {
    Transport(tonic::Status),
    Protocol(IndexProgressError),
}

impl std::fmt::Display for IndexProgressStreamError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(status) => {
                write!(
                    formatter,
                    "index progress transport error: {}",
                    status.message()
                )
            }
            Self::Protocol(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for IndexProgressStreamError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(status) => Some(status),
            Self::Protocol(error) => Some(error),
        }
    }
}

#[cfg(test)]
mod index_progress_tracker_tests {
    use super::*;

    fn progress(phase: Phase, message: &str) -> IndexProgress {
        IndexProgress {
            phase: phase as i32,
            message: message.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn done_is_the_only_successful_terminal_state() {
        let mut tracker = IndexProgressTracker::default();
        tracker
            .observe(&progress(Phase::Discovering, "scanning"))
            .unwrap();
        tracker.observe(&progress(Phase::Done, "complete")).unwrap();

        assert_eq!(tracker.finish().unwrap(), "complete");
    }

    #[test]
    fn error_empty_and_truncated_streams_fail_closed() {
        let error = IndexProgressTracker::default().finish().unwrap_err();
        assert!(matches!(error, IndexProgressError::Empty));

        let mut truncated = IndexProgressTracker::default();
        truncated
            .observe(&progress(Phase::Writing, "not finished"))
            .unwrap();
        assert!(matches!(
            truncated.finish().unwrap_err(),
            IndexProgressError::Truncated { .. }
        ));

        let mut failed = IndexProgressTracker::default();
        failed
            .observe(&progress(Phase::Error, "parser exploded"))
            .unwrap();
        assert!(matches!(
            failed.finish().unwrap_err(),
            IndexProgressError::Reported { message } if message == "parser exploded"
        ));
    }

    /// nw-127: an in-band terminal error must reach the caller as a REPORTED
    /// error naming the cause, not as a truncated stream.
    ///
    /// The daemon's watchdog used to set its cancel flag and say nothing, so the
    /// client saw the stream simply end and rendered "index progress stream
    /// ended before completion" — indistinguishable from a crash, and shown as a
    /// failure for an index that was still running and went on to SUCCEED. The
    /// watchdog now emits a non-terminal warning (naming
    /// NESTWEAVER_INDEX_TIMEOUT_SECS) and lets the run's own terminal event
    /// report whether it aborted before writing or committed anyway; what
    /// remains pinned here is that any terminal `Phase::Error` still surfaces
    /// as an explanation rather than a truncation.
    #[test]
    fn a_timeout_reported_in_band_beats_a_truncated_stream() {
        // A terminal Error event reported in-band.
        let mut reported = IndexProgressTracker::default();
        reported
            .observe(&progress(Phase::Writing, "still writing"))
            .unwrap();
        reported
            .observe(&progress(
                Phase::Error,
                "index exceeded the 1800s timeout and cancellation was requested",
            ))
            .unwrap();
        let err = reported.finish().unwrap_err();
        match err {
            IndexProgressError::Reported { message } => {
                assert!(message.contains("timeout"), "{message}");
            }
            other => panic!("expected a reported error naming the timeout, got {other:?}"),
        }

        // What it did before: the same run, with the terminal event missing.
        let mut silent = IndexProgressTracker::default();
        silent
            .observe(&progress(Phase::Writing, "still writing"))
            .unwrap();
        assert!(
            matches!(
                silent.finish().unwrap_err(),
                IndexProgressError::Truncated { .. }
            ),
            "without the terminal event the caller can only report a truncated stream"
        );
    }

    #[test]
    fn any_event_after_a_terminal_event_is_rejected() {
        for terminal in [Phase::Done, Phase::Error] {
            let mut tracker = IndexProgressTracker::default();
            tracker.observe(&progress(terminal, "terminal")).unwrap();

            assert!(matches!(
                tracker.observe(&progress(Phase::Writing, "late")),
                Err(IndexProgressError::AfterTerminal { .. })
            ));
        }
    }
}
