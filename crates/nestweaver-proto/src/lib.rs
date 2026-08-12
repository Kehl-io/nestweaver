pub mod nestweaver_daemon_v1 {
    tonic::include_proto!("nestweaver.daemon.v1");
}

pub use nestweaver_daemon_v1::*;

#[cfg(test)]
mod additive_status_contract_tests {
    use super::*;
    use prost::Message;

    /// Encode a length-delimited (wire type 2) string field.
    fn string_field(tag: u32, value: &str) -> Vec<u8> {
        let mut out = Vec::new();
        prost::encoding::encode_varint(u64::from(tag << 3 | 2), &mut out);
        prost::encoding::encode_varint(value.len() as u64, &mut out);
        out.extend_from_slice(value.as_bytes());
        out
    }

    /// Encode a varint (wire type 0) field.
    fn varint_field(tag: u32, value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        prost::encoding::encode_varint(u64::from(tag << 3), &mut out);
        prost::encoding::encode_varint(value, &mut out);
        out
    }

    /// An `EmbeddingStatus` exactly as a pre-4.2 daemon put it on the wire:
    /// hand-built tags for fields 1-8 only, with nothing from 4.2 present.
    fn pre_4_2_embedding_bytes() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(string_field(1, "ready")); // state
        bytes.extend(string_field(2, "local")); // backend
        bytes.extend(string_field(3, "auto")); // requested_device
        bytes.extend(string_field(4, "cpu")); // selected_device
        bytes.extend(string_field(5, "old-model")); // model_id
        bytes.extend(varint_field(7, 1)); // metal_compiled
        bytes
    }

    /// A3: the additive-compat claim, tested rather than asserted.
    ///
    /// These are hand-built wire bytes carrying ONLY the fields an older
    /// daemon knew about — not a new struct with new fields left at their
    /// defaults, which would prove nothing. Decoding must succeed and leave
    /// every field added in 4.2 at its proto3 default, so an upgraded client
    /// talking to an old daemon reads "no pass running, nothing queued"
    /// instead of erroring or inventing progress.
    #[test]
    fn a_pre_4_2_daemons_bytes_decode_with_the_new_fields_at_their_defaults() {
        let old_embedding = pre_4_2_embedding_bytes();

        let decoded = EmbeddingStatus::decode(old_embedding.as_slice())
            .expect("a pre-4.2 EmbeddingStatus must still decode");
        assert_eq!(decoded.state, "ready");
        assert_eq!(decoded.model_id, "old-model");
        assert!(decoded.metal_compiled);
        assert!(
            !decoded.pass_active,
            "an old daemon must read as 'no pass running', never as a live embed"
        );
        assert_eq!(decoded.pass_processed, 0);
        assert_eq!(decoded.pass_total, 0);
        assert_eq!(decoded.pass_started_at, 0);
        assert_eq!(decoded.pass_scope, "");

        let mut old_status = Vec::new();
        old_status.extend(varint_field(2, 42)); // notes
        old_status.extend(varint_field(10, 1)); // indexing_active
        old_status.extend(varint_field(12, 3)); // queue_depth
        {
            // embedding_status = 13, a nested message.
            let mut nested = Vec::new();
            prost::encoding::encode_varint(u64::from(13u32 << 3 | 2), &mut nested);
            prost::encoding::encode_varint(old_embedding.len() as u64, &mut nested);
            nested.extend_from_slice(&old_embedding);
            old_status.extend(nested);
        }

        let decoded = BrainStatusResponse::decode(old_status.as_slice())
            .expect("a pre-4.2 BrainStatusResponse must still decode");
        assert_eq!(decoded.notes, 42);
        assert!(decoded.indexing_active);
        assert_eq!(decoded.queue_depth, 3, "the old meaning must be preserved");
        assert_eq!(
            decoded.write_queue_depth, 0,
            "an old daemon reports no blocked writers rather than erroring"
        );
        assert_eq!(decoded.write_holder, "");
        assert_eq!(decoded.write_holder_seconds, 0);
        assert!(!decoded.embedding_status.expect("nested status").pass_active);
    }

    /// The other direction: a 4.2 daemon's bytes must remain readable by a
    /// consumer that only knows the old fields. Prost skips unknown fields, so
    /// re-decoding after stripping is not expressible here; what IS testable
    /// is that setting the new fields never perturbs the old ones on the wire.
    #[test]
    fn new_progress_fields_do_not_disturb_the_pre_4_2_field_values() {
        let old_only = EmbeddingStatus {
            state: "ready".to_string(),
            model_id: "m".to_string(),
            ..Default::default()
        };
        let with_progress = EmbeddingStatus {
            pass_active: true,
            pass_processed: 41_230,
            pass_total: 88_131,
            pass_started_at: 1_700_000_000,
            pass_scope: "all".to_string(),
            ..old_only.clone()
        };
        let round_tripped = EmbeddingStatus::decode(with_progress.encode_to_vec().as_slice())
            .expect("decode round trip");
        assert_eq!(round_tripped.state, old_only.state);
        assert_eq!(round_tripped.model_id, old_only.model_id);
        assert_eq!(round_tripped.pass_total, 88_131);

        // Proto3 omits default-valued scalars, so an IDLE 4.2 daemon must put
        // exactly the pre-4.2 byte sequence on the wire — no new tags at all.
        // Compared against the hand-built old-daemon bytes, not against
        // another instance of this same struct: comparing a struct to itself
        // with the new fields set to the values it already holds cannot fail
        // and proves nothing.
        let idle_4_2 = EmbeddingStatus {
            state: "ready".to_string(),
            backend: "local".to_string(),
            requested_device: "auto".to_string(),
            selected_device: "cpu".to_string(),
            model_id: "old-model".to_string(),
            metal_compiled: true,
            ..Default::default()
        };
        assert_eq!(
            idle_4_2.encode_to_vec(),
            pre_4_2_embedding_bytes(),
            "an idle 4.2 daemon must be byte-identical to a pre-4.2 one"
        );

        // And the converse, so the assertion above is known to be falsifiable:
        // a RUNNING pass does add bytes.
        assert_ne!(
            EmbeddingStatus {
                pass_active: true,
                ..idle_4_2.clone()
            }
            .encode_to_vec(),
            pre_4_2_embedding_bytes(),
        );
    }

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
            ..Default::default()
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

    #[test]
    fn effective_config_roundtrip_preserves_all_three_provenance_states() {
        use effective_config::Source;

        let configured = BrainStatusResponse {
            effective_config: Some(EffectiveConfig {
                source: Some(Source::ConfiguredPath("/tmp/instance.toml".to_string())),
            }),
            ..Default::default()
        };
        let configured =
            BrainStatusResponse::decode(configured.encode_to_vec().as_slice()).unwrap();
        assert!(matches!(
            configured.effective_config.unwrap().source,
            Some(Source::ConfiguredPath(path)) if path == "/tmp/instance.toml"
        ));

        let defaults = BrainStatusResponse {
            effective_config: Some(EffectiveConfig {
                source: Some(Source::CompiledDefaults(
                    effective_config::CompiledDefaults {},
                )),
            }),
            ..Default::default()
        };
        let defaults = BrainStatusResponse::decode(defaults.encode_to_vec().as_slice()).unwrap();
        assert!(matches!(
            defaults.effective_config.unwrap().source,
            Some(Source::CompiledDefaults(_))
        ));

        let unknown = BrainStatusResponse::default();
        let unknown = BrainStatusResponse::decode(unknown.encode_to_vec().as_slice()).unwrap();
        assert!(unknown.effective_config.is_none());
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

    /// The watchdog's timeout warning is NON-TERMINAL (its phase defaults to
    /// DISCOVERING), so the run's own late Writing/Done events still land and
    /// the caller's outcome derives from the genuine terminal event. This is
    /// the sequence a cancelled-but-committed index produces; back when the
    /// watchdog emitted a terminal `Phase::Error`, the late Writing/Done
    /// events were rejected as AfterTerminal and an index that had in fact
    /// committed was misreported to the CLI as a failure.
    #[test]
    fn a_non_terminal_timeout_warning_lets_the_real_done_report_through() {
        let mut tracker = IndexProgressTracker::default();
        tracker
            .observe(&progress(
                Phase::Discovering,
                "index exceeded the 1800s timeout and cancellation was requested \
                 (raise NESTWEAVER_INDEX_TIMEOUT_SECS)",
            ))
            .unwrap();
        tracker
            .observe(&progress(
                Phase::Writing,
                "Indexed 239745 files, 3122546 symbols",
            ))
            .unwrap();
        tracker
            .observe(&progress(
                Phase::Done,
                "Done — 239745 files, 3122546 symbols, 4725058 edges. Cancellation was \
                 requested but the index had already passed its last cancellation point and \
                 COMMITTED anyway. To discard this run and re-index from scratch, \
                 run: nestweaver index --repo /repos/big --force",
            ))
            .unwrap();

        let message = tracker.finish().unwrap();
        assert!(
            message.contains("COMMITTED"),
            "a committed-after-cancellation run must say so, got: {message}"
        );
        assert!(
            message.contains("nestweaver index --repo /repos/big --force"),
            "the repair must be named, got: {message}"
        );
        assert!(
            message.contains("239745 files"),
            "the real counts must survive, got: {message}"
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
