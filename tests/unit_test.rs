use async_lsp::lsp_types::{self as lsp};
use async_lsp::{LanguageServer, ServerSocket};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use tracing::debug;

const APP_PATH: &str = env!("CARGO_BIN_EXE_lsp_proxy");
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
async fn not_found_workspace_folder() {
    init_tracing();
    let (mut server, mainloop, process) = spawn_server_process();
    let mainloop_fut = tokio::spawn(async move {
        let (stdin, stdout) = (process.stdin.unwrap(), process.stdout.unwrap());
        mainloop.run_buffered(stdout, stdin).await.unwrap();
    });

    let initialize_params = lsp::InitializeParams {
        initialization_options: Some(
            serde_json::from_value(serde_json::json!({ "configPath": "./temp_config.toml" }))
                .unwrap(),
        ),
        ..lsp::InitializeParams::default()
    };

    let initialize_with_missing_ws = server.initialize(initialize_params).await;

    debug!("initialize_with_missing_ws: `{initialize_with_missing_ws:#?}`");
    assert!(matches!(
        initialize_with_missing_ws,
        Err(async_lsp::Error::Response(async_lsp::ResponseError {message, ..}))
            if message.starts_with("Resolve workspace folder fail")
    ));

    server.initialized(lsp::InitializedParams {}).unwrap();
    server.shutdown(()).await.unwrap();
    server.exit(()).unwrap();
    server.emit(()).unwrap();
    mainloop_fut.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn not_found_config_option() {
    init_tracing();
    let (mut server, mainloop, process) = spawn_server_process();
    let mainloop_fut = tokio::spawn(async move {
        let (stdin, stdout) = (process.stdin.unwrap(), process.stdout.unwrap());
        mainloop.run_buffered(stdout, stdin).await.unwrap();
    });

    let root_dir = Path::new(TMPDIR).canonicalize().unwrap();
    let initialize_params = lsp::InitializeParams {
        workspace_folders: Some(vec![lsp::WorkspaceFolder {
            uri: lsp::Url::from_file_path(&root_dir).unwrap(),
            name: "root".into(),
        }]),
        ..lsp::InitializeParams::default()
    };

    let initialize_with_missing_config_opt = server.initialize(initialize_params).await;

    debug!("initialize_with_missing_config_opt: `{initialize_with_missing_config_opt:#?}`");
    assert!(matches!(
        initialize_with_missing_config_opt,
        Err(async_lsp::Error::Response(async_lsp::ResponseError {message, ..}))
            if message == "missing 'configPath' initialize option"
    ));

    server.initialized(lsp::InitializedParams {}).unwrap();
    server.shutdown(()).await.unwrap();
    server.exit(()).unwrap();
    server.emit(()).unwrap();
    mainloop_fut.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn not_found_config_file() {
    init_tracing();
    let (mut server, mainloop, process) = spawn_server_process();
    let mainloop_fut = tokio::spawn(async move {
        let (stdin, stdout) = (process.stdin.unwrap(), process.stdout.unwrap());
        mainloop.run_buffered(stdout, stdin).await.unwrap();
    });

    let root_dir = Path::new(TMPDIR).canonicalize().unwrap();
    let initialize_params = lsp::InitializeParams {
        workspace_folders: Some(vec![lsp::WorkspaceFolder {
            uri: lsp::Url::from_file_path(&root_dir).unwrap(),
            name: "root".into(),
        }]),
        initialization_options: Some(
            serde_json::from_value(serde_json::json!({ "configPath": "./not_exists_config.toml" }))
                .unwrap(),
        ),
        ..lsp::InitializeParams::default()
    };

    let initialize_with_undefinded_config = server.initialize(initialize_params).await;

    debug!("initialize_with_undefinded_config: `{initialize_with_undefinded_config:#?}`");
    assert!(matches!(
        initialize_with_undefinded_config,
        Err(async_lsp::Error::Response(async_lsp::ResponseError {message, ..}))
            if message == "config file not exists"
    ));

    server.initialized(lsp::InitializedParams {}).unwrap();
    server.shutdown(()).await.unwrap();
    server.exit(()).unwrap();
    server.emit(()).unwrap();
    mainloop_fut.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn config_is_unsigned() {
    init_tracing();
    let (mut server, mainloop, process) = spawn_server_process();
    let mainloop_fut = tokio::spawn(async move {
        let (stdin, stdout) = (process.stdin.unwrap(), process.stdout.unwrap());
        mainloop.run_buffered(stdout, stdin).await.unwrap();
    });

    let root_dir = Path::new(TMPDIR).canonicalize().unwrap();
    let initialize_params = lsp::InitializeParams {
        workspace_folders: Some(vec![lsp::WorkspaceFolder {
            uri: lsp::Url::from_file_path(&root_dir).unwrap(),
            name: "root".into(),
        }]),
        initialization_options: Some(
            serde_json::from_value(
                serde_json::json!({ "configPath": "./unsigned_temp_config.toml" }),
            )
            .unwrap(),
        ),
        ..lsp::InitializeParams::default()
    };

    let config_path = PathBuf::from(TMPDIR).join("unsigned_temp_config.toml");
    let contents = include_str!("test_config.toml");
    let mut config: lsp_proxy::Config = toml::from_str(contents).unwrap();
    config.get_symbols_query += "--"; // brake sign
    let contents = toml::to_string(&config).unwrap();

    std::fs::write(&config_path, contents).unwrap();

    let initialize_with_unsigned_config = server.initialize(initialize_params.clone()).await;
    let error_msg = match initialize_with_unsigned_config {
        Err(async_lsp::Error::Response(async_lsp::ResponseError { message, .. })) => message,
        _ => unreachable!(),
    };

    debug!("initialize_with_unsigned_config: `{error_msg}`");
    assert!(error_msg.contains("you should sign config"));
    assert!(error_msg.contains("--help"));

    server.initialized(lsp::InitializedParams {}).unwrap();
    server.shutdown(()).await.unwrap();
    server.exit(()).unwrap();
    server.emit(()).unwrap();
    mainloop_fut.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn config_is_reasigned() {
    init_tracing();
    let (mut server, mainloop, process) = spawn_server_process();
    let mainloop_fut = tokio::spawn(async move {
        let (stdin, stdout) = (process.stdin.unwrap(), process.stdout.unwrap());
        mainloop.run_buffered(stdout, stdin).await.unwrap();
    });

    let root_dir = Path::new(TMPDIR).canonicalize().unwrap();
    let initialize_params = lsp::InitializeParams {
        workspace_folders: Some(vec![lsp::WorkspaceFolder {
            uri: lsp::Url::from_file_path(&root_dir).unwrap(),
            name: "root".into(),
        }]),
        initialization_options: Some(
            serde_json::from_value(serde_json::json!({
                "configPath": "./reasigned_temp_config.toml"
            }))
            .unwrap(),
        ),
        ..lsp::InitializeParams::default()
    };

    let config_path = PathBuf::from(TMPDIR).join("reasigned_temp_config.toml");
    let contents = include_str!("test_config.toml");
    let mut config: lsp_proxy::Config = toml::from_str(contents).unwrap();
    config.get_symbols_query += "--"; // brake sign
    let contents = toml::to_string(&config).unwrap();

    std::fs::write(&config_path, contents).unwrap();

    let mut child = async_process::Command::new(APP_PATH)
        .args(["sign", config_path.to_str().unwrap()])
        // .current_dir(&root_dir)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(cfg_select! {
            debug_assertions => std::process::Stdio::inherit(),
            _ => std::process::Stdio::null()
        })
        .kill_on_drop(true)
        .spawn()
        .expect("Failed run app");

    let status = child.status().await.unwrap();
    let initialize_with_reasigned_config = server.initialize(initialize_params.clone()).await;

    assert!(matches!(status.code(), Some(0)));
    assert!(initialize_with_reasigned_config.is_ok());

    server.initialized(lsp::InitializedParams {}).unwrap();
    server.shutdown(()).await.unwrap();
    server.exit(()).unwrap();
    server.emit(()).unwrap();
    mainloop_fut.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn request_hover() {
    run_server_with(async |server: &mut ServerSocket, workspace: PathBuf| {
        let file_uri = lsp::Url::from_file_path(workspace.join("some_file.md")).unwrap();
        let text = "select top 5 * from vStoreWithDemographics (nolock)";
        let doc = lsp::TextDocumentItem::new(file_uri.clone(), "md".into(), 0, text.into());

        server
            .did_open(lsp::DidOpenTextDocumentParams { text_document: doc })
            .unwrap();

        let params = lsp::HoverParams {
            text_document_position_params: lsp::TextDocumentPositionParams::new(
                lsp::TextDocumentIdentifier::new(file_uri.clone()),
                lsp::Position::new(0, text.find("vStoreWithDemographics").unwrap() as _),
            ),
            work_done_progress_params: lsp::WorkDoneProgressParams::default(),
        };

        let hover = server.hover(params).await;

        debug!("hover result: {hover:?}");
        assert!(matches!(
            hover,
            Ok(Some(lsp::Hover { contents: lsp::HoverContents::Markup(lsp::MarkupContent {value, ..}), .. }))
                if value.contains("View: vStoreWithDemographics")
        ));
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn request_definition() {
    run_server_with(async |server: &mut ServerSocket, workspace: PathBuf| {
        let file_uri = lsp::Url::from_file_path(workspace.join("some_file.md")).unwrap();
        let text = "vSalesPerson";
        let doc = lsp::TextDocumentItem::new(file_uri.clone(), "md".into(), 0, text.into());

        server
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

        let definition = server.definition(params).await;

        debug!("definition result: {definition:?}");
        assert!(matches!(
            definition,
            Ok(Some(lsp::GotoDefinitionResponse::Scalar(_)))
        ));

        let loc = match definition.unwrap().unwrap() {
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
        ))
    })
    .await;
}

async fn run_server_with(test_case: impl AsyncFn(&mut ServerSocket, PathBuf)) {
    init_tracing();
    let (mut server, mainloop, process) = spawn_server_process();
    let mainloop_fut = tokio::spawn(async move {
        let (stdin, stdout) = (process.stdin.unwrap(), process.stdout.unwrap());
        mainloop.run_buffered(stdout, stdin).await.unwrap();
    });

    let root_dir = Path::new(TMPDIR).canonicalize().unwrap();
    let initialize_params = lsp::InitializeParams {
        workspace_folders: Some(vec![lsp::WorkspaceFolder {
            uri: lsp::Url::from_file_path(&root_dir).unwrap(),
            name: "root".into(),
        }]),
        initialization_options: Some(
            serde_json::from_value(serde_json::json!({
                "configPath": "./temp_config.toml"
            }))
            .unwrap(),
        ),
        ..lsp::InitializeParams::default()
    };

    let path = PathBuf::from(TMPDIR).join("temp_config.toml");
    let contents = include_str!("test_config.toml");
    std::fs::write(path, contents).unwrap();

    server.initialize(initialize_params).await.unwrap();
    server.initialized(lsp::InitializedParams {}).unwrap();

    test_case(&mut server, root_dir).await;

    server.shutdown(()).await.unwrap();
    server.exit(()).unwrap();
    server.emit(()).unwrap();
    mainloop_fut.await.unwrap();
}

fn spawn_server_process() -> (async_lsp::ServerSocket, MainLoop, async_process::Child) {
    let (mainloop, server) = async_lsp::MainLoop::new_client(|_server| {
        let mut router = async_lsp::router::Router::new(());
        router.event(|_, _: ()| ControlFlow::Break(Ok(())));
        tower::ServiceBuilder::new()
            .layer(async_lsp::tracing::TracingLayer::default())
            .layer(async_lsp::panic::CatchUnwindLayer::default())
            .layer(async_lsp::concurrency::ConcurrencyLayer::default())
            .service(router)
    });

    let child = async_process::Command::new(APP_PATH)
        .args(["lsp"])
        // .current_dir(&root_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(cfg_select! {
            debug_assertions => std::process::Stdio::inherit(),
            _ => std::process::Stdio::null()
        })
        .kill_on_drop(true)
        .spawn()
        .expect("Failed run service");

    (server, mainloop, child)
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
