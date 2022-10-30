use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};
use log::{error, info, warn};
use lsp_server::{Connection, Message, RequestId};
use lsp_types::{notification::*, request::*, *};
use rowan::ast::AstNode;
use serde::Serialize;
use threadpool::ThreadPool;

use crate::{
    citation,
    client::LspClient,
    component_db::COMPONENT_DATABASE,
    debouncer,
    diagnostics::DiagnosticManager,
    dispatch::{NotificationDispatcher, RequestDispatcher},
    distro::Distribution,
    features::{
        execute_command, find_all_references, find_document_highlights, find_document_links,
        find_document_symbols, find_foldings, find_hover, find_inlay_hints, find_workspace_symbols,
        format_source_code, goto_definition, prepare_rename_all, rename_all, BuildEngine,
        BuildParams, BuildResult, BuildStatus, CompletionItemData, FeatureRequest, ForwardSearch,
        ForwardSearchResult, ForwardSearchStatus,
    },
    normalize_uri,
    syntax::bibtex,
    ClientCapabilitiesExt, Database, Document, DocumentData, DocumentLanguage, Environment,
    LineIndex, LineIndexExt, Options, StartupOptions, Workspace, WorkspaceEvent,
};

#[derive(Debug)]
enum InternalMessage {
    SetDistro(Distribution),
    SetOptions(Arc<Options>),
    FileEvent(notify::Event),
}

#[derive(Clone)]
struct ServerFork {
    connection: Arc<Connection>,
    internal_tx: Sender<InternalMessage>,
    client: LspClient,
    workspace: Workspace,
    diagnostic_tx: debouncer::Sender<Workspace>,
    diagnostic_manager: DiagnosticManager,
    build_engine: Arc<BuildEngine>,
}

impl ServerFork {
    pub fn register_config_capability(&self) {
        if self
            .workspace
            .environment
            .client_capabilities
            .has_push_configuration_support()
        {
            let reg = Registration {
                id: "pull-config".to_string(),
                method: DidChangeConfiguration::METHOD.to_string(),
                register_options: None,
            };

            let params = RegistrationParams {
                registrations: vec![reg],
            };

            if let Err(why) = self.client.send_request::<RegisterCapability>(params) {
                error!(
                    "Failed to register \"{}\" notification: {}",
                    DidChangeConfiguration::METHOD,
                    why
                );
            }
        }
    }

    pub fn pull_config(&self) -> Result<()> {
        if !self
            .workspace
            .environment
            .client_capabilities
            .has_pull_configuration_support()
        {
            return Ok(());
        }

        let params = ConfigurationParams {
            items: vec![ConfigurationItem {
                section: Some("texlab".to_string()),
                scope_uri: None,
            }],
        };

        match self.client.send_request::<WorkspaceConfiguration>(params) {
            Ok(mut json) => {
                let value = json.pop().expect("invalid configuration request");
                let options = self.parse_options(value)?;
                self.internal_tx
                    .send(InternalMessage::SetOptions(Arc::new(options)))
                    .unwrap();
            }
            Err(why) => {
                error!("Retrieving configuration failed: {}", why);
            }
        };

        Ok(())
    }

    pub fn parse_options(&self, value: serde_json::Value) -> Result<Options> {
        let options = match serde_json::from_value(value) {
            Ok(new_options) => new_options,
            Err(why) => {
                self.client.send_notification::<ShowMessage>(
                    ShowMessageParams {
                        message: format!(
                            "The texlab configuration is invalid; using the default settings instead.\nDetails: {why}"
                        ),
                        typ: MessageType::WARNING,
                    },
                )?;

                None
            }
        };

        Ok(options.unwrap_or_default())
    }

    pub fn feature_request<P>(&self, uri: Arc<Url>, params: P) -> FeatureRequest<P> {
        FeatureRequest {
            params,
            workspace: self.workspace.slice(&uri),
            uri,
        }
    }
}

pub struct Server {
    connection: Arc<Connection>,
    internal_tx: Sender<InternalMessage>,
    internal_rx: Receiver<InternalMessage>,
    client: LspClient,
    db: Database,
    workspace: Workspace,
    diagnostic_tx: debouncer::Sender<Workspace>,
    diagnostic_manager: DiagnosticManager,
    pool: ThreadPool,
    build_engine: Arc<BuildEngine>,
}

impl Server {
    pub fn new(connection: Connection, current_dir: PathBuf) -> Self {
        let client = LspClient::new(connection.sender.clone());
        let db = Database::default();
        let workspace = Workspace::new(Environment::new(Arc::new(current_dir)));
        let (internal_tx, internal_rx) = crossbeam_channel::unbounded();
        let diagnostic_manager = DiagnosticManager::default();
        let diagnostic_tx = create_debouncer(client.clone(), diagnostic_manager.clone());
        Self {
            connection: Arc::new(connection),
            internal_tx,
            internal_rx,
            client,
            db,
            workspace,
            diagnostic_tx,
            diagnostic_manager,
            pool: threadpool::Builder::new().build(),
            build_engine: Arc::default(),
        }
    }

    fn spawn(&self, job: impl FnOnce(ServerFork) + Send + 'static) {
        let fork = self.fork();
        self.pool.execute(move || job(fork));
    }

    fn fork(&self) -> ServerFork {
        ServerFork {
            connection: self.connection.clone(),
            internal_tx: self.internal_tx.clone(),
            client: self.client.clone(),
            workspace: self.workspace.clone(),
            diagnostic_tx: self.diagnostic_tx.clone(),
            diagnostic_manager: self.diagnostic_manager.clone(),
            build_engine: self.build_engine.clone(),
        }
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities {
            text_document_sync: Some(TextDocumentSyncCapability::Options(
                TextDocumentSyncOptions {
                    open_close: Some(true),
                    change: Some(TextDocumentSyncKind::INCREMENTAL),
                    will_save: None,
                    will_save_wait_until: None,
                    save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                        include_text: Some(false),
                    })),
                },
            )),
            document_link_provider: Some(DocumentLinkOptions {
                resolve_provider: Some(false),
                work_done_progress_options: WorkDoneProgressOptions::default(),
            }),
            folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
            definition_provider: Some(OneOf::Left(true)),
            references_provider: Some(OneOf::Left(true)),
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            completion_provider: Some(CompletionOptions {
                resolve_provider: Some(true),
                trigger_characters: Some(vec![
                    "\\".into(),
                    "{".into(),
                    "}".into(),
                    "@".into(),
                    "/".into(),
                    " ".into(),
                ]),
                ..CompletionOptions::default()
            }),
            document_symbol_provider: Some(OneOf::Left(true)),
            workspace_symbol_provider: Some(OneOf::Left(true)),
            rename_provider: Some(OneOf::Right(RenameOptions {
                prepare_provider: Some(true),
                work_done_progress_options: WorkDoneProgressOptions::default(),
            })),
            document_highlight_provider: Some(OneOf::Left(true)),
            document_formatting_provider: Some(OneOf::Left(true)),
            execute_command_provider: Some(ExecuteCommandOptions {
                commands: vec![
                    "texlab.cleanAuxiliary".into(),
                    "texlab.cleanArtifacts".into(),
                ],
                ..Default::default()
            }),
            inlay_hint_provider: Some(OneOf::Left(true)),
            ..ServerCapabilities::default()
        }
    }

    fn initialize(&mut self) -> Result<()> {
        let (id, params) = self.connection.initialize_start()?;
        let params: InitializeParams = serde_json::from_value(params)?;

        self.workspace.environment.client_capabilities = Arc::new(params.capabilities);
        self.workspace.environment.client_info = params.client_info.map(Arc::new);

        let result = InitializeResult {
            capabilities: self.capabilities(),
            server_info: Some(ServerInfo {
                name: "TexLab".to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
            offset_encoding: None,
        };
        self.connection
            .initialize_finish(id, serde_json::to_value(result)?)?;

        let StartupOptions { skip_distro } =
            serde_json::from_value(params.initialization_options.unwrap_or_default())
                .unwrap_or_default();

        if !skip_distro {
            self.spawn(move |server| {
                let distro = Distribution::detect();
                info!("Detected distribution: {}", distro.kind);

                server
                    .internal_tx
                    .send(InternalMessage::SetDistro(distro))
                    .unwrap();
            });
        }

        self.register_diagnostics_handler();
        self.register_file_watching();

        self.spawn(move |server| {
            server.register_config_capability();
            let _ = server.pull_config();
        });

        Ok(())
    }

    fn register_file_watching(&mut self) {
        let tx = self.internal_tx.clone();
        let watcher = notify::recommended_watcher(move |ev: Result<_, _>| {
            if let Ok(ev) = ev {
                let _ = tx.send(InternalMessage::FileEvent(ev));
            }
        });

        if let Ok(watcher) = watcher {
            self.workspace.register_watcher(watcher);
        }
    }

    fn register_diagnostics_handler(&mut self) {
        let (event_sender, event_receiver) = crossbeam_channel::unbounded();
        let diagnostic_tx = self.diagnostic_tx.clone();
        let diagnostic_manager = self.diagnostic_manager.clone();
        std::thread::spawn(move || {
            for event in event_receiver {
                match event {
                    WorkspaceEvent::Changed(workspace, document) => {
                        diagnostic_manager.push_syntax(&workspace, document.uri());
                        let delay = workspace.environment.options.diagnostics_delay;
                        diagnostic_tx.send(workspace, delay.0).unwrap();
                    }
                };
            }
        });

        self.workspace.listeners.push(event_sender);
    }

    fn cancel(&self, _params: CancelParams) -> Result<()> {
        Ok(())
    }

    fn did_change_watched_files(&mut self, _params: DidChangeWatchedFilesParams) -> Result<()> {
        Ok(())
    }

    fn did_change_configuration(&mut self, params: DidChangeConfigurationParams) -> Result<()> {
        if self
            .workspace
            .environment
            .client_capabilities
            .has_pull_configuration_support()
        {
            self.spawn(move |server| {
                let _ = server.pull_config();
            });
        } else {
            let options = self.fork().parse_options(params.settings)?;
            self.workspace.environment.options = Arc::new(options);
            self.reparse_all()?;
        }

        Ok(())
    }

    fn did_open(&mut self, mut params: DidOpenTextDocumentParams) -> Result<()> {
        normalize_uri(&mut params.text_document.uri);

        let language_id = &params.text_document.language_id;
        let language = DocumentLanguage::by_language_id(language_id);
        let document = self.workspace.open(
            Arc::new(params.text_document.uri),
            Arc::new(params.text_document.text),
            language.unwrap_or(DocumentLanguage::Latex),
        )?;

        self.workspace.viewport.insert(Arc::clone(document.uri()));

        if self.workspace.environment.options.chktex.on_open_and_save {
            self.run_chktex(document);
        }

        Ok(())
    }

    fn did_change(&mut self, mut params: DidChangeTextDocumentParams) -> Result<()> {
        normalize_uri(&mut params.text_document.uri);

        let uri = Arc::new(params.text_document.uri);
        match self.workspace.get(&uri) {
            Some(old_document) => {
                let mut text = old_document.text().to_string();
                apply_document_edit(&mut text, params.content_changes);
                let language = old_document.data().language();
                let new_document =
                    self.workspace
                        .open(Arc::clone(&uri), Arc::new(text), language)?;
                self.workspace
                    .viewport
                    .insert(Arc::clone(new_document.uri()));

                self.build_engine.positions_by_uri.insert(
                    Arc::clone(&uri),
                    Position::new(
                        old_document
                            .text()
                            .lines()
                            .zip(new_document.text().lines())
                            .position(|(a, b)| a != b)
                            .unwrap_or_default() as u32,
                        0,
                    ),
                );

                if self.workspace.environment.options.chktex.on_edit {
                    self.run_chktex(new_document);
                };
            }
            None => match uri.to_file_path() {
                Ok(path) => {
                    self.workspace.load(path)?;
                }
                Err(_) => return Ok(()),
            },
        };

        Ok(())
    }

    fn did_save(&mut self, params: DidSaveTextDocumentParams) -> Result<()> {
        let mut uri = params.text_document.uri;
        normalize_uri(&mut uri);

        if let Some(request) = self
            .workspace
            .get(&uri)
            .filter(|_| self.workspace.environment.options.build.on_save)
            .map(|document| {
                self.feature_request(
                    Arc::clone(document.uri()),
                    BuildParams {
                        text_document: TextDocumentIdentifier::new(uri.clone()),
                    },
                )
            })
        {
            self.spawn(move |server| {
                server
                    .build_engine
                    .build(request, server.client)
                    .unwrap_or_else(|why| {
                        error!("Build failed: {}", why);
                        BuildResult {
                            status: BuildStatus::FAILURE,
                        }
                    });
            });
        }

        if let Some(document) = self
            .workspace
            .get(&uri)
            .filter(|_| self.workspace.environment.options.chktex.on_open_and_save)
        {
            self.run_chktex(document);
        }

        Ok(())
    }

    fn did_close(&mut self, mut params: DidCloseTextDocumentParams) -> Result<()> {
        normalize_uri(&mut params.text_document.uri);
        self.workspace.close(&params.text_document.uri);
        Ok(())
    }

    fn run_chktex(&mut self, document: Document) {
        self.spawn(move |server| {
            server
                .diagnostic_manager
                .push_chktex(&server.workspace, document.uri());

            let delay = server.workspace.environment.options.diagnostics_delay;
            server
                .diagnostic_tx
                .send(server.workspace.clone(), delay.0)
                .unwrap();
        });
    }

    fn feature_request<P>(&self, uri: Arc<Url>, params: P) -> FeatureRequest<P> {
        FeatureRequest {
            params,
            workspace: self.workspace.slice(&uri),
            uri,
        }
    }

    fn handle_feature_request<P, R, H>(
        &self,
        id: RequestId,
        params: P,
        uri: Arc<Url>,
        handler: H,
    ) -> Result<()>
    where
        P: Send + 'static,
        R: Serialize,
        H: FnOnce(FeatureRequest<P>) -> R + Send + 'static,
    {
        self.spawn(move |server| {
            let request = server.feature_request(uri, params);
            if request.workspace.iter().next().is_none() {
                let code = lsp_server::ErrorCode::InvalidRequest as i32;
                let message = "unknown document".to_string();
                let response = lsp_server::Response::new_err(id, code, message);
                server.connection.sender.send(response.into()).unwrap();
            } else {
                let result = handler(request);
                server
                    .connection
                    .sender
                    .send(lsp_server::Response::new_ok(id, result).into())
                    .unwrap();
            }
        });

        Ok(())
    }

    fn document_link(&self, id: RequestId, mut params: DocumentLinkParams) -> Result<()> {
        normalize_uri(&mut params.text_document.uri);
        let uri = Arc::new(params.text_document.uri.clone());
        self.handle_feature_request(id, params, uri, find_document_links)?;
        Ok(())
    }

    fn document_symbols(&self, id: RequestId, mut params: DocumentSymbolParams) -> Result<()> {
        normalize_uri(&mut params.text_document.uri);
        let uri = Arc::new(params.text_document.uri.clone());
        self.handle_feature_request(id, params, uri, find_document_symbols)?;
        Ok(())
    }

    fn workspace_symbols(&self, id: RequestId, params: WorkspaceSymbolParams) -> Result<()> {
        self.spawn(move |server| {
            let result = find_workspace_symbols(&server.workspace, &params);
            server
                .connection
                .sender
                .send(lsp_server::Response::new_ok(id, result).into())
                .unwrap();
        });
        Ok(())
    }

    fn completion(&self, id: RequestId, mut params: CompletionParams) -> Result<()> {
        normalize_uri(&mut params.text_document_position.text_document.uri);
        let uri = Arc::new(params.text_document_position.text_document.uri.clone());

        self.build_engine
            .positions_by_uri
            .insert(Arc::clone(&uri), params.text_document_position.position);

        self.handle_feature_request(id, params, uri, crate::features::complete)?;
        Ok(())
    }

    fn completion_resolve(&self, id: RequestId, mut item: CompletionItem) -> Result<()> {
        self.spawn(move |server| {
            match serde_json::from_value(item.data.clone().unwrap()).unwrap() {
                CompletionItemData::Package | CompletionItemData::Class => {
                    item.documentation = COMPONENT_DATABASE
                        .documentation(&item.label)
                        .map(Documentation::MarkupContent);
                }
                CompletionItemData::Citation { uri, key } => {
                    if let Some(root) = server.workspace.get(&uri).and_then(|document| {
                        document
                            .data()
                            .as_bibtex()
                            .map(|data| bibtex::SyntaxNode::new_root(data.green.clone()))
                    }) {
                        item.documentation = bibtex::Root::cast(root)
                            .and_then(|root| root.find_entry(&key))
                            .and_then(|entry| citation::render(&entry))
                            .map(|value| {
                                Documentation::MarkupContent(MarkupContent {
                                    kind: MarkupKind::Markdown,
                                    value,
                                })
                            });
                    }
                }
                _ => {}
            };

            server
                .connection
                .sender
                .send(lsp_server::Response::new_ok(id, item).into())
                .unwrap();
        });
        Ok(())
    }

    fn folding_range(&self, id: RequestId, mut params: FoldingRangeParams) -> Result<()> {
        normalize_uri(&mut params.text_document.uri);
        let uri = Arc::new(params.text_document.uri.clone());
        self.handle_feature_request(id, params, uri, find_foldings)?;
        Ok(())
    }

    fn references(&self, id: RequestId, mut params: ReferenceParams) -> Result<()> {
        normalize_uri(&mut params.text_document_position.text_document.uri);
        let uri = Arc::new(params.text_document_position.text_document.uri.clone());
        self.handle_feature_request(id, params, uri, find_all_references)?;
        Ok(())
    }

    fn hover(&self, id: RequestId, mut params: HoverParams) -> Result<()> {
        normalize_uri(&mut params.text_document_position_params.text_document.uri);
        let uri = Arc::new(
            params
                .text_document_position_params
                .text_document
                .uri
                .clone(),
        );
        self.build_engine.positions_by_uri.insert(
            Arc::clone(&uri),
            params.text_document_position_params.position,
        );

        self.handle_feature_request(id, params, uri, find_hover)?;
        Ok(())
    }

    fn goto_definition(&self, id: RequestId, mut params: GotoDefinitionParams) -> Result<()> {
        normalize_uri(&mut params.text_document_position_params.text_document.uri);
        let uri = Arc::new(
            params
                .text_document_position_params
                .text_document
                .uri
                .clone(),
        );
        self.handle_feature_request(id, params, uri, goto_definition)?;
        Ok(())
    }

    fn prepare_rename(&self, id: RequestId, mut params: TextDocumentPositionParams) -> Result<()> {
        normalize_uri(&mut params.text_document.uri);
        let uri = Arc::new(params.text_document.uri.clone());
        self.handle_feature_request(id, params, uri, prepare_rename_all)?;
        Ok(())
    }

    fn rename(&self, id: RequestId, mut params: RenameParams) -> Result<()> {
        normalize_uri(&mut params.text_document_position.text_document.uri);
        let uri = Arc::new(params.text_document_position.text_document.uri.clone());
        self.handle_feature_request(id, params, uri, rename_all)?;
        Ok(())
    }

    fn document_highlight(&self, id: RequestId, mut params: DocumentHighlightParams) -> Result<()> {
        normalize_uri(&mut params.text_document_position_params.text_document.uri);
        let uri = Arc::new(
            params
                .text_document_position_params
                .text_document
                .uri
                .clone(),
        );
        self.handle_feature_request(id, params, uri, find_document_highlights)?;
        Ok(())
    }

    fn formatting(&self, id: RequestId, mut params: DocumentFormattingParams) -> Result<()> {
        normalize_uri(&mut params.text_document.uri);
        let uri = Arc::new(params.text_document.uri.clone());
        self.handle_feature_request(id, params, uri, format_source_code)?;
        Ok(())
    }

    fn execute_command(&self, id: RequestId, params: ExecuteCommandParams) -> Result<()> {
        self.spawn(move |server| {
            let result = execute_command(&server.workspace, &params.command, params.arguments);
            let response = match result {
                Ok(()) => lsp_server::Response::new_ok(id, ()),
                Err(why) => lsp_server::Response::new_err(
                    id,
                    lsp_server::ErrorCode::InternalError as i32,
                    why.to_string(),
                ),
            };

            server.connection.sender.send(response.into()).unwrap();
        });

        Ok(())
    }

    fn inlay_hints(&self, id: RequestId, mut params: InlayHintParams) -> Result<()> {
        normalize_uri(&mut params.text_document.uri);
        let uri = Arc::new(params.text_document.uri.clone());
        self.handle_feature_request(id, params, uri, find_inlay_hints)?;
        Ok(())
    }

    fn inlay_hint_resolve(&self, id: RequestId, hint: InlayHint) -> Result<()> {
        let response = lsp_server::Response::new_ok(id, hint);
        self.connection.sender.send(response.into()).unwrap();
        Ok(())
    }

    fn semantic_tokens_range(
        &self,
        _id: RequestId,
        _params: SemanticTokensRangeParams,
    ) -> Result<()> {
        Ok(())
    }

    fn build(&self, id: RequestId, mut params: BuildParams) -> Result<()> {
        normalize_uri(&mut params.text_document.uri);
        let uri = Arc::new(params.text_document.uri.clone());
        let client = self.client.clone();
        let build_engine = Arc::clone(&self.build_engine);
        self.handle_feature_request(id, params, uri, move |request| {
            build_engine.build(request, client).unwrap_or_else(|why| {
                error!("Build failed: {}", why);
                BuildResult {
                    status: BuildStatus::FAILURE,
                }
            })
        })?;
        Ok(())
    }

    fn forward_search(&self, id: RequestId, mut params: TextDocumentPositionParams) -> Result<()> {
        normalize_uri(&mut params.text_document.uri);
        let uri = Arc::new(params.text_document.uri.clone());
        self.handle_feature_request(id, params, uri, |req| {
            let options = &req.workspace.environment.options.forward_search;
            match options.executable.as_deref().zip(options.args.as_deref()) {
                Some((executable, args)) => ForwardSearch::builder()
                    .executable(executable)
                    .args(args)
                    .line(req.params.position.line)
                    .workspace(&req.workspace)
                    .tex_uri(&req.uri)
                    .build()
                    .execute()
                    .unwrap_or(ForwardSearchResult {
                        status: ForwardSearchStatus::ERROR,
                    }),
                None => ForwardSearchResult {
                    status: ForwardSearchStatus::UNCONFIGURED,
                },
            }
        })?;
        Ok(())
    }

    fn reparse_all(&mut self) -> Result<()> {
        for document in self.workspace.iter().collect::<Vec<_>>() {
            self.workspace.open(
                Arc::clone(document.uri()),
                Arc::new(document.text().to_string()),
                document.data().language(),
            )?;
        }

        match &self.workspace.environment.options.aux_directory {
            Some(path) => self.workspace.watch_dir(path),
            None => self.workspace.watch_dir(&PathBuf::from(".")),
        };

        Ok(())
    }

    fn process_messages(&mut self) -> Result<()> {
        loop {
            crossbeam_channel::select! {
                recv(&self.connection.receiver) -> msg => {
                    match msg? {
                        Message::Request(request) => {
                            if self.connection.handle_shutdown(&request)? {
                                return Ok(());
                            }

                            if let Some(response) = RequestDispatcher::new(request)
                                .on::<DocumentLinkRequest, _>(|id, params| self.document_link(id, params))?
                                .on::<FoldingRangeRequest, _>(|id, params| self.folding_range(id, params))?
                                .on::<References, _>(|id, params| self.references(id, params))?
                                .on::<HoverRequest, _>(|id, params| self.hover(id, params))?
                                .on::<DocumentSymbolRequest, _>(|id, params| {
                                    self.document_symbols(id, params)
                                })?
                                .on::<WorkspaceSymbol, _>(|id, params| self.workspace_symbols(id, params))?
                                .on::<Completion, _>(|id, params| {
                                    self.completion(id, params)?;
                                    Ok(())
                                })?
                                .on::<ResolveCompletionItem, _>(|id, params| {
                                    self.completion_resolve(id, params)?;
                                    Ok(())
                                })?
                                .on::<GotoDefinition, _>(|id, params| self.goto_definition(id, params))?
                                .on::<PrepareRenameRequest, _>(|id, params| {
                                    self.prepare_rename(id, params)
                                })?
                                .on::<Rename, _>(|id, params| self.rename(id, params))?
                                .on::<DocumentHighlightRequest, _>(|id, params| {
                                    self.document_highlight(id, params)
                                })?
                                .on::<Formatting, _>(|id, params| self.formatting(id, params))?
                                .on::<BuildRequest, _>(|id, params| self.build(id, params))?
                                .on::<ForwardSearchRequest, _>(|id, params| {
                                    self.forward_search(id, params)
                                })?
                                .on::<ExecuteCommand,_>(|id, params| self.execute_command(id, params))?
                                .on::<SemanticTokensRangeRequest, _>(|id, params| {
                                    self.semantic_tokens_range(id, params)
                                })?
                                .on::<InlayHintRequest, _>(|id,params| {
                                    self.inlay_hints(id, params)
                                })?
                                .on::<InlayHintResolveRequest,_>(|id, params| {
                                    self.inlay_hint_resolve(id, params)
                                })?
                                .default()
                            {
                                self.connection.sender.send(response.into())?;
                            }
                        }
                        Message::Notification(notification) => {
                            NotificationDispatcher::new(notification)
                                .on::<Cancel, _>(|params| self.cancel(params))?
                                .on::<DidChangeConfiguration, _>(|params| {
                                    self.did_change_configuration(params)
                                })?
                                .on::<DidChangeWatchedFiles, _>(|params| {
                                    self.did_change_watched_files(params)
                                })?
                                .on::<DidOpenTextDocument, _>(|params| self.did_open(params))?
                                .on::<DidChangeTextDocument, _>(|params| self.did_change(params))?
                                .on::<DidSaveTextDocument, _>(|params| self.did_save(params))?
                                .on::<DidCloseTextDocument, _>(|params| self.did_close(params))?
                                .default();
                        }
                        Message::Response(response) => {
                            self.client.recv_response(response)?;
                        }
                    };
                },
                recv(&self.internal_rx) -> msg => {
                    match msg? {
                        InternalMessage::SetDistro(distro) => {
                            self.workspace.environment.resolver = Arc::new(distro.resolver);
                            self.reparse_all()?;
                        }
                        InternalMessage::SetOptions(options) => {
                            self.workspace.environment.options = options;
                            self.reparse_all()?;
                        }
                        InternalMessage::FileEvent(ev) => {
                            match ev.kind {
                                notify::EventKind::Create(_) | notify::EventKind::Modify(_) => {
                                    for path in ev.paths {
                                        let _ = self.workspace.reload(path);
                                    }
                                }
                                notify::EventKind::Remove(_) => {
                                    for uri in ev.paths.iter().flat_map(Url::from_file_path) {
                                        self.workspace.remove(&uri);
                                    }
                                }
                                notify::EventKind::Any
                                | notify::EventKind::Access(_)
                                | notify::EventKind::Other => {}
                            };
                        }
                    };
                }
            };
        }
    }

    pub fn run(mut self) -> Result<()> {
        self.initialize()?;
        self.process_messages()?;
        self.pool.join();
        Ok(())
    }
}

fn create_debouncer(
    client: LspClient,
    diagnostic_manager: DiagnosticManager,
) -> debouncer::Sender<Workspace> {
    let (tx, rx) = debouncer::unbounded();
    std::thread::spawn(move || {
        while let Ok(workspace) = rx.recv() {
            if let Err(why) = publish_diagnostics(&client, &diagnostic_manager, &workspace) {
                warn!("Failed to publish diagnostics: {}", why);
            }
        }
    });

    tx
}

fn publish_diagnostics(
    client: &LspClient,
    diagnostic_manager: &DiagnosticManager,
    workspace: &Workspace,
) -> Result<()> {
    for document in workspace.iter() {
        if matches!(document.data(), DocumentData::BuildLog(_)) {
            continue;
        }

        let diagnostics = diagnostic_manager.publish(workspace, document.uri());
        client.send_notification::<PublishDiagnostics>(PublishDiagnosticsParams {
            uri: document.uri().as_ref().clone(),
            version: None,
            diagnostics,
        })?;
    }

    Ok(())
}

fn apply_document_edit(old_text: &mut String, changes: Vec<TextDocumentContentChangeEvent>) {
    for change in changes {
        let line_index = LineIndex::new(old_text);
        match change.range {
            Some(range) => {
                let range = std::ops::Range::<usize>::from(line_index.offset_lsp_range(range));
                old_text.replace_range(range, &change.text);
            }
            None => {
                *old_text = change.text;
            }
        };
    }
}

struct BuildRequest;

impl lsp_types::request::Request for BuildRequest {
    type Params = BuildParams;

    type Result = BuildResult;

    const METHOD: &'static str = "textDocument/build";
}

struct ForwardSearchRequest;

impl lsp_types::request::Request for ForwardSearchRequest {
    type Params = TextDocumentPositionParams;

    type Result = ForwardSearchResult;

    const METHOD: &'static str = "textDocument/forwardSearch";
}
