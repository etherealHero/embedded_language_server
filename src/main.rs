use async_lsp::LanguageClient;
use async_lsp::lsp_types::{self as lsp, notification as N, request as R};
use std::{ops::ControlFlow as F, sync::Arc};
use tracing::{debug, error, info, warn};

type Req<T> = Result<<T as R::Request>::Result, async_lsp::ResponseError>;
type Pool = Arc<tokio::sync::Mutex<Option<deadpool_tiberius::Pool>>>;
type _Notify = F<Result<(), async_lsp::Error>>;

#[derive(serde::Deserialize, Debug, Default)]
struct Config {
    host: String,
    database: String,
    get_symbols_query: String,
}

#[derive(Debug)]
struct SymbolInfo {
    hover: Option<String>,
}

#[derive(Default)]
struct ServerState {
    pool: Pool,
    config: tokio::sync::RwLock<Config>,
    client: std::sync::OnceLock<async_lsp::ClientSocket>,
    symbols: dashmap::DashMap<String, SymbolInfo>,
    url_to_path: dashmap::DashMap<lsp::Url, std::path::PathBuf>,
    text_documents: dashmap::DashMap<std::path::PathBuf, ropey::Rope>,
}

struct Server {
    state: Arc<ServerState>,
}

impl ServerState {
    fn set_text_document(
        &self,
        url: lsp::Url,
        changes: &[lsp::TextDocumentContentChangeEvent],
    ) -> std::io::Result<()> {
        let path = self.url_to_path(url)?;
        if changes.len() == 1 && changes[0].range.is_none() {
            let text_document = ropey::Rope::from_str(changes[0].text.as_str());
            self.text_documents.insert(path, text_document);
        } else {
            let err = std::io::Error::from(std::io::ErrorKind::NotFound);
            let td = &mut self.text_documents.get_mut(&path).ok_or(err)?;
            for change in changes {
                let r = change.range.as_ref().unwrap();
                let start = td.line_to_char(r.start.line as usize) + r.start.character as usize;
                let end = td.line_to_char(r.end.line as usize) + r.end.character as usize;
                td.remove(start..end);
                td.insert(start, &change.text);
            }
        }
        Ok(())
    }

    fn get_text_document(&self, url: lsp::Url) -> std::io::Result<ropey::Rope> {
        self.text_documents
            .get(&self.url_to_path(url)?)
            .map(|text_document| text_document.clone())
            .ok_or(std::io::Error::from(std::io::ErrorKind::NotFound))
    }

    fn remove_text_document(&self, url: lsp::Url) -> std::io::Result<()> {
        self.text_documents.remove(&self.url_to_path(url)?);
        Ok(())
    }

    fn url_to_path(&self, url: lsp::Url) -> std::io::Result<std::path::PathBuf> {
        if let Some(p) = self.url_to_path.get(&url) {
            Ok(p.clone())
        } else {
            let path = url.to_file_path();
            let path = path.map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
            let path = dunce::canonicalize(dunce::simplified(&path))?;
            self.url_to_path.insert(url, path.clone());
            Ok(path)
        }
    }
}

impl Server {
    async fn query(
        &self,
        query: &str, // TODO: add timeout (with config option)
    ) -> deadpool_tiberius::SqlServerResult<Vec<deadpool_tiberius::tiberius::Row>> {
        use deadpool_tiberius::{SqlServerError as SSE, deadpool::managed::BuildError as BE};
        let lock_fail = |e| SSE::Io(std::io::Error::new(std::io::ErrorKind::ResourceBusy, e));
        let try_lock = self.state.pool.try_lock();
        let pool = try_lock.map_err(lock_fail)?.as_ref().cloned();
        let pool = pool.ok_or(SSE::PoolBuild(BE::NoRuntimeSpecified))?;
        let mut conn = pool.get().await?;
        Ok(conn.simple_query(query).await?.into_first_result().await?)
    }

    fn get_config(&self, p: &lsp::InitializeParams) -> std::io::Result<Config> {
        use std::io::{Error as E, ErrorKind as EK};

        let undefined_config_path_opt = "Initialization options must contains 'configPath' option"; // TODO:
        let options = p.initialization_options.as_ref();
        let config_path = options.and_then(|v| v["configPath"].as_str().map(String::from));
        let config_path = &config_path.ok_or(E::new(EK::NotFound, undefined_config_path_opt))?;
        let msg = concat!(
            "Resolve workspace folder fail. ",
            "Language Client does not provide any workspace folder options ",
            "(workspace_folders, root_path, root_uri). ",
            "You should open your text editor in some project. ",
            "If it's not helped, your editor does not support this feature ",
            "(https://microsoft.github.io/language-server-protocol/",
            "specifications/lsp/3.17/specification/#workspace_workspaceFolders)."
        );

        #[allow(deprecated)]
        let root = p
            .workspace_folders
            .as_ref()
            .and_then(|wf| wf.first().cloned())
            .and_then(|f| f.uri.to_file_path().ok())
            .or_else(|| p.root_path.as_ref().map(std::path::PathBuf::from))
            .or_else(|| p.root_uri.as_ref().and_then(|url| url.to_file_path().ok()))
            .inspect(|p| info!("Resolved Workspace: {}", p.display()))
            .ok_or(E::new(EK::NotFound, msg))?;

        info!("Resolved config path: {}", root.join(config_path).display());

        let path = root.join(config_path);
        let try_exists = path.try_exists()?;
        let try_exists = try_exists.eq(&true).then_some(1);

        try_exists.ok_or(E::new(EK::NotFound, "Config file not exists"))?;
        info!("Found config: {}", path.display());

        let raw_config = std::fs::read(path).map(|b| String::from_utf8_lossy(&b).into_owned())?;

        toml::from_str(&raw_config).map_err(|e| E::new(EK::InvalidData, e))
    }

    async fn startup(&self, p: &lsp::InitializeParams) -> std::io::Result<()> {
        let config = self.get_config(p)?;
        let new_pool = deadpool_tiberius::Manager::new()
            .host(&config.host)
            .database(&config.database)
            .authentication(deadpool_tiberius::tiberius::AuthMethod::Integrated)
            .trust_cert()
            .wait_timeout(std::time::Duration::from_secs(5))
            .create_pool()
            .inspect_err(|e| error!("Create pool error: {e}"))
            .map_err(std::io::Error::other)?;

        {
            let mut unit_pool = self.state.pool.lock().await;
            *unit_pool = Some(new_pool);
            info!("Create pool success: {}.{}", config.host, config.database)
        } // lock free

        let rows = self.query(&config.get_symbols_query).await;
        let result = rows.inspect_err(|e| error!("Query error: {e}"));
        let missing_column = "Column 'Identifier' must present in get_symbols_query";
        for row in result.unwrap_or_default() {
            let try_ident = row.try_get::<&str, &str>("Identifier");
            let ident = try_ident.map_err(std::io::Error::other)?;
            let ident = ident.ok_or_else(|| std::io::Error::other(missing_column))?;
            let try_hover = row.try_get::<&str, &str>("HoverInfo");
            let hover = try_hover.unwrap_or_default().map(String::from);
            let symbol = SymbolInfo { hover };
            self.state.symbols.insert(ident.into(), symbol);
        }

        let mut uninit_config = self.state.config.write().await;
        *uninit_config = config;

        Ok(())
    }
}

/// [`lsp`] implementation
impl Server {
    async fn completion(self, _: lsp::CompletionParams) -> Req<R::Completion> {
        use rayon::iter::*;
        type SymbolRef<'a> = dashmap::mapref::multiple::RefMulti<'a, String, SymbolInfo>;
        let map = |s: SymbolRef| lsp::CompletionItem {
            label: s.key().to_string(),
            documentation: Some(lsp::Documentation::MarkupContent(lsp::MarkupContent {
                kind: lsp::MarkupKind::Markdown,
                value: s.hover.clone().unwrap_or_default(),
            })),
            ..Default::default()
        };
        let completions = self.state.symbols.par_iter().map(map).collect();
        Ok(Some(lsp::CompletionResponse::Array(completions)))
    }

    async fn hover(self, p: lsp::HoverParams) -> Req<R::HoverRequest> {
        use async_lsp::{ErrorCode as E, ResponseError as R};

        let fail = |e| R::new(E::REQUEST_FAILED, e);
        let url = p.text_document_position_params.text_document.uri;
        let position = p.text_document_position_params.position;
        let text_document = self.state.get_text_document(url).map_err(fail)?;
        let (line_idx, offset) = (position.line as usize, position.character as usize);
        let line = text_document.get_line(line_idx).map(String::from);
        let line = line.ok_or(fail(std::io::Error::from(std::io::ErrorKind::InvalidData)))?;

        let ident = if offset > line.len() {
            None
        } else {
            let re = regex::Regex::new(r"[\p{L}\p{N}_$]+").unwrap();
            re.find_iter(&line).find_map(|m| {
                let whole_ident = m.range().contains(&offset);
                let last_char_ident = m.range().contains(&offset.saturating_sub(1));
                (whole_ident | last_char_ident).then_some(m.as_str().to_string())
            })
        };

        let hover = ident
            .as_ref()
            .and_then(|identifier| self.state.symbols.get(identifier))
            .and_then(|symbol| symbol.hover.clone())
            .map(|value| lsp::Hover {
                contents: lsp::HoverContents::Markup(lsp::MarkupContent {
                    kind: lsp::MarkupKind::Markdown,
                    value,
                }),
                range: None,
            });

        Ok(hover)
    }

    async fn initialize(self, p: lsp::InitializeParams) -> Req<R::Initialize> {
        info!("{} v{}", clap::crate_name!(), clap::crate_version!());
        debug!("initialization_options `{:#?}`", p.initialization_options);

        if let Err(e) = self.startup(&p).await {
            error!("Startup error: {e}");
            let message = "Startup error occured, see output for more details".to_string();
            let typ = lsp::MessageType::WARNING;
            let client_socket = self.state.client.get().cloned();
            client_socket.map(|mut c| c.show_message(lsp::ShowMessageParams { typ, message }));
        } else {
            info!("Startup success");
        };

        Ok(lsp::InitializeResult {
            capabilities: lsp::ServerCapabilities {
                text_document_sync: Some(lsp::TextDocumentSyncCapability::Kind(
                    lsp::TextDocumentSyncKind::INCREMENTAL,
                )),
                hover_provider: Some(lsp::HoverProviderCapability::Simple(true)),
                definition_provider: Some(lsp::OneOf::Left(true)),
                completion_provider: Some(lsp::CompletionOptions::default()),
                ..lsp::ServerCapabilities::default()
            },
            server_info: Some(async_lsp::lsp_types::ServerInfo {
                name: clap::crate_name!().to_string(),
                version: Some(clap::crate_version!().to_string()),
            }),
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let (server, _) = async_lsp::MainLoop::new_server(|client| {
        let mut router = async_lsp::router::Router::new(Arc::new(ServerState {
            client: client.clone().into(),
            ..ServerState::default()
        }));

        fn add_request<T: R::Request, Fut: Future<Output = Req<T>> + Send + 'static, H>(
            router: &mut async_lsp::router::Router<Arc<ServerState>, async_lsp::ResponseError>,
            handler: H,
        ) -> &mut async_lsp::router::Router<Arc<ServerState>, async_lsp::ResponseError>
        where
            H: Fn(Server, T::Params) -> Fut + Send + Sync + 'static,
        {
            router.request::<T, _>(move |st: &mut Arc<ServerState>, p| {
                let state = Arc::clone(st);
                handler(Server { state }, p)
            })
        }

        add_request::<R::Completion, _, _>(&mut router, Server::completion);
        add_request::<R::HoverRequest, _, _>(&mut router, Server::hover);
        add_request::<R::Initialize, _, _>(&mut router, Server::initialize);

        router
            .notification::<N::Initialized>(|_, _| F::Continue(()))
            .notification::<N::DidChangeConfiguration>(|_, _| F::Continue(()))
            .notification::<N::DidOpenTextDocument>(|st, p| {
                let event = lsp::TextDocumentContentChangeEvent {
                    text: p.text_document.text,
                    range_length: None,
                    range: None,
                };
                st.set_text_document(p.text_document.uri, &[event])
                    .inspect_err(|e| error!("Did open text document error: {e}"))
                    .map_or_else(|e| F::Break(Err(e.into())), |_| F::Continue(()))
            })
            .notification::<N::DidChangeTextDocument>(|st, p| {
                st.set_text_document(p.text_document.uri, &p.content_changes)
                    .inspect_err(|e| error!("did change text document error: {e}"))
                    .map_or_else(|e| F::Break(Err(e.into())), |_| F::Continue(()))
            })
            .notification::<N::DidCloseTextDocument>(|st, p| {
                st.remove_text_document(p.text_document.uri)
                    .inspect_err(|e| error!("did close text document error: {e}"))
                    .map_or_else(|e| F::Break(Err(e.into())), |_| F::Continue(()))
            })
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
        .with_line_number(cfg!(debug_assertions))
        .with_file(cfg!(debug_assertions))
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
