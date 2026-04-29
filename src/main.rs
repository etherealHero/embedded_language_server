use anyhow::{Result, anyhow, bail, ensure};
use std::{ops::ControlFlow as F, path::PathBuf, sync::Arc};
use tracing::{debug, error, info, warn};

use async_lsp::LanguageClient;
use async_lsp::lsp_types::{self as lsp, notification as N, request as R};
use deadpool_tiberius as dt;
use rayon::iter::*;

type Req<T> = std::result::Result<<T as R::Request>::Result, anyhow::Error>;
type Pool = Arc<tokio::sync::Mutex<Option<dt::Pool>>>;
type SymbolRef<'a> = dashmap::mapref::multiple::RefMulti<'a, String, SymbolInfo>;
type _Notify = F<std::result::Result<(), async_lsp::Error>>;

static RE_IDENT: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| regex::Regex::new(r"[\p{L}\p{N}_$]+").unwrap());

#[derive(serde::Deserialize, Debug, Default, Clone)]
struct Config {
    host: String,
    database: String,
    get_symbols_query: String,
    case_sensitive: Option<bool>,
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
    config: tokio::sync::RwLock<Config>,
    config_path: std::sync::OnceLock<PathBuf>,
    client: std::sync::OnceLock<async_lsp::ClientSocket>,
    symbols: dashmap::DashMap<String, SymbolInfo>,
    url_to_path: dashmap::DashMap<lsp::Url, PathBuf>,
    text_documents: dashmap::DashMap<PathBuf, ropey::Rope>,
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
            let err = anyhow!("text document not found");
            let td = &mut self.text_documents.get_mut(&path).ok_or(err)?;
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
            .ok_or(anyhow!("text document not found"))
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
            let path = dunce::canonicalize(dunce::simplified(&path))?;
            self.url_to_path.insert(url, path.clone());
            Ok(path)
        }
    }
}

impl Server {
    async fn query(&self, query: &str) -> Result<Vec<dt::tiberius::Row>> {
        let pool = self.state.pool.try_lock()?.as_ref().cloned();
        let pool = pool.ok_or(anyhow!("connection pool not initialized"))?;
        let pool_timeouts = dt::deadpool::managed::Timeouts {
            wait: Some(std::time::Duration::from_secs(10)),
            create: Some(std::time::Duration::from_secs(10)),
            recycle: Some(std::time::Duration::from_secs(10)),
        };

        info!("execute query...");
        let mut conn = pool.timeout_get(&pool_timeouts).await?;
        let running_query = async { conn.simple_query(query).await?.into_first_result().await };
        let timeout_duration = std::time::Duration::from_secs(10);
        let Ok(result) = tokio::time::timeout(timeout_duration, running_query).await else {
            dt::deadpool::managed::Object::take(conn).close().await?;
            bail!("execute query timeout")
        };

        Ok(result?)
    }

    fn parse_config(&self, p: &lsp::InitializeParams) -> Result<(PathBuf, Config)> {
        let options = p.initialization_options.as_ref();
        let config_path = options.and_then(|v| v["configPath"].as_str().map(String::from));
        let config_path = &config_path.ok_or(anyhow!("missing 'configPath' initialize option"))?;
        let resolve_workspace_fail_message = concat!(
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
            .or_else(|| p.root_path.as_ref().map(PathBuf::from))
            .or_else(|| p.root_uri.as_ref().and_then(|url| url.to_file_path().ok()))
            .inspect(|p| info!("resolved Workspace: {}", p.display()))
            .ok_or(anyhow!(resolve_workspace_fail_message))?;

        info!("resolved config path: {}", root.join(config_path).display());

        let path = root.join(config_path);
        let try_exists = path.try_exists()?.eq(&true).then_some(1);

        try_exists.ok_or(anyhow!("config file not exists"))?;
        info!("found config: {}", path.display());

        let raw_config = std::fs::read(&path).map(|b| String::from_utf8_lossy(&b).into_owned())?;
        let config = toml::from_str(&raw_config).map_err(|e| anyhow!("parse config error: {e}"))?;

        Ok((path, config))
    }

    async fn startup(&self, p: &lsp::InitializeParams) -> Result<()> {
        let (config_path, config) = self.parse_config(p)?;
        let new_pool = dt::Manager::new()
            .host(&config.host)
            .database(&config.database)
            .authentication(dt::tiberius::AuthMethod::Integrated)
            .trust_cert()
            .wait_timeout(std::time::Duration::from_secs(5))
            .create_pool()
            .inspect_err(|e| error!("create pool error: {e}"))?;

        {
            let mut unit_pool = self.state.pool.lock().await;
            *unit_pool = Some(new_pool);
            info!("create pool success: {}.{}", config.host, config.database)
        } // lock free

        let get_prop = |row: &dt::tiberius::Row, prop: &str| {
            let try_get_prop = row.try_get::<&str, &str>(prop);
            try_get_prop.unwrap_or_default().map(String::from)
        };

        for row in self.query(&config.get_symbols_query).await? {
            let ident = row.try_get::<&str, &str>("Identifier")?;
            let ident = ident.ok_or_else(|| anyhow!("'Identifier' column must be string"))?;
            let symbol = SymbolInfo {
                hover: get_prop(&row, "HoverInfo"),
                definition: get_prop(&row, "DefinitionInfo"),
                definition_file_ext: get_prop(&row, "DefinitionFileExtension"),
            };
            self.state.symbols.insert(ident.into(), symbol);
        }

        let mut uninit_config = self.state.config.write().await;
        *uninit_config = config;
        ensure!(self.state.config_path.set(config_path).is_ok());

        Ok(())
    }

    fn get_ident_on_text_document(
        &self,
        url: lsp::Url,
        position: lsp::Position,
    ) -> Result<Option<String>> {
        let text_document = self.state.get_text_document(url)?;
        let line_idx = position.line as usize;
        let line = text_document.get_line(line_idx).map(String::from);
        let line = line.ok_or(anyhow!("line_idx is out of bounds"))?;
        Ok(self.get_ident_on_line(&line, position))
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
            .map(|ident| self.get_symbol(&ident))
            .unwrap_or(Ok(None))
    }

    fn get_symbol(&self, ident: &str) -> Result<Option<(String, SymbolInfo)>> {
        let symbol_pair = if self.state.config.try_read()?.case_sensitive.unwrap_or(true) {
            self.state
                .symbols
                .get(ident)
                .map(|s| (s.key().clone(), s.value().clone()))
        } else {
            let ident = ident.to_lowercase();
            self.state.symbols.par_iter().find_map_first(|s| {
                s.key()
                    .to_lowercase()
                    .eq(&ident)
                    .then_some((s.key().clone(), s.value().clone()))
            })
        };

        if symbol_pair.is_some() {
            Ok(symbol_pair)
        } else {
            warn!("symbol by ident `{ident}` not found");
            Ok(None)
        }
    }

    fn emit_symbol_definition(&self, symbol: (String, SymbolInfo)) -> Result<lsp::Url> {
        ensure!(self.state.config_path.get().is_some());

        let config = self.state.config.try_read()?;
        let path = format!("lsp_proxy_output/{}.{}", config.host, config.database);
        let config_path = self.state.config_path.get().unwrap();
        let output_folder = config_path.parent().unwrap().join(path);
        let symbol_info = &symbol.1;

        if !std::fs::exists(&output_folder)? {
            std::fs::create_dir_all(&output_folder)?
        };

        ensure!(symbol_info.definition_file_ext.is_some() & symbol_info.definition.is_some());

        let file = output_folder.join(symbol.0.to_owned() + &symbol.1.definition_file_ext.unwrap());

        std::fs::write(&file, symbol.1.definition.unwrap())?;
        Ok(lsp::Url::from_file_path(file).unwrap())
    }
}

/// [`lsp`] implementation
impl Server {
    async fn document_symbol(self, p: lsp::DocumentSymbolParams) -> Req<R::DocumentSymbolRequest> {
        let url = p.text_document.uri;
        let case_sensitive = self.state.config.try_read()?.case_sensitive.unwrap_or(true);
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
                    .filter_map(|(byte, _)| {
                        let line_idx = text_document.try_byte_to_line(byte).ok()?;
                        let line = text_document.get_line(line_idx)?.as_str()?;
                        let line_idx = u32::try_from(line_idx).ok()?;
                        let line_by_case_sensitive = match case_sensitive {
                            true => line.to_string(),
                            false => line.to_lowercase(),
                        };

                        let offset = line_by_case_sensitive
                            .match_indices(&symbol_by_case_sensitive)
                            .find_map(|(offset, _)| {
                                let offset = u32::try_from(offset).ok()?;
                                let position = lsp::Position::new(line_idx, offset);
                                self.get_ident_on_text_document(url.clone(), position)
                                    .ok()??
                                    .len()
                                    .eq(&(symbol_len as usize))
                                    .then_some(offset)
                            })?;

                        let start = lsp::Position::new(line_idx, offset);
                        let end = lsp::Position::new(line_idx, offset + symbol_len);

                        Some(lsp::DocumentSymbol {
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
        ensure!(self.state.config_path.get().is_some());

        let url = p.text_document_position.text_document.uri;
        let position = p.text_document_position.position;
        let Some((symbol_to_search, _)) = self.get_symbol_on_text_document(url, position)? else {
            return Ok(None);
        };

        let config = self.state.config.try_read()?;
        let output = format!("lsp_proxy_output/{}.{}", config.host, config.database);
        let config_path = self.state.config_path.get().unwrap();
        let output_folder = config_path.parent().unwrap().join(output);
        let case_sensitive = config.case_sensitive.unwrap_or(true);
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
                let uri = lsp::Url::from_file_path(output_folder.join(filename)).ok()?;
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
        let get_symbol = self.get_symbol(&p.name)?;
        let symbol = get_symbol.ok_or_else(|| anyhow!("Expect symbol resolve"))?;
        let try_emit = self.emit_symbol_definition(symbol);
        try_emit.inspect_err(|e| error!("emit_symbol_definition error: {e}"))?;
        Ok(p)
    }

    async fn ws_symbol(self, _: lsp::WorkspaceSymbolParams) -> Req<R::WorkspaceSymbolRequest> {
        ensure!(self.state.config_path.get().is_some());
        let config = self.state.config.try_read()?;
        let path = format!("lsp_proxy_output/{}.{}", config.host, config.database);
        let config_path = self.state.config_path.get().unwrap();
        let output_folder = &config_path.parent().unwrap().join(path);
        let map = |s: SymbolRef| lsp::WorkspaceSymbol {
            name: s.key().clone(),
            kind: lsp::SymbolKind::VARIABLE,
            tags: None,
            container_name: None,
            location: lsp::OneOf::Right(lsp::WorkspaceLocation {
                uri: lsp::Url::from_file_path(output_folder.join(
                    s.key().clone() + &s.definition_file_ext.clone().unwrap_or(".md".to_owned()),
                ))
                .unwrap(),
            }),
            data: None,
        };
        let symbols = self.state.symbols.par_iter().map(map).collect();
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
        info!("{} v{}", clap::crate_name!(), clap::crate_version!());

        debug!("initialization_options `{:#?}`", p.initialization_options);
        debug!(
            "token_types `{:#?}`",
            p.capabilities
                .text_document
                .as_ref()
                .and_then(|c| c.semantic_tokens.as_ref())
                .map(|c| &c.token_types)
        );

        self.startup(&p).await?;

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
                    label: Some(clap::crate_name!().into()),
                    work_done_progress_options: lsp::WorkDoneProgressOptions::default(),
                })),
                workspace_symbol_provider: Some(lsp::OneOf::Right(lsp::WorkspaceSymbolOptions {
                    resolve_provider: Some(true),
                    work_done_progress_options: lsp::WorkDoneProgressOptions::default(),
                })),
                ..lsp::ServerCapabilities::default()
            },
            server_info: Some(lsp::ServerInfo {
                name: clap::crate_name!().to_string(),
                version: Some(clap::crate_version!().to_string()),
            }),
        })
    }
}

impl Server {
    fn create(client: async_lsp::ClientSocket) -> async_lsp::router::Router<Arc<ServerState>> {
        use std::io::Error as E;

        let mut router = async_lsp::router::Router::new(Arc::new(ServerState {
            client: client.into(),
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

        router
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let (server, _) = async_lsp::MainLoop::new_server(|client| {
        use async_lsp::client_monitor::ClientProcessMonitorLayer;
        tower::ServiceBuilder::new()
            .layer(async_lsp::tracing::TracingLayer::default())
            .layer(async_lsp::server::LifecycleLayer::default())
            .layer(async_lsp::panic::CatchUnwindLayer::default())
            .layer(async_lsp::concurrency::ConcurrencyLayer::default())
            .layer(ClientProcessMonitorLayer::new(client.clone()))
            .service(Server::create(client))
    });

    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
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
