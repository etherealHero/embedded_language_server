use async_lsp::lsp_types::{self as lsp, notification as N, request as R};
use std::ops::ControlFlow as F;
use tracing::{debug, error, info, warn};

type Req<T> = futures::future::BoxFuture<
    'static,
    Result<<T as R::Request>::Result, async_lsp::ResponseError>,
>;

type Notify = F<Result<(), async_lsp::Error>>;

type Pool = std::sync::Arc<tokio::sync::Mutex<Option<deadpool_tiberius::Pool>>>;

#[derive(Default)]
struct ServerState {
    pool: Pool,
    text_documents: dashmap::DashMap<std::path::PathBuf, ropey::Rope>,
    url_to_path: dashmap::DashMap<lsp::Url, std::path::PathBuf>,
    _client: Option<async_lsp::ClientSocket>,
}

fn set_text_document(
    st: &mut ServerState,
    url: lsp::Url,
    changes: &[lsp::TextDocumentContentChangeEvent],
) -> std::io::Result<()> {
    let path = url_to_path(st, url)?;
    if changes.len() == 1 && changes[0].range.is_none() {
        let text_document = ropey::Rope::from_str(changes[0].text.as_str());
        st.text_documents.insert(path, text_document);
    } else {
        let err = std::io::Error::from(std::io::ErrorKind::NotFound);
        let mut text_document = st.text_documents.get_mut(&path).ok_or(err)?;
        for change in changes {
            let td = &mut text_document;
            let r = change.range.as_ref().unwrap();
            let text = change.text.as_str();
            let start = td.line_to_char(r.start.line as usize) + r.start.character as usize;
            let end = td.line_to_char(r.end.line as usize) + r.end.character as usize;
            td.remove(start..end);
            td.insert(start, text);
        }
    }
    Ok(())
}

fn get_text_document(st: &ServerState, url: lsp::Url) -> std::io::Result<ropey::Rope> {
    st.text_documents
        .get(&url_to_path(st, url)?)
        .map(|text_document| text_document.clone())
        .ok_or(std::io::Error::from(std::io::ErrorKind::NotFound))
}

fn remove_text_document(st: &ServerState, url: lsp::Url) -> std::io::Result<()> {
    st.text_documents.remove(&url_to_path(st, url)?);
    Ok(())
}

fn url_to_path(st: &ServerState, url: lsp::Url) -> std::io::Result<std::path::PathBuf> {
    if let Some(p) = st.url_to_path.get(&url) {
        Ok(p.clone())
    } else {
        let path = url.to_file_path();
        let path = path.map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
        let path = dunce::canonicalize(dunce::simplified(&path))?;
        st.url_to_path.insert(url, path.clone());
        Ok(path)
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let (server, _) = async_lsp::MainLoop::new_server(|client| {
        let mut router = async_lsp::router::Router::new(ServerState {
            pool: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            _client: client.clone().into(),
            ..ServerState::default()
        });
        router
            .request::<R::Initialize, _>(initialize)
            .request::<R::HoverRequest, _>(hover)
            .notification::<N::Initialized>(|_, _| F::Continue(()))
            .notification::<N::DidChangeConfiguration>(|_, _| F::Continue(()))
            .notification::<N::DidOpenTextDocument>(open)
            .notification::<N::DidChangeTextDocument>(change)
            .notification::<N::DidCloseTextDocument>(close)
            .unhandled_notification(|_, notify| {
                warn!("unhandled_notification `{}`", notify.method);
                F::Continue(())
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

fn close(st: &mut ServerState, p: lsp::DidCloseTextDocumentParams) -> Notify {
    remove_text_document(st, p.text_document.uri)
        .inspect_err(|e| error!("did close text document error: {e}"))
        .map_or_else(|e| F::Break(Err(e.into())), |_| F::Continue(()))
}

fn change(st: &mut ServerState, p: lsp::DidChangeTextDocumentParams) -> Notify {
    set_text_document(st, p.text_document.uri, &p.content_changes)
        .inspect_err(|e| error!("did change text document error: {e}"))
        .map_or_else(|e| F::Break(Err(e.into())), |_| F::Continue(()))
}

fn open(st: &mut ServerState, p: lsp::DidOpenTextDocumentParams) -> Notify {
    set_text_document(
        st,
        p.text_document.uri,
        &[lsp::TextDocumentContentChangeEvent {
            text: p.text_document.text,
            range_length: None,
            range: None,
        }],
    )
    .inspect_err(|e| error!("did open text document error: {e}"))
    .map_or_else(|e| F::Break(Err(e.into())), |_| F::Continue(()))
}

fn hover(st: &mut ServerState, p: lsp::HoverParams) -> Req<R::HoverRequest> {
    use async_lsp::{ErrorCode as E, ResponseError as R};

    let fail = |e| Box::pin(async move { Err(R::new(E::REQUEST_FAILED, e)) });
    let pool = st.pool.clone();
    let url = p.text_document_position_params.text_document.uri;
    let text_document = match get_text_document(st, url) {
        Ok(text_document) => text_document,
        Err(e) => return fail(e),
    };
    let line_idx = p.text_document_position_params.position.line as usize;
    let Some(line) = text_document.get_line(line_idx) else {
        return fail(std::io::Error::from(std::io::ErrorKind::InvalidData));
    };
    let line = line.to_string();

    Box::pin(async move {
        let res = query(pool)
            .await
            .inspect_err(|e| error!("query error: {e}"))
            .ok()
            .unwrap_or_default();

        Ok(Some(lsp::Hover {
            contents: lsp::HoverContents::Scalar(lsp::MarkedString::String(format!(
                "I am a hover text! Query `{res}`, current server state text document line: `\n\n{line}\n\n`"
            ))),
            range: None,
        }))
    })
}

fn initialize(st: &mut ServerState, p: lsp::InitializeParams) -> Req<R::Initialize> {
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
            text_document_sync: Some(lsp::TextDocumentSyncCapability::Kind(
                lsp::TextDocumentSyncKind::INCREMENTAL,
            )),
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
