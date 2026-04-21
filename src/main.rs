use async_lsp::lsp_types::{self as lsp, notification as N, request as R};

struct ServerState {
    _client: async_lsp::ClientSocket,
    counter: i32,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let (server, _) = async_lsp::MainLoop::new_server(|client| {
        let mut router = async_lsp::router::Router::new(ServerState {
            _client: client.clone(),
            counter: 0,
        });
        router
            .request::<R::Initialize, _>(|_, _| async move {
                tracing::info!("{} v{}", clap::crate_name!(), clap::crate_version!());
                Ok(lsp::InitializeResult {
                    capabilities: lsp::ServerCapabilities {
                        hover_provider: Some(lsp::HoverProviderCapability::Simple(true)),
                        definition_provider: Some(lsp::OneOf::Left(true)),
                        ..lsp::ServerCapabilities::default()
                    },
                    server_info: Some(async_lsp::lsp_types::ServerInfo {
                        name: clap::crate_name!().to_string(),
                        version: Some(clap::crate_version!().to_string()),
                    }),
                })
            })
            .request::<R::HoverRequest, _>(|st, _| {
                let counter = st.counter;
                async move {
                    Ok(Some(lsp::Hover {
                        contents: lsp::HoverContents::Scalar(lsp::MarkedString::String(format!(
                            "I am a hover text {counter}!",
                        ))),
                        range: None,
                    }))
                }
            })
            .notification::<N::Initialized>(|_, _| std::ops::ControlFlow::Continue(()))
            .notification::<N::DidChangeConfiguration>(|_, _| std::ops::ControlFlow::Continue(()))
            .notification::<N::DidOpenTextDocument>(|_, _| std::ops::ControlFlow::Continue(()))
            .notification::<N::DidChangeTextDocument>(|_, _| std::ops::ControlFlow::Continue(()))
            .notification::<N::DidCloseTextDocument>(|_, _| std::ops::ControlFlow::Continue(()))
            .unhandled_notification(|_, notify| {
                tracing::warn!("unhandled_notification `{}`", notify.method);
                std::ops::ControlFlow::Continue(())
            })
            .unhandled_request(|_, req| async move {
                tracing::warn!("unhandled_request `{}`", req.method);
                Err(async_lsp::ResponseError::new(
                    async_lsp::ErrorCode::REQUEST_FAILED,
                    format!("unhandled request `{}`", req.method),
                ))
            });

        tower::ServiceBuilder::new()
            .layer(async_lsp::tracing::TracingLayer::default())
            .layer(async_lsp::server::LifecycleLayer::default())
            .layer(async_lsp::panic::CatchUnwindLayer::default())
            .layer(async_lsp::concurrency::ConcurrencyLayer::default())
            .layer(async_lsp::client_monitor::ClientProcessMonitorLayer::new(
                client,
            ))
            .service(router)
    });

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();

    #[cfg(unix)] // Prefer truly asynchronous piped stdin/stdout without blocking tasks.
    let (stdin, stdout) = (
        async_lsp::stdio::PipeStdin::lock_tokio().unwrap(),
        async_lsp::stdio::PipeStdout::lock_tokio().unwrap(),
    );

    #[cfg(not(unix))] // Fallback to spawn blocking read/write otherwise.
    let (stdin, stdout) = (
        tokio_util::compat::TokioAsyncReadCompatExt::compat(tokio::io::stdin()),
        tokio_util::compat::TokioAsyncWriteCompatExt::compat_write(tokio::io::stdout()),
    );

    server.run_buffered(stdin, stdout).await.unwrap();
}
