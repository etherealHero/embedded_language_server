use async_lsp::lsp_types::{self as lsp, notification as N, request as R};
use tracing::{debug, error, info, warn};

type Fut<T> = futures::future::BoxFuture<
    'static,
    Result<<T as R::Request>::Result, async_lsp::ResponseError>,
>;

type Pool = std::sync::Arc<tokio::sync::Mutex<Option<deadpool_tiberius::Pool>>>;

struct ServerState {
    pool: Pool,
    _client: async_lsp::ClientSocket,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let (server, _) = async_lsp::MainLoop::new_server(|client| {
        let mut router = async_lsp::router::Router::new(ServerState {
            pool: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            _client: client.clone(),
        });
        router
            .request::<R::Initialize, _>(initialize)
            .request::<R::HoverRequest, _>(hover)
            .notification::<N::Initialized>(|_, _| std::ops::ControlFlow::Continue(()))
            .notification::<N::DidChangeConfiguration>(|_, _| std::ops::ControlFlow::Continue(()))
            .notification::<N::DidOpenTextDocument>(|_, _| std::ops::ControlFlow::Continue(()))
            .notification::<N::DidChangeTextDocument>(|_, _| std::ops::ControlFlow::Continue(()))
            .notification::<N::DidCloseTextDocument>(|_, _| std::ops::ControlFlow::Continue(()))
            .unhandled_notification(|_, notify| {
                warn!("unhandled_notification `{}`", notify.method);
                std::ops::ControlFlow::Continue(())
            })
            .unhandled_request(|_, req| async move {
                warn!("unhandled_request `{}`", req.method);
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
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .with_max_level(cfg_select! {
            debug_assertions => tracing::Level::DEBUG,
            _ => tracing::Level::INFO
        })
        .init();

    let (stdin, stdout) = cfg_select! {
        unix => (
            async_lsp::stdio::PipeStdin::lock_tokio().unwrap(),
            async_lsp::stdio::PipeStdout::lock_tokio().unwrap(),
        ),
        _ => (
            tokio_util::compat::TokioAsyncReadCompatExt::compat(tokio::io::stdin()),
            tokio_util::compat::TokioAsyncWriteCompatExt::compat_write(tokio::io::stdout()),
        )
    };

    server.run_buffered(stdin, stdout).await.unwrap();
}

async fn query(pool: Pool) -> deadpool_tiberius::SqlServerResult<String> {
    let get_pool = pool.lock().await;
    let Some(pool) = get_pool.as_ref() else {
        return Err(deadpool_tiberius::SqlServerError::PoolBuild(
            deadpool_tiberius::deadpool::managed::BuildError::NoRuntimeSpecified,
        ));
    };
    let mut conn = pool.get().await?;
    let res = conn
        .simple_query("select 'lorem ipsum' as field0")
        .await?
        .into_row()
        .await?
        .unwrap();
    let res: Option<&str> = res.get("field0");
    Ok(res.unwrap().to_string())
}

fn hover(st: &mut ServerState, _: <R::HoverRequest as R::Request>::Params) -> Fut<R::HoverRequest> {
    let pool = st.pool.clone();
    Box::pin(async move {
        let res = query(pool)
            .await
            .inspect_err(|e| error!("query error: {e}"))
            .ok()
            .unwrap_or_default();

        Ok(Some(lsp::Hover {
            contents: lsp::HoverContents::Scalar(lsp::MarkedString::String(format!(
                "I am a hover text! {res}"
            ))),
            range: None,
        }))
    })
}

fn initialize(
    st: &mut ServerState,
    p: <R::Initialize as R::Request>::Params,
) -> Fut<R::Initialize> {
    info!("{} v{}", clap::crate_name!(), clap::crate_version!());

    let opt = p.initialization_options;
    let get = |key| opt.as_ref().and_then(|v| v[key].as_str().map(String::from));

    debug!("initialization_options `{:#?}`", opt);

    let create_pool = match (get("host"), get("database")) {
        (Some(ref h), Some(ref d)) => deadpool_tiberius::Manager::new()
            .host(h)
            .database(d)
            .authentication(deadpool_tiberius::tiberius::AuthMethod::Integrated)
            .trust_cert()
            .wait_timeout(std::time::Duration::from_secs(5))
            .create_pool()
            .inspect(|_| info!(r#"Create pool success with host("{h}") and database("{d}")"#))
            .inspect_err(|e| error!("Create pool error: {e}"))
            .ok()
            .map(|new_pool| (new_pool, st.pool.clone())),
        _ => {
            warn!("Initialization options expect host(String) and database(String) options");
            None
        }
    };

    let initialize_result = Ok(lsp::InitializeResult {
        capabilities: lsp::ServerCapabilities {
            hover_provider: Some(lsp::HoverProviderCapability::Simple(true)),
            definition_provider: Some(lsp::OneOf::Left(true)),
            ..lsp::ServerCapabilities::default()
        },
        server_info: Some(async_lsp::lsp_types::ServerInfo {
            name: clap::crate_name!().to_string(),
            version: Some(clap::crate_version!().to_string()),
        }),
    });

    Box::pin(async move {
        if let Some((new_pool, pool)) = create_pool {
            let mut pool = pool.lock().await;
            *pool = Some(new_pool);
        }

        initialize_result
    })
}
