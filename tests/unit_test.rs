use embedded_lsp_proxy::Config;

use async_lsp::LanguageServer;
use async_lsp::lsp_types::{self as lsp};
use tracing::debug;

const APP_PATH: &str = env!("CARGO_BIN_EXE_embedded_lsp_proxy");
const TMPDIR: &str = env!("CARGO_TARGET_TMPDIR");

type MainLoop = async_lsp::MainLoop<
    async_lsp::tracing::Tracing<
        async_lsp::panic::CatchUnwind<
            async_lsp::concurrency::Concurrency<async_lsp::router::Router<()>>,
        >,
    >,
>;

#[tokio::test(flavor = "current_thread")]
async fn show_help_when_non_options() {
    init_tracing();

    let mut cmd = assert_cmd::Command::cargo_bin(env!("CARGO_PKG_NAME")).unwrap();
    let assert = cmd.assert();
    let output = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    debug!("output: `{output}`");
    assert!(output.contains("Start language server service"));
    assert!(output.contains("Sign config file"));
    assert!(output.contains("Print help"));
    assert!(output.contains("Enable debug level logging"));
}

#[tokio::test(flavor = "current_thread")]
async fn config_is_unsigned() -> anyhow::Result<()> {
    init_tracing();

    let config_path = std::path::PathBuf::from(TMPDIR).join("unsigned.toml");
    let raw_config = include_str!("test_config.toml");
    let mut config: Config = toml::from_str(raw_config)?;

    config.get_symbols_query += "--brake sign";
    std::fs::write(&config_path, toml::to_string(&config)?)?;

    let (mut client, mainloop, app) = spawn(["lsp", config_path.to_str().unwrap()]);
    let mainloop_fut = tokio::spawn(async move {
        let (stdin, stdout) = (app.stdin.unwrap(), app.stdout.unwrap());
        mainloop.run_buffered(stdout, stdin).await.unwrap();
    });

    let initialize_with_unsigned_config = client.initialize(Default::default()).await;
    let error_msg = match initialize_with_unsigned_config {
        Err(async_lsp::Error::Response(async_lsp::ResponseError { message, .. })) => message,
        _ => unreachable!(),
    };

    debug!("initialize_with_unsigned_config: `{error_msg}`");
    assert!(error_msg.contains("you should sign config"));
    assert!(error_msg.contains("--help"));

    client.initialized(lsp::InitializedParams {})?;
    client.shutdown(()).await?;
    client.exit(())?;
    client.emit(())?;
    mainloop_fut.await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn config_is_reasigned() -> anyhow::Result<()> {
    init_tracing();

    let config_path = std::path::PathBuf::from(TMPDIR).join("reasigned.toml");
    let raw_config = include_str!("test_config.toml");
    let mut config: Config = toml::from_str(raw_config)?;

    config.get_symbols_query += "--brake sign";
    std::fs::write(&config_path, toml::to_string(&config)?)?;

    let (_, _, mut app) = spawn(["sign", config_path.to_str().unwrap()]);
    let sign_status = app.status().await.unwrap();

    let (mut client, mainloop, service) = spawn(["lsp", config_path.to_str().unwrap()]);
    let mainloop_fut = tokio::spawn(async move {
        let (stdin, stdout) = (service.stdin.unwrap(), service.stdout.unwrap());
        mainloop.run_buffered(stdout, stdin).await.unwrap();
    });

    let initialize_with_reasigned_config = client.initialize(Default::default()).await;

    assert!(matches!(sign_status.code(), Some(0)));
    assert!(initialize_with_reasigned_config.is_ok());

    client.initialized(lsp::InitializedParams {})?;
    client.shutdown(()).await?;
    client.exit(())?;
    client.emit(())?;
    mainloop_fut.await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn request_hover() -> anyhow::Result<()> {
    init_tracing();
    let root_dir = std::path::Path::new(TMPDIR).canonicalize()?;
    let config_path = root_dir.join("temp_config.toml");

    std::fs::write(&config_path, include_str!("test_config.toml"))?;

    let (mut client, mainloop, service) = spawn(["lsp", config_path.to_str().unwrap()]);
    let mainloop_fut = tokio::spawn(async move {
        let (stdin, stdout) = (service.stdin.unwrap(), service.stdout.unwrap());
        mainloop.run_buffered(stdout, stdin).await.unwrap();
    });

    client.initialize(Default::default()).await?;
    client.initialized(lsp::InitializedParams {})?;

    let file_uri = lsp::Url::from_file_path(root_dir.join("some_file.md")).unwrap();
    let text = "select top 5 * from vStoreWithDemographics (nolock)";
    let doc = lsp::TextDocumentItem::new(file_uri.clone(), "md".into(), 0, text.into());

    client.did_open(lsp::DidOpenTextDocumentParams { text_document: doc })?;

    let params = lsp::HoverParams {
        text_document_position_params: lsp::TextDocumentPositionParams::new(
            lsp::TextDocumentIdentifier::new(file_uri.clone()),
            lsp::Position::new(0, text.find("vStoreWithDemographics").unwrap() as _),
        ),
        work_done_progress_params: lsp::WorkDoneProgressParams::default(),
    };

    let hover = client.hover(params).await;

    debug!("hover result: {hover:?}");
    assert!(matches!(
        hover,
        Ok(Some(lsp::Hover { contents: lsp::HoverContents::Markup(lsp::MarkupContent {value, ..}), .. }))
            if value.contains("View: vStoreWithDemographics")
    ));

    client.shutdown(()).await?;
    client.exit(())?;
    client.emit(())?;
    mainloop_fut.await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn request_definition() -> anyhow::Result<()> {
    init_tracing();
    let root_dir = std::path::Path::new(TMPDIR).canonicalize()?;
    let config_path = root_dir.join("temp_config.toml");

    std::fs::write(&config_path, include_str!("test_config.toml"))?;

    let (mut client, mainloop, service) = spawn(["lsp", config_path.to_str().unwrap()]);
    let mainloop_fut = tokio::spawn(async move {
        let (stdin, stdout) = (service.stdin.unwrap(), service.stdout.unwrap());
        mainloop.run_buffered(stdout, stdin).await.unwrap();
    });

    client.initialize(Default::default()).await?;
    client.initialized(lsp::InitializedParams {})?;

    let file_uri = lsp::Url::from_file_path(root_dir.join("some_file.md")).unwrap();
    let text = "vSalesPerson";
    let doc = lsp::TextDocumentItem::new(file_uri.clone(), "md".into(), 0, text.into());

    client
        .did_open(lsp::DidOpenTextDocumentParams { text_document: doc })
        .unwrap();

    let params = lsp::GotoDefinitionParams {
        text_document_position_params: lsp::TextDocumentPositionParams::new(
            lsp::TextDocumentIdentifier::new(file_uri.clone()),
            lsp::Position::new(0, text.find("vSalesPerson").unwrap() as _),
        ),
        work_done_progress_params: lsp::WorkDoneProgressParams::default(),
        partial_result_params: lsp::PartialResultParams::default(),
    };

    let definition = client.definition(params).await;

    debug!("definition result: {definition:?}");
    assert!(matches!(
        definition,
        Ok(Some(lsp::GotoDefinitionResponse::Scalar(_)))
    ));

    let loc = match definition?.unwrap() {
        lsp::GotoDefinitionResponse::Scalar(loc) => loc,
        _ => unreachable!(),
    };

    assert!(matches!(
        std::fs::exists(loc.uri.to_file_path().unwrap()),
        Ok(true)
    ));

    assert!(matches!(
        std::fs::read_to_string(loc.uri.to_file_path().unwrap()),
        Ok(content) if content.contains("CREATE VIEW [Sales].[vSalesPerson]")
    ));

    client.shutdown(()).await?;
    client.exit(())?;
    client.emit(())?;
    mainloop_fut.await?;
    Ok(())
}

fn spawn(
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> (async_lsp::ServerSocket, MainLoop, async_process::Child) {
    let (mainloop, client) = async_lsp::MainLoop::new_client(|_server| {
        let mut router = async_lsp::router::Router::new(());
        router.event(|_, _: ()| std::ops::ControlFlow::Break(Ok(())));
        tower::ServiceBuilder::new()
            .layer(async_lsp::tracing::TracingLayer::default())
            .layer(async_lsp::panic::CatchUnwindLayer::default())
            .layer(async_lsp::concurrency::ConcurrencyLayer::default())
            .service(router)
    });

    let child = async_process::Command::new(APP_PATH)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(cfg_select! {
            debug_assertions => std::process::Stdio::inherit(),
            _ => std::process::Stdio::null()
        })
        .kill_on_drop(true)
        .spawn()
        .expect("run app");

    (client, mainloop, child)
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(cfg_select! {
            debug_assertions => tracing::Level::DEBUG,
            _ => tracing::Level::INFO
        })
        .without_time()
        .with_writer(std::io::stdout)
        .try_init();
}
