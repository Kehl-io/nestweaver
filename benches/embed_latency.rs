fn main() {
    #[cfg(feature = "embed")]
    {
        use std::time::Instant;
        use nestweaver_embed::{EmbedConfig, EmbedModel};

        let config = EmbedConfig::default();
        eprintln!("Loading model: {}...", config.model_id);
        let load_start = Instant::now();
        let model = EmbedModel::load(&config).expect("Failed to load model");
        eprintln!("Model loaded in {:.1}ms (dim={})", load_start.elapsed().as_millis(), model.dimension());

        let queries = [
            "how does authentication work",
            "where is the upload pipeline",
            "rate limiting middleware",
            "database connection pool configuration",
            "error handling in the payment flow",
        ];

        // Warm up
        let _ = model.embed_query(queries[0]);

        let mut times = Vec::new();
        for q in &queries {
            let start = Instant::now();
            let _ = model.embed_query(q).expect("embed failed");
            times.push(start.elapsed());
        }

        let avg_ms = times.iter().map(|t| t.as_millis()).sum::<u128>() as f64 / times.len() as f64;
        let min_ms = times.iter().map(|t| t.as_millis()).min().unwrap();
        let max_ms = times.iter().map(|t| t.as_millis()).max().unwrap();

        eprintln!("\nQuery embedding latency ({} queries):", queries.len());
        eprintln!("  avg: {avg_ms:.1}ms");
        eprintln!("  min: {min_ms}ms");
        eprintln!("  max: {max_ms}ms");
        eprintln!("  target: <50ms Metal, <400ms CPU");
    }

    #[cfg(not(feature = "embed"))]
    {
        eprintln!("Benchmark requires --features embed");
    }
}
