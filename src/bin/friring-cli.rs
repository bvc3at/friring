//! Friring CLI binary — scriptable access to the same state exposed by the
//! MCP server and the TUI. Every subcommand works without the TUI running.

use clap::Parser;

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::WARN.into()),
        )
        .init();

    let cli = friring::cli::Cli::parse();

    // Dispatched here, before anything opens the database: `sandbox relay` runs
    // *inside* a sandbox, where ADR-29 keeps the database out on purpose.
    // Opening one from in there would either create a stray database inside the
    // boundary or fail and leave the sandbox with no egress at all.
    if let friring::cli::Command::Sandbox { action } = &cli.command {
        if let Err(e) = friring::cli::sandbox::run(action) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    }

    // Publish settings before Database::open (audit pruning reads retention).
    // Warnings go to the WARN-level stderr logger; `config validate` is the
    // loud path.
    let (settings, _) = friring::agent::settings_config::load_or_seed_with_warnings();
    friring::session::settings::init(settings);

    let db_path = match friring::paths::database_file() {
        Some(p) => p,
        None => {
            eprintln!("error: cannot resolve database path (is HOME set?)");
            std::process::exit(2);
        }
    };

    let db = match friring::storage::Database::open(&db_path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!(
                "error: failed to open database at {}: {e}",
                db_path.display()
            );
            std::process::exit(2);
        }
    };

    if let Err(e) = friring::cli::run(cli, &db) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
