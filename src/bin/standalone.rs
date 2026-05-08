use embedded_language_server::{Cli, OneOf, init_registry, run_service};

#[derive(clap::Subcommand, Debug)]
enum Commands {
    /// Start language server service (stdio transport)
    Lsp {
        /// Path to cache dir
        cache_dir: std::path::PathBuf,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    use clap::{CommandFactory, Parser};
    let cli = Cli::parse();

    init_registry(cli.debug);

    match cli.command {
        Some(Commands::Lsp { cache_dir }) => {
            tracing::debug!("debug level logging enabled"); // if cli.debug
            let service = run_service(OneOf::Right(cache_dir)).await;
            let _ = service.inspect_err(|e| tracing::error!("service error: {e}"));
        }
        None => {
            let mut cmd = Cli::<Commands>::command();
            tracing::error!("expect command (See --help)");
            cmd.print_help().unwrap();
            std::process::exit(0);
        }
    };
}
