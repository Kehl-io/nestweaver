#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod apple_hardware {
    use std::path::PathBuf;
    use std::process::Command;

    fn required_path(name: &str) -> PathBuf {
        let value = std::env::var_os(name)
            .unwrap_or_else(|| panic!("{name} must name an explicit smoke-test path"));
        let path = PathBuf::from(value);
        assert!(
            path.is_absolute(),
            "{name} must be absolute: {}",
            path.display()
        );
        path
    }

    #[test]
    #[ignore = "requires an isolated macOS runner before the scoped daemon starts"]
    fn cold_daemon_preconditions_are_clean() {
        let db_path = required_path("NESTWEAVER_METAL_SMOKE_DB");
        let output_dir = required_path("NESTWEAVER_METAL_SMOKE_OUTPUT_DIR");
        let instance_id = nestweaver_daemon::instance_id_from_db_path(&db_path);
        let runtime_dir = nestweaver_daemon::runtime_dir(&instance_id);
        let socket = nestweaver_daemon::socket_path(&instance_id);
        let pidfile = nestweaver_daemon::pidfile_path(&instance_id);
        let log = nestweaver_daemon::log_path(&instance_id);
        let plist = nestweaver_daemon::lifecycle::launchd_plist_path(&instance_id);
        let label = nestweaver_daemon::lifecycle::launchd_label(&instance_id);

        let processes = Command::new("pgrep")
            .args(["-x", "nestweaver"])
            .output()
            .expect("pgrep must be available on the macOS runner");
        assert_eq!(
            processes.status.code(),
            Some(1),
            "fresh runner already has a NestWeaver process:\n{}",
            String::from_utf8_lossy(&processes.stdout)
        );

        for (kind, path) in [
            ("target database", db_path.as_path()),
            ("runtime directory", runtime_dir.as_path()),
            ("daemon socket", socket.as_path()),
            ("PID file", pidfile.as_path()),
            ("daemon log", log.as_path()),
            ("launchd plist", plist.as_path()),
            ("prior output directory", output_dir.as_path()),
        ] {
            assert!(
                !path.exists(),
                "{kind} must not exist before the cold start: {}",
                path.display()
            );
        }

        let service = format!("gui/{}/{label}", unsafe { libc::getuid() });
        let launchd = Command::new("launchctl")
            .args(["print", &service])
            .output()
            .expect("launchctl must be available on the macOS runner");
        assert!(
            !launchd.status.success(),
            "launchd service must not exist before the cold start: {service}"
        );
    }

    #[cfg(feature = "metal")]
    #[test]
    #[ignore = "requires Apple Silicon, a Metal-enabled build, and an explicit populated cache"]
    fn metal_embedding_is_finite_normalized_and_uses_metal() {
        let cache_dir = required_path("NESTWEAVER_METAL_SMOKE_CACHE_DIR");
        let model_id = std::env::var("NESTWEAVER_METAL_SMOKE_MODEL_ID")
            .expect("NESTWEAVER_METAL_SMOKE_MODEL_ID must be explicit");
        let config = nestweaver_embed::EmbedConfig {
            model_id,
            cache_dir,
            external_endpoint: None,
            external_model: None,
        };

        let model = nestweaver_embed::EmbedModel::load_with_policy_and_artifact_mode(
            &config,
            nestweaver_embed::DevicePolicy::Metal,
            nestweaver_embed::ArtifactMode::CacheOnly,
        )
        .expect("cache-only Metal model must load");
        assert_eq!(
            model.device_kind(),
            Some(nestweaver_embed::DeviceKind::Metal)
        );

        let vector = model
            .embed_query("NestWeaver cold Metal smoke test")
            .expect("Metal inference must succeed");
        assert!(!vector.is_empty(), "embedding vector must not be empty");
        assert!(
            vector.iter().all(|value| value.is_finite()),
            "embedding vector must contain only finite values"
        );
        let norm = vector
            .iter()
            .map(|value| f64::from(*value).powi(2))
            .sum::<f64>()
            .sqrt();
        assert!(
            (norm - 1.0).abs() <= 1e-4,
            "embedding vector must be L2-normalized; norm={norm}"
        );
    }
}
