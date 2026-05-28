//! Example binary: materialize projects from an instance config.
//!
//! Usage:
//!   cargo run --bin materialize-instance -- --config examples/nestweaver-instance.toml --db ./nestweaver.lbug

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "materialize-instance",
    about = "Materialize projects from an instance config into the graph"
)]
struct Args {
    /// Path to the instance config file (TOML)
    #[arg(long)]
    config: PathBuf,

    /// Path to the database file
    #[arg(long, default_value = "./nestweaver.lbug")]
    db: PathBuf,

    /// Instance ID override (defaults to the value in the config)
    #[arg(long)]
    instance: Option<String>,
}

fn main() -> anyhow::Result<()> {
    // Initialize tracing so diagnostic messages from the engine are visible.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

    let config = nestweaver_engine::InstanceConfig::from_file(&args.config)
        .with_context(|| format!("failed to load config from {}", args.config.display()))?;

    let instance_id = args.instance.as_deref().unwrap_or(&config.instance_id);

    let store = nestweaver_store::GraphStore::open(&args.db)
        .with_context(|| format!("failed to open database at {}", args.db.display()))?;

    let result = nestweaver_engine::materialize_projects(&store, &config, instance_id, &args.db)
        .context("materialize_projects")?;

    println!(
        "Materialized {} project(s): {} note edges, {} symbol edges, \
         {} component edges, {} wiki notes ingested, {} wiki fetch errors",
        result.projects_created,
        result.note_edges,
        result.symbol_edges,
        result.component_edges,
        result.wiki_notes_ingested,
        result.wiki_fetch_errors,
    );

    Ok(())
}
