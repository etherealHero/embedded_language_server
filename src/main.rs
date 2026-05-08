use embedded_language_server::{Cli, Config, OneOf, init_registry, run_service};

#[derive(clap::Subcommand, Debug)]
enum Commands {
    /// Start language server service (stdio transport)
    Lsp {
        /// Path to config file
        file: std::path::PathBuf,
    },
    /// Sign config file
    Sign {
        /// Path to config file
        file: std::path::PathBuf,
    },
    /// Print sample config to stdout
    SampleConfig,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    use clap::{CommandFactory, Parser};
    let cli = Cli::parse();

    init_registry(cli.debug);

    match cli.command {
        Some(Commands::Lsp { file }) => {
            tracing::debug!("debug level logging enabled"); // if cli.debug
            let service = run_service(OneOf::Left(file)).await;
            let _ = service.inspect_err(|e| tracing::error!("service error: {e}"));
        }
        Some(Commands::Sign { file }) => {
            let sign = Config::sign(&file).inspect(|_| println!("sign config successfull!"));
            let _ = sign.inspect_err(|e| eprintln!("sign config fail: {e}"));
        }
        Some(Commands::SampleConfig) => {
            println!(include_str!("../tests/sample_config.toml"));
            std::process::exit(0);
        }
        None => {
            let mut cmd = Cli::<Commands>::command();
            tracing::error!("expect command (See --help)");
            cmd.print_help().unwrap();
            std::process::exit(0);
        }
    };
}
