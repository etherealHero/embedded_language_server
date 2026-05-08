use anyhow::{Context, Result, anyhow, bail, ensure};
use std::{ops::ControlFlow as F, path::PathBuf, sync::Arc};
use tracing::{debug, error, info, warn};

use async_lsp::lsp_types::{self as lsp, notification as N, request as R};
use deadpool_tiberius as dt;
use rayon::iter::*;

type Req<T> = std::result::Result<<T as R::Request>::Result, anyhow::Error>;
type Pool = Arc<tokio::sync::Mutex<Option<dt::Pool>>>;
type SymbolRef<'a> = dashmap::mapref::multiple::RefMulti<'a, String, SymbolInfo>;
type _Notify = F<std::result::Result<(), async_lsp::Error>>;

const CRATE_NAME: &str = clap::crate_name!();

static APP: once_cell::sync::Lazy<String> = once_cell::sync::Lazy::new(|| {
    let exe = std::env::current_exe().unwrap();
    exe.file_prefix().unwrap().to_str().unwrap().to_string()
});

static RE_IDENT: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| regex::Regex::new(r"[\p{L}\p{N}_$]+").unwrap());

pub static CONFIG_HELP: &str = concat!(
    "config TOML file should be like:\n",
    include_str!("../tests/sample_config.toml")
);

/// resolve absolute path of [`raw_path`] (supports file or directory path)
pub fn resolve_path(raw_path: &PathBuf) -> Result<std::path::PathBuf> {
    let absolute_exists = std::fs::exists(raw_path).is_ok_and(|e| e);
    let relative = std::env::current_dir().map(|cd| cd.join(raw_path));
    let path = absolute_exists.then_some(raw_path.into()).or(relative.ok());
    let path = path.context(format!("{raw_path:?} not found"))?;
    Ok(dunce::simplified(&path).to_path_buf())
}

#[derive(serde::Deserialize, serde::Serialize, Debug, Default, Clone)]
pub struct Config {
    pub ado_connection_string: String,
    pub trust_cert: Option<bool>,
    pub case_sensitive: Option<bool>,
    pub get_symbols_query: String,
    pub sign: String,
}

impl Config {
    pub fn parse(config_path: &std::path::PathBuf) -> Result<Self> {
        let raw_config = std::fs::read_to_string(config_path).context("config file read error")?;
        toml::from_str(&raw_config).context("config parse error. ".to_owned() + CONFIG_HELP)
    }

    pub fn sign_key(&self) -> String {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(dotenv_codegen::dotenv!("SECRET").as_bytes());
        hasher.update(self.get_symbols_query.as_bytes());
        hex::encode(hasher.finalize())
    }

    pub fn sign(raw_path: &PathBuf) -> Result<()> {
        let path = resolve_path(raw_path)?;
        let raw_config = std::fs::read_to_string(&path).context("config file read error")?;
        let config = Self::parse(&path)?;
        let signed_config = raw_config.replace(&config.sign, &config.sign_key());
        std::fs::write(path, signed_config).context("overwrite config file error")
    }
}

#[derive(Debug, Clone)]
struct SymbolInfo {
    hover: Option<String>,
    definition: Option<String>,
    definition_file_ext: Option<String>,
}

#[derive(Default)]
struct ServerState {
    pool: Pool,
    config: Config,
    cache_dir: PathBuf,
    symbols: dashmap::DashMap<String, SymbolInfo>,
    url_to_path: dashmap::DashMap<lsp::Url, PathBuf>,
    text_documents: dashmap::DashMap<PathBuf, ropey::Rope>,
    _client: Option<async_lsp::ClientSocket>,
}

struct Server {
    state: Arc<ServerState>,
}

impl ServerState {
    fn set_text_document(
        &self,
        url: lsp::Url,
        changes: &[lsp::TextDocumentContentChangeEvent],
    ) -> Result<()> {
        let path = self.url_to_path(url)?;
        if changes.len() == 1 && changes[0].range.is_none() {
            let text_document = ropey::Rope::from_str(&changes[0].text.replace("\r\n", "\n"));
            self.text_documents.insert(path, text_document);
        } else {
            let err = "text document not found";
            let td = &mut self.text_documents.get_mut(&path).context(err)?;
            for change in changes {
                let r = change.range.as_ref().unwrap();
                let start = td.line_to_char(r.start.line as usize) + r.start.character as usize;
                let end = td.line_to_char(r.end.line as usize) + r.end.character as usize;
                td.remove(start..end);
                td.insert(start, &change.text.replace("\r\n", "\n"));
            }
        }
        Ok(())
    }

    fn get_text_document(&self, url: lsp::Url) -> Result<ropey::Rope> {
        self.text_documents
            .get(&self.url_to_path(url)?)
            .map(|text_document| text_document.clone())
            .context("text document not found")
    }

    fn remove_text_document(&self, url: lsp::Url) -> Result<()> {
        self.text_documents.remove(&self.url_to_path(url)?);
        Ok(())
    }

    fn url_to_path(&self, url: lsp::Url) -> Result<PathBuf> {
        if let Some(p) = self.url_to_path.get(&url) {
            Ok(p.clone())
        } else {
            let path = url.to_file_path();
            let path = path.map_err(|_| anyhow!("url to file path fail"))?;
            let path = dunce::simplified(&path);
            let path = dunce::canonicalize(path).unwrap_or(path.to_path_buf());
            self.url_to_path.insert(url, path.clone());
            Ok(path)
        }
    }
}

impl Server {
    async fn query(&self, query: &str) -> Result<Vec<dt::tiberius::Row>> {
        let pool = self.state.pool.try_lock()?.as_ref().cloned();
        let pool = pool.context("connection pool not initialized")?;
        let pool_timeouts = dt::deadpool::managed::Timeouts {
            wait: Some(std::time::Duration::from_secs(10)),
            create: Some(std::time::Duration::from_secs(10)),
            recycle: Some(std::time::Duration::from_secs(10)),
        };

        info!("execute query...");
        let mut conn = pool.timeout_get(&pool_timeouts).await?;
        let running_query = async { conn.simple_query(query).await?.into_first_result().await };
        let timeout_duration = std::time::Duration::from_secs(30);
        let Ok(result) = tokio::time::timeout(timeout_duration, running_query).await else {
            dt::deadpool::managed::Object::take(conn).close().await?;
            bail!("execute query timeout")
        };

        Ok(result.inspect(|r| info!("execute query success: {} rows selected", r.len()))?)
    }

    async fn startup(&self) -> Result<()> {
        let config = &self.state.config;

        ensure!(
            config.sign_key() == config.sign,
            "you should sign config (See: '{} --help')\n{CONFIG_HELP}",
            *APP
        );

        let manager = match config.trust_cert.is_some_and(|t| t) {
            true => dt::Manager::from_ado_string(&config.ado_connection_string)?.trust_cert(),
            false => dt::Manager::from_ado_string(&config.ado_connection_string)?,
        };

        let new_pool = manager
            .wait_timeout(std::time::Duration::from_secs(5))
            .create_pool()
            .inspect_err(|e| error!("create pool error: {e}"))?;

        {
            let mut unit_pool = self.state.pool.lock().await;
            *unit_pool = Some(new_pool);
            info!("create pool success");
        } // lock free

        let get_prop = |row: &dt::tiberius::Row, prop: &str| {
            let try_get_prop = row.try_get::<&str, &str>(prop);
            try_get_prop.unwrap_or_default().map(String::from)
        };

        for row in self.query(&config.get_symbols_query).await? {
            let ident = row.try_get::<&str, &str>("Identifier")?;
            let ident = ident.context("'Identifier' column must be string")?;
            let symbol = SymbolInfo {
                hover: get_prop(&row, "HoverInfo"),
                definition: get_prop(&row, "DefinitionInfo"),
                definition_file_ext: get_prop(&row, "DefinitionFileExtension"),
            };
            self.state.symbols.insert(ident.into(), symbol);
        }

        Ok(())
    }

    fn get_ident_on_text_document(
        &self,
        url: lsp::Url,
        p: lsp::Position,
    ) -> Result<Option<String>> {
        let text_document = self.state.get_text_document(url)?;
        let line = text_document.get_line(p.line as usize).map(String::from);
        Ok(self.get_ident_on_line(&line.context("line_idx is out of bounds")?, p))
    }

    fn get_ident_on_line(&self, line: &str, position: lsp::Position) -> Option<String> {
        let offset = position.character as usize;
        if offset > line.len() {
            None
        } else {
            RE_IDENT.find_iter(line).find_map(|m| {
                let whole_ident = m.range().contains(&offset);
                let last_char_ident = m.range().contains(&offset.saturating_sub(1));
                (whole_ident | last_char_ident).then_some(m.as_str().to_string())
            })
        }
    }

    fn get_symbol_on_text_document(
        &self,
        url: lsp::Url,
        position: lsp::Position,
    ) -> Result<Option<(String, SymbolInfo)>> {
        self.get_ident_on_text_document(url, position)?
            .map(|ident| Ok(self.get_symbol(&ident)))
            .unwrap_or(Ok(None))
    }

    fn get_symbol(&self, ident: &str) -> Option<(String, SymbolInfo)> {
        let trace = || debug!("symbol by ident `{ident}` not found");
        let symbol_pair = if self.state.config.case_sensitive.unwrap_or(true) {
            let symbol = self.state.symbols.get(ident);
            symbol.map(|s| (s.key().clone(), s.value().clone()))
        } else {
            let ident = ident.to_lowercase();
            self.state.symbols.par_iter().find_map_first(|s| {
                let matched = s.key().to_lowercase().eq(&ident);
                matched.then_some((s.key().clone(), s.value().clone()))
            })
        };
        symbol_pair.is_none().then(trace);
        symbol_pair
    }

    fn emit_symbol_definition(&self, symbol: (String, SymbolInfo)) -> Result<lsp::Url> {
        if !std::fs::exists(&self.state.cache_dir)? {
            std::fs::create_dir_all(&self.state.cache_dir)?
        };

        let symbol_info = &symbol.1;
        ensure!(symbol_info.definition_file_ext.is_some() & symbol_info.definition.is_some());

        let name = symbol.0.to_owned() + &symbol.1.definition_file_ext.unwrap();
        let file = self.state.cache_dir.join(name);

        std::fs::write(&file, symbol.1.definition.unwrap())?;
        Ok(lsp::Url::from_file_path(file).unwrap())
    }
}

/// [`lsp`] implementation
impl Server {
    async fn document_symbol(self, p: lsp::DocumentSymbolParams) -> Req<R::DocumentSymbolRequest> {
        let url = p.text_document.uri;
        let case_sensitive = self.state.config.case_sensitive.unwrap_or(true);
        let text_document = self.state.get_text_document(url.clone())?;
        let text_document_by_case_sensitive = match case_sensitive {
            true => text_document.to_string(),
            false => text_document.to_string().to_lowercase(),
        };

        let symbols = self
            .state
            .symbols
            .par_iter()
            .filter_map(|s| {
                let symbol = s.key();
                let symbol_len = u32::try_from(s.key().len()).ok()?;
                let symbol_by_case_sensitive = match case_sensitive {
                    true => symbol.to_string(),
                    false => symbol.to_lowercase(),
                };

                text_document_by_case_sensitive
                    .match_indices(&symbol_by_case_sensitive)
                    .filter_map(|(b, _)| {
                        let line = u32::try_from(text_document.try_byte_to_line(b).ok()?).ok()?;
                        let line_start_byte = text_document.try_line_to_byte(line as _).ok()?;
                        let offset = u32::try_from(b.checked_sub(line_start_byte)?).ok()?;
                        let start = lsp::Position::new(line, offset);
                        let end = lsp::Position::new(line, offset + symbol_len);
                        let ident = self.get_ident_on_text_document(url.clone(), start).ok()??;
                        let matched = ident.len().eq(&(symbol_len as usize));
                        matched.then_some(lsp::DocumentSymbol {
                            name: symbol.clone(),
                            range: lsp::Range::new(start, end),
                            selection_range: lsp::Range::new(start, end),
                            detail: None,
                            children: None,
                            kind: lsp::SymbolKind::VARIABLE,
                            tags: None,
                            #[allow(deprecated)]
                            deprecated: None,
                        })
                    })
                    .collect::<Vec<_>>()
                    .into()
            })
            .flatten()
            .collect::<Vec<_>>();

        Ok(Some(lsp::DocumentSymbolResponse::Nested(symbols)))
    }

    async fn references(self, p: lsp::ReferenceParams) -> Req<R::References> {
        let url = p.text_document_position.text_document.uri;
        let position = p.text_document_position.position;
        let Some((symbol_to_search, _)) = self.get_symbol_on_text_document(url, position)? else {
            return Ok(None);
        };

        let case_sensitive = self.state.config.case_sensitive.unwrap_or(true);
        let symbol_to_search_len = symbol_to_search.len() as u32;
        let symbol_to_search = &match case_sensitive {
            true => symbol_to_search,
            false => symbol_to_search.to_lowercase(),
        };

        let locations: Vec<_> = self
            .state
            .symbols
            .par_iter()
            .filter_map(|symbol| {
                let ext = symbol.definition_file_ext.clone()?;
                let filename = symbol.key().to_owned() + &ext;
                let uri = lsp::Url::from_file_path(self.state.cache_dir.join(filename)).ok()?;
                let definition_by_case_sensitive = match case_sensitive {
                    true => symbol.definition.clone()?,
                    false => symbol.definition.clone()?.to_lowercase(),
                };

                let locations: Vec<_> = definition_by_case_sensitive
                    .lines()
                    .enumerate()
                    .par_bridge()
                    .into_par_iter()
                    .filter_map(|(line_idx, line): (usize, &str)| {
                        let line_idx = u32::try_from(line_idx).ok()?;
                        let line_ranges: Vec<_> = line
                            .match_indices(symbol_to_search)
                            .map(|(offset, _)| lsp::Position::new(line_idx, offset as u32))
                            .filter_map(|start| {
                                let character = start.character + symbol_to_search_len;
                                let end = lsp::Position::new(line_idx, character);
                                self.get_ident_on_line(line, start)?
                                    .len()
                                    .eq(&(symbol_to_search_len as usize))
                                    .then_some(lsp::Range::new(start, end))
                            })
                            .collect();
                        Some(line_ranges)
                    })
                    .collect::<Vec<_>>()
                    .into_par_iter()
                    .flatten()
                    .map(|range| lsp::Location::new(uri.clone(), range))
                    .collect();

                if !locations.is_empty() {
                    let symbol = (symbol.key().to_string(), symbol.value().clone());
                    let msg = "Error on omit symbol reference";
                    let try_emit = self.emit_symbol_definition(symbol);
                    try_emit.inspect_err(|e| error!("{msg}: {e}")).ok()?;
                    Some(locations)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .into_par_iter()
            .flatten()
            .collect();

        Ok(Some(locations))
    }

    async fn ws_symbol_resolve(self, p: lsp::WorkspaceSymbol) -> Req<R::WorkspaceSymbolResolve> {
        let symbol = self.get_symbol(&p.name).context("Expect symbol resolve")?;
        let try_emit = self.emit_symbol_definition(symbol);
        try_emit.inspect_err(|e| error!("emit_symbol_definition error: {e}"))?;
        Ok(p)
    }

    async fn ws_symbol(self, _: lsp::WorkspaceSymbolParams) -> Req<R::WorkspaceSymbolRequest> {
        let map = |s: SymbolRef| {
            let ext = s.definition_file_ext.as_ref()?;
            let path = self.state.cache_dir.join(s.key().clone() + ext);
            let uri = lsp::Url::from_file_path(path).ok()?;
            Some(lsp::WorkspaceSymbol {
                location: lsp::OneOf::Right(lsp::WorkspaceLocation { uri }),
                kind: lsp::SymbolKind::VARIABLE,
                name: s.key().clone(),
                container_name: None,
                tags: None,
                data: None,
            })
        };
        let symbols = self.state.symbols.par_iter().filter_map(map).collect();
        Ok(Some(lsp::WorkspaceSymbolResponse::Nested(symbols)))
    }

    async fn definition(self, p: lsp::GotoDefinitionParams) -> Req<R::GotoDefinition> {
        let url = p.text_document_position_params.text_document.uri;
        let position = p.text_document_position_params.position;
        let Some((symbol, symbol_info)) = self.get_symbol_on_text_document(url, position)? else {
            return Ok(None);
        };
        ensure!(symbol_info.definition_file_ext.is_some() & symbol_info.definition.is_some());

        if symbol_info.definition.is_none() | symbol_info.definition_file_ext.is_none() {
            warn!("definition info of `{symbol}` symbol not found");
            return Ok(None);
        }
        let zero_range = lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(0, 0));
        let output_file_uri = self.emit_symbol_definition((symbol, symbol_info))?;
        let location = lsp::Location::new(output_file_uri, zero_range);

        Ok(Some(lsp::GotoDefinitionResponse::Scalar(location)))
    }

    async fn completion(self, _: lsp::CompletionParams) -> Req<R::Completion> {
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
        let url = p.text_document_position_params.text_document.uri;
        let position = p.text_document_position_params.position;
        let symbol = self.get_symbol_on_text_document(url, position)?;
        let hover = symbol.map(|(_, info)| lsp::Hover {
            contents: lsp::HoverContents::Markup(lsp::MarkupContent {
                kind: lsp::MarkupKind::Markdown,
                value: info.hover.unwrap_or_default(),
            }),
            range: None,
        });
        Ok(hover)
    }

    async fn initialize(self, p: lsp::InitializeParams) -> Req<R::Initialize> {
        info!("{CRATE_NAME} v{}", clap::crate_version!());
        debug!("initialization_options `{:#?}`", p.initialization_options);

        let try_startup = self.startup().await;
        try_startup.inspect_err(|e| error!("startup fail: {e}"))?;

        Ok(lsp::InitializeResult {
            capabilities: lsp::ServerCapabilities {
                text_document_sync: Some(lsp::TextDocumentSyncCapability::Kind(
                    lsp::TextDocumentSyncKind::INCREMENTAL,
                )),
                hover_provider: Some(lsp::HoverProviderCapability::Simple(true)),
                definition_provider: Some(lsp::OneOf::Left(true)),
                references_provider: Some(lsp::OneOf::Left(true)),
                completion_provider: Some(lsp::CompletionOptions::default()),
                document_symbol_provider: Some(lsp::OneOf::Right(lsp::DocumentSymbolOptions {
                    label: Some(APP.to_string()),
                    work_done_progress_options: lsp::WorkDoneProgressOptions::default(),
                })),
                workspace_symbol_provider: Some(lsp::OneOf::Right(lsp::WorkspaceSymbolOptions {
                    resolve_provider: Some(true),
                    work_done_progress_options: lsp::WorkDoneProgressOptions::default(),
                })),
                ..lsp::ServerCapabilities::default()
            },
            server_info: Some(lsp::ServerInfo {
                name: APP.to_string(),
                version: Some(clap::crate_version!().to_string()),
            }),
        })
    }

    async fn shutdown(self, _: ()) -> Req<R::Shutdown> {
        Ok(())
    }
}

impl Server {
    fn create(
        client: async_lsp::ClientSocket,
        config: Config,
        cache_dir: PathBuf,
    ) -> async_lsp::router::Router<Arc<ServerState>> {
        use std::io::Error as E;

        let mut router = async_lsp::router::Router::new(Arc::new(ServerState {
            config,
            cache_dir,
            _client: client.into(),
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
                use futures::TryFutureExt;
                let state = Arc::clone(st);
                let f = |e| async_lsp::ResponseError::new(async_lsp::ErrorCode::REQUEST_FAILED, e);
                handler(Server { state }, p).map_err(f)
            })
        }

        add_request::<R::DocumentSymbolRequest, _, _>(&mut router, Server::document_symbol);
        add_request::<R::References, _, _>(&mut router, Server::references);
        add_request::<R::WorkspaceSymbolResolve, _, _>(&mut router, Server::ws_symbol_resolve);
        add_request::<R::WorkspaceSymbolRequest, _, _>(&mut router, Server::ws_symbol);
        add_request::<R::GotoDefinition, _, _>(&mut router, Server::definition);
        add_request::<R::Completion, _, _>(&mut router, Server::completion);
        add_request::<R::HoverRequest, _, _>(&mut router, Server::hover);
        add_request::<R::Initialize, _, _>(&mut router, Server::initialize);
        add_request::<R::Shutdown, _, _>(&mut router, Server::shutdown);

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
                    .map_or_else(|e| F::Break(Err(E::other(e).into())), |_| F::Continue(()))
            })
            .notification::<N::DidChangeTextDocument>(|st, p| {
                st.set_text_document(p.text_document.uri, &p.content_changes)
                    .inspect_err(|e| error!("did change text document error: {e}"))
                    .map_or_else(|e| F::Break(Err(E::other(e).into())), |_| F::Continue(()))
            })
            .notification::<N::DidCloseTextDocument>(|st, p| {
                st.remove_text_document(p.text_document.uri)
                    .inspect_err(|e| error!("did close text document error: {e}"))
                    .map_or_else(|e| F::Break(Err(E::other(e).into())), |_| F::Continue(()))
            })
            .notification::<N::Exit>(|_, _| std::process::exit(0))
            .unhandled_notification(|_, notify| {
                debug!("unhandled_notification `{}`", notify.method);
                F::Continue(())
            })
            .unhandled_request(|_, req| async move {
                debug!("unhandled_request `{}`", req.method);
                Err(async_lsp::ResponseError::new(
                    async_lsp::ErrorCode::REQUEST_FAILED,
                    format!("unhandled request `{}`", req.method),
                ))
            });

        router
    }
}

pub fn init_registry(debug_level_in_release: bool) {
    use tracing::level_filters::LevelFilter;
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

    let f = |m: &tracing::Metadata<'_>| m.name() != "service_ready";
    let layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(cfg!(debug_assertions) | debug_level_in_release)
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
pub struct Cli<T: clap::Subcommand> {
    /// Enable debug level logging
    #[arg(long, global = true)]
    pub debug: bool,

    /// VSCode provides this flag by default, we ignore it
    #[arg(long, hide = true, global = true)]
    stdio: bool,

    /// Subcommands to run specific modes
    #[command(subcommand)]
    pub command: Option<T>,
}

type ConfigPath = PathBuf;
type CacheDir = PathBuf;
pub use lsp::OneOf;

pub async fn run_service(opt: lsp::OneOf<ConfigPath, CacheDir>) -> Result<()> {
    let (config, cache_dir) = match opt {
        lsp::OneOf::Left(config_path) => {
            let p = resolve_path(&config_path)?;
            let p = dunce::canonicalize(p).inspect(|p| info!("config: {}", p.display()))?;
            let cache_dir = p.parent().context("extract folder of config path fail")?;
            let cache_dir = cache_dir.join(format!("{CRATE_NAME}_output/{}/", *APP));
            let config = Config::parse(&p)?;
            (config, cache_dir)
        }
        lsp::OneOf::Right(cache_dir) => {
            let config: Config = toml::from_str(include_str!("../tests/sample_config.toml"))?;
            let cache_dir = resolve_path(&cache_dir)?;
            std::fs::create_dir_all(&cache_dir)?;
            (config, dunce::canonicalize(cache_dir)?)
        }
    };

    info!("start service...");

    let (server, _) = async_lsp::MainLoop::new_server(|client| {
        use async_lsp::client_monitor::ClientProcessMonitorLayer;
        tower::ServiceBuilder::new()
            .layer(async_lsp::tracing::TracingLayer::default())
            .layer(async_lsp::server::LifecycleLayer::default())
            .layer(async_lsp::panic::CatchUnwindLayer::default())
            .layer(async_lsp::concurrency::ConcurrencyLayer::default())
            .layer(ClientProcessMonitorLayer::new(client.clone()))
            .service(Server::create(client, config, cache_dir))
    });

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

    info!("service is running");
    let service = server.run_buffered(stdin, stdout);
    service.await.context("server process fail")
}
