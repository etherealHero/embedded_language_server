use embedded_lsp_proxy::{Config, run_service};

fn init_registry(debug_level_in_release: bool) {
    use tracing::level_filters::LevelFilter;
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

    let f = |m: &tracing::Metadata<'_>| m.name() != "service_ready";
    let layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .with_line_number(cfg!(debug_assertions) | debug_level_in_release)
        .with_file(cfg!(debug_assertions));

    tracing_subscriber::registry()
        .with(cfg_select! {
            debug_assertions => LevelFilter::DEBUG,
            _ => if debug_level_in_release { LevelFilter::DEBUG } else { LevelFilter::INFO }
        })
        .with(tracing_subscriber::filter::filter_fn(f))
        .with(match debug_level_in_release {
            true => layer.with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE),
            false => layer,
        })
        .init();
}

#[derive(clap::Parser, Debug)]
#[command(long_about = None)]
struct Cli {
    /// Enable debug level logging
    #[arg(long, global = true)]
    debug: bool,

    /// VSCode provides this flag by default, we ignore it
    #[arg(long, hide = true, global = true)]
    stdio: bool,

    /// Subcommands to run specific modes
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(clap::Subcommand, Debug)]
enum Commands {
    /// Start language server service (stdio transport)
    Lsp {
        /// Path to config file
        file: String,
    },
    /// Sign config file
    Sign {
        /// Path to config file
        file: String,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    use clap::{CommandFactory, Parser};
    let cli = Cli::parse();

    init_registry(cli.debug);

    match cli.command {
        Some(Commands::Lsp { file }) => {
            tracing::debug!("debug level logging enabled"); // if cli.debug
            run_service(&file).await;
        }
        Some(Commands::Sign { file }) => {
            let _ = Config::sign(&file)
                .inspect(|_| println!("sign config successfull!"))
                .inspect_err(|e| eprintln!("sign config fail: {e}"));
        }
        None => {
            let mut cmd = Cli::command();
            tracing::error!("expect 'sign' or 'lsp' option (See --help)");
            cmd.print_help().unwrap();
            std::process::exit(0);
        }
    };
}
