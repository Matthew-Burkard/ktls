//! The language server: LSP lifecycle, document sync, scheduling and dispatch.
//!
//! The main loop owns all mutable state. Requests run on the rayon pool against
//! immutable [`Snapshot`]s and report back through the task channel.

pub mod config;
mod handlers;
pub mod line_index;
mod loader;
mod to_proto;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use crossbeam_channel::{Receiver, Sender, select};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::Notification as _;
use lsp_types::{
    ClientCapabilities, InitializeParams, InitializeResult, MessageType, PositionEncodingKind, ServerCapabilities, ServerInfo,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions, Uri,
};

use self::config::Config;
use self::handlers::{Cancelled, RequestError};
use self::line_index::Encoding;
use self::loader::{Loaded, ProgressEvent};
use crate::analysis::diagnostics::Coverage;
use crate::analysis::{AnalysisHost, Snapshot};
use crate::util::{path_to_uri, uri_to_path};

pub const COMMAND_RELOAD: &str = "kotlin.reloadProject";
pub const COMMAND_MODEL: &str = "kotlin.projectModel";

/// Messages from background work to the main loop.
pub enum Task {
    Respond(Response),
    Diagnostics { path: PathBuf, version: Option<i32>, diagnostics: Vec<lsp_types::Diagnostic>, coverage: Coverage },
    Loaded(Box<Loaded>),
    Progress(ProgressEvent),
}

struct OpenDoc {
    version: i32,
    text: String,
}

/// What an outgoing server→client request was for.
enum Outgoing {
    Configuration,
    Other,
}

pub struct Server {
    sender: Sender<Message>,
    tasks_tx: Sender<Task>,
    tasks_rx: Receiver<Task>,
    host: AnalysisHost,
    docs: HashMap<PathBuf, OpenDoc>,
    config: Config,
    encoding: Encoding,
    caps: ClientCapabilities,
    folders: Vec<PathBuf>,
    /// In-flight requests and their cancellation flags.
    in_flight: HashMap<RequestId, Arc<AtomicBool>>,
    outgoing: HashMap<RequestId, Outgoing>,
    next_id: i32,
    /// Flag shared by all work on the current snapshot; set when the state changes.
    snapshot_flag: Arc<AtomicBool>,
    diagnostics_due: HashMap<PathBuf, Instant>,
    reload_due: Option<Instant>,
    load_generation: u64,
    loading: bool,
    pending_reload: bool,
    progress_token: Option<lsp_types::NumberOrString>,
    shown_messages: HashSet<String>,
    shutdown: bool,
}

pub fn server_capabilities(encoding: Encoding, pull_diagnostics: bool, prepare_rename: bool) -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(match encoding {
            Encoding::Utf8 => PositionEncodingKind::UTF8,
            Encoding::Utf16 => PositionEncodingKind::UTF16,
        }),
        text_document_sync: Some(TextDocumentSyncCapability::Options(TextDocumentSyncOptions {
            open_close: Some(true),
            change: Some(TextDocumentSyncKind::INCREMENTAL),
            will_save: None,
            will_save_wait_until: None,
            save: Some(lsp_types::TextDocumentSyncSaveOptions::Supported(true)),
        })),
        document_symbol_provider: Some(lsp_types::OneOf::Left(true)),
        definition_provider: Some(lsp_types::OneOf::Left(true)),
        references_provider: Some(lsp_types::OneOf::Left(true)),
        rename_provider: Some(if prepare_rename {
            lsp_types::OneOf::Right(lsp_types::RenameOptions { prepare_provider: Some(true), work_done_progress_options: Default::default() })
        } else {
            lsp_types::OneOf::Left(true)
        }),
        hover_provider: Some(lsp_types::HoverProviderCapability::Simple(true)),
        completion_provider: Some(lsp_types::CompletionOptions {
            trigger_characters: Some(vec![".".into(), ":".into(), "@".into()]),
            resolve_provider: Some(false),
            completion_item: Some(lsp_types::CompletionOptionsCompletionItem { label_details_support: Some(true) }),
            ..Default::default()
        }),
        code_action_provider: Some(lsp_types::CodeActionProviderCapability::Options(lsp_types::CodeActionOptions {
            code_action_kinds: Some(vec![
                lsp_types::CodeActionKind::QUICKFIX,
                lsp_types::CodeActionKind::REFACTOR_REWRITE,
                lsp_types::CodeActionKind::SOURCE_FIX_ALL,
            ]),
            resolve_provider: None,
            work_done_progress_options: Default::default(),
        })),
        workspace_symbol_provider: Some(lsp_types::OneOf::Left(true)),
        execute_command_provider: Some(lsp_types::ExecuteCommandOptions {
            commands: vec![COMMAND_RELOAD.into(), COMMAND_MODEL.into()],
            work_done_progress_options: Default::default(),
        }),
        diagnostic_provider: pull_diagnostics.then(|| {
            lsp_types::DiagnosticServerCapabilities::Options(lsp_types::DiagnosticOptions {
                identifier: Some("kotlin".into()),
                inter_file_dependencies: true,
                workspace_diagnostics: false,
                work_done_progress_options: Default::default(),
            })
        }),
        workspace: Some(lsp_types::WorkspaceServerCapabilities {
            workspace_folders: Some(lsp_types::WorkspaceFoldersServerCapabilities {
                supported: Some(true),
                change_notifications: Some(lsp_types::OneOf::Left(true)),
            }),
            file_operations: None,
        }),
        ..Default::default()
    }
}

/// Run the server over `connection` until `exit`.
pub fn run(connection: Connection) -> anyhow::Result<()> {
    let (id, params) = connection.initialize_start()?;
    let init: InitializeParams = serde_json::from_value(params).context("invalid initialize params")?;
    let encoding = match init.capabilities.general.as_ref().and_then(|g| g.position_encodings.as_ref()) {
        Some(encs) if encs.contains(&PositionEncodingKind::UTF8) => Encoding::Utf8,
        _ => Encoding::Utf16,
    };
    let pull = init.capabilities.text_document.as_ref().is_some_and(|t| t.diagnostic.is_some());
    let result = InitializeResult {
        capabilities: server_capabilities(
            encoding,
            pull,
            init.capabilities.text_document.as_ref().and_then(|t| t.rename.as_ref()).and_then(|r| r.prepare_support).unwrap_or(false),
        ),
        server_info: Some(ServerInfo { name: "kotlin-lsp-rust".into(), version: Some(env!("CARGO_PKG_VERSION").into()) }),
    };
    connection.initialize_finish(id, serde_json::to_value(result)?)?;

    #[allow(deprecated)]
    let mut folders: Vec<PathBuf> = init.workspace_folders.iter().flatten().filter_map(|f| uri_to_path(&f.uri)).collect();
    #[allow(deprecated)]
    if folders.is_empty()
        && let Some(root) = init.root_uri.as_ref().and_then(uri_to_path) {
            folders.push(root);
        }
    let config = init.initialization_options.as_ref().map(Config::from_json).unwrap_or_default();
    let (tasks_tx, tasks_rx) = crossbeam_channel::unbounded();
    let mut server = Server {
        sender: connection.sender.clone(),
        tasks_tx,
        tasks_rx,
        host: {
            let mut host = AnalysisHost::default();
            host.set_lint_config(config.lint.clone());
            host
        },
        docs: HashMap::new(),
        config,
        encoding,
        caps: init.capabilities,
        folders,
        in_flight: HashMap::new(),
        outgoing: HashMap::new(),
        next_id: 0,
        snapshot_flag: handlers::new_flag(),
        diagnostics_due: HashMap::new(),
        reload_due: None,
        load_generation: 0,
        loading: false,
        pending_reload: false,
        progress_token: None,
        shown_messages: HashSet::new(),
        shutdown: false,
    };
    // `initialize_finish` already consumed the client's `initialized` notification.
    server.on_initialized();
    server.main_loop(&connection.receiver)
}

impl Server {
    fn pull_diagnostics(&self) -> bool {
        self.caps.text_document.as_ref().is_some_and(|t| t.diagnostic.is_some())
    }

    fn send(&self, msg: impl Into<Message>) {
        let _ = self.sender.send(msg.into());
    }

    fn notify<N: lsp_types::notification::Notification>(&self, params: N::Params) {
        self.send(Notification::new(N::METHOD.to_string(), params));
    }

    fn request<R: lsp_types::request::Request>(&mut self, params: R::Params, purpose: Outgoing) {
        self.next_id += 1;
        let id = RequestId::from(format!("kotlin-lsp-{}", self.next_id));
        self.outgoing.insert(id.clone(), purpose);
        self.send(Request::new(id, R::METHOD.to_string(), params));
    }

    fn log(&self, typ: MessageType, message: impl Into<String>) {
        self.notify::<lsp_types::notification::LogMessage>(lsp_types::LogMessageParams { typ, message: message.into() });
    }

    fn show_once(&mut self, typ: MessageType, message: String) {
        if self.shown_messages.insert(message.clone()) {
            self.notify::<lsp_types::notification::ShowMessage>(lsp_types::ShowMessageParams { typ, message });
        }
    }

    fn main_loop(&mut self, receiver: &Receiver<Message>) -> anyhow::Result<()> {
        loop {
            let now = Instant::now();
            let next_due = self.diagnostics_due.values().copied().chain(self.reload_due).min();
            let timeout = next_due.map(|d| d.saturating_duration_since(now)).unwrap_or(Duration::from_secs(3600));
            select! {
                recv(receiver) -> msg => {
                    let Ok(msg) = msg else { return Ok(()) };
                    match msg {
                        Message::Request(req) => self.on_request(req),
                        Message::Notification(n) => {
                            if n.method == lsp_types::notification::Exit::METHOD {
                                return Ok(());
                            }
                            self.on_notification(n);
                        }
                        Message::Response(resp) => self.on_response(resp),
                    }
                }
                recv(self.tasks_rx) -> task => {
                    if let Ok(task) = task {
                        self.on_task(task);
                    }
                }
                default(timeout) => {}
            }
            self.run_due();
        }
    }

    // ------------------------------------------------------------ requests

    fn on_request(&mut self, req: Request) {
        if self.shutdown {
            self.send(Response::new_err(req.id, ErrorCode::InvalidRequest as i32, "server is shutting down".into()));
            return;
        }
        use lsp_types::request::*;
        match req.method.as_str() {
            Shutdown::METHOD => {
                self.shutdown = true;
                self.send(Response::new_ok(req.id, ()));
            }
            DocumentSymbolRequest::METHOD => {
                let Ok((id, params)) = req.extract::<lsp_types::DocumentSymbolParams>(DocumentSymbolRequest::METHOD) else { return };
                let path = self.path_of(&params.text_document.uri);
                self.spawn(id, move |cx| {
                    let path = path.ok_or(Cancelled)?;
                    let syms = handlers::document_symbols(cx, &path)?;
                    Ok(serde_json::to_value(lsp_types::DocumentSymbolResponse::Nested(syms)).unwrap())
                });
            }
            GotoDefinition::METHOD => {
                let Ok((id, params)) = req.extract::<lsp_types::GotoDefinitionParams>(GotoDefinition::METHOD) else { return };
                let tdp = params.text_document_position_params;
                let path = self.path_of(&tdp.text_document.uri);
                self.spawn(id, move |cx| {
                    let path = path.ok_or(Cancelled)?;
                    let locs = handlers::definition(cx, &path, tdp.position)?;
                    Ok(serde_json::to_value(lsp_types::GotoDefinitionResponse::Array(locs)).unwrap())
                });
            }
            References::METHOD => {
                let Ok((id, params)) = req.extract::<lsp_types::ReferenceParams>(References::METHOD) else { return };
                let tdp = params.text_document_position;
                let include = params.context.include_declaration;
                let path = self.path_of(&tdp.text_document.uri);
                self.spawn(id, move |cx| {
                    let path = path.ok_or(Cancelled)?;
                    Ok(serde_json::to_value(handlers::references(cx, &path, tdp.position, include)?).unwrap())
                });
            }
            Completion::METHOD => {
                let Ok((id, params)) = req.extract::<lsp_types::CompletionParams>(Completion::METHOD) else { return };
                let tdp = params.text_document_position;
                let path = self.path_of(&tdp.text_document.uri);
                let snippets = self
                    .caps
                    .text_document
                    .as_ref()
                    .and_then(|t| t.completion.as_ref())
                    .and_then(|c| c.completion_item.as_ref())
                    .and_then(|i| i.snippet_support)
                    .unwrap_or(false);
                self.spawn(id, move |cx| {
                    let path = path.ok_or(Cancelled)?;
                    Ok(serde_json::to_value(handlers::completion(cx, &path, tdp.position, snippets)?).unwrap())
                });
            }
            PrepareRenameRequest::METHOD => {
                let Ok((id, params)) = req.extract::<lsp_types::TextDocumentPositionParams>(PrepareRenameRequest::METHOD) else { return };
                let path = self.path_of(&params.text_document.uri);
                self.spawn(id, move |cx| {
                    let path = path.ok_or(Cancelled)?;
                    Ok(serde_json::to_value(handlers::prepare_rename(cx, &path, params.position)?).unwrap())
                });
            }
            Rename::METHOD => {
                let Ok((id, params)) = req.extract::<lsp_types::RenameParams>(Rename::METHOD) else { return };
                let tdp = params.text_document_position;
                let path = self.path_of(&tdp.text_document.uri);
                let rename_files = self
                    .caps
                    .workspace
                    .as_ref()
                    .and_then(|w| w.workspace_edit.as_ref())
                    .and_then(|e| e.resource_operations.as_ref())
                    .is_some_and(|ops| ops.contains(&lsp_types::ResourceOperationKind::Rename));
                self.spawn(id, move |cx| {
                    let path = path.ok_or(Cancelled)?;
                    Ok(serde_json::to_value(handlers::rename(cx, &path, tdp.position, &params.new_name, rename_files)?).unwrap())
                });
            }
            HoverRequest::METHOD => {
                let Ok((id, params)) = req.extract::<lsp_types::HoverParams>(HoverRequest::METHOD) else { return };
                let tdp = params.text_document_position_params;
                let path = self.path_of(&tdp.text_document.uri);
                self.spawn(id, move |cx| {
                    let path = path.ok_or(Cancelled)?;
                    Ok(serde_json::to_value(handlers::hover(cx, &path, tdp.position)?).unwrap())
                });
            }
            CodeActionRequest::METHOD => {
                let Ok((id, params)) = req.extract::<lsp_types::CodeActionParams>(CodeActionRequest::METHOD) else { return };
                let path = self.path_of(&params.text_document.uri);
                let enabled = self.config.lint.enabled;
                self.spawn(id, move |cx| {
                    let actions = match (&path, enabled) {
                        (Some(p), true) => handlers::code_actions(cx, p, params.range, params.context.only.as_deref())?,
                        _ => Vec::new(),
                    };
                    Ok(serde_json::to_value(actions).unwrap())
                });
            }
            WorkspaceSymbolRequest::METHOD => {
                let Ok((id, params)) = req.extract::<lsp_types::WorkspaceSymbolParams>(WorkspaceSymbolRequest::METHOD) else { return };
                self.spawn(id, move |cx| {
                    let syms = handlers::workspace_symbols(cx, &params.query)?;
                    Ok(serde_json::to_value(lsp_types::WorkspaceSymbolResponse::Nested(syms)).unwrap())
                });
            }
            DocumentDiagnosticRequest::METHOD => {
                let Ok((id, params)) = req.extract::<lsp_types::DocumentDiagnosticParams>(DocumentDiagnosticRequest::METHOD) else { return };
                let path = self.path_of(&params.text_document.uri);
                let enabled = self.config.diagnostics.enabled;
                self.spawn(id, move |cx| {
                    let items = match (&path, enabled) {
                        (Some(p), true) => handlers::file_diagnostics(cx, p)?.0,
                        _ => Vec::new(),
                    };
                    let report = lsp_types::DocumentDiagnosticReportResult::Report(lsp_types::DocumentDiagnosticReport::Full(
                        lsp_types::RelatedFullDocumentDiagnosticReport {
                            related_documents: None,
                            full_document_diagnostic_report: lsp_types::FullDocumentDiagnosticReport { result_id: None, items },
                        },
                    ));
                    Ok(serde_json::to_value(report).unwrap())
                });
            }
            ExecuteCommand::METHOD => {
                let Ok((id, params)) = req.extract::<lsp_types::ExecuteCommandParams>(ExecuteCommand::METHOD) else { return };
                match params.command.as_str() {
                    COMMAND_RELOAD => {
                        self.start_load(true);
                        self.send(Response::new_ok(id, serde_json::Value::Null));
                    }
                    COMMAND_MODEL => {
                        let snap = self.host.snapshot();
                        self.send(Response::new_ok(id, handlers::project_model(&snap)));
                    }
                    other => self.send(Response::new_err(id, ErrorCode::InvalidParams as i32, format!("unknown command {other}"))),
                }
            }
            _ => self.send(Response::new_err(req.id, ErrorCode::MethodNotFound as i32, format!("unsupported request {}", req.method))),
        }
    }

    /// Run `f` on a worker with a fresh snapshot; the result is sent unless cancelled.
    fn spawn(&mut self, id: RequestId, f: impl FnOnce(&handlers::Ctx) -> Result<serde_json::Value, RequestError> + Send + 'static) {
        let snap = self.host.snapshot();
        let flag = handlers::new_flag();
        self.in_flight.insert(id.clone(), flag.clone());
        let snapshot_flag = self.snapshot_flag.clone();
        let enc = self.encoding;
        let tx = self.tasks_tx.clone();
        rayon::spawn(move || {
            let cx = handlers::Ctx { snap: &snap, enc, cancelled: &flag };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&cx)));
            let resp = match result {
                Ok(Ok(v)) => Response::new_ok(id, v),
                Ok(Err(RequestError::Cancelled)) if snapshot_flag.load(Ordering::Relaxed) => {
                    Response::new_err(id, ErrorCode::ContentModified as i32, "content modified".into())
                }
                Ok(Err(RequestError::Cancelled)) => Response::new_err(id, ErrorCode::RequestCanceled as i32, "cancelled".into()),
                Ok(Err(RequestError::Failed(msg))) => Response::new_err(id, ErrorCode::RequestFailed as i32, msg),
                Err(panic) => {
                    let msg = panic.downcast_ref::<String>().cloned().or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                    log::error!("request handler panicked: {msg}");
                    Response::new_err(id, ErrorCode::InternalError as i32, format!("internal error: {msg}"))
                }
            };
            let _ = tx.send(Task::Respond(resp));
        });
    }

    fn on_response(&mut self, resp: Response) {
        let Some(purpose) = self.outgoing.remove(&resp.id) else { return };
        match purpose {
            Outgoing::Configuration => {
                if let Some(first) = resp.response_result.as_ref().ok().and_then(|v| v.as_array()).and_then(|a| a.first())
                    && !first.is_null() {
                        let new = Config::from_json(first);
                        self.apply_config(new);
                    }
            }
            Outgoing::Other => {
                if let Err(err) = resp.response_result {
                    log::warn!("client returned error: {}", err.message);
                }
            }
        }
    }

    fn apply_config(&mut self, new: Config) {
        if new == self.config {
            return;
        }
        let reload = new.project != self.config.project || new.jdk_home != self.config.jdk_home || new.cache_dir != self.config.cache_dir;
        self.host.set_lint_config(new.lint.clone());
        self.config = new;
        if reload {
            self.start_load(false);
        } else {
            self.schedule_open_files(Duration::ZERO);
        }
    }

    // ------------------------------------------------------- notifications

    fn on_notification(&mut self, n: Notification) {
        use lsp_types::notification::*;
        match n.method.as_str() {
            Initialized::METHOD => self.on_initialized(),
            Cancel::METHOD => {
                if let Ok(p) = serde_json::from_value::<lsp_types::CancelParams>(n.params) {
                    let id: RequestId = match p.id {
                        lsp_types::NumberOrString::Number(i) => i.into(),
                        lsp_types::NumberOrString::String(s) => s.into(),
                    };
                    if let Some(flag) = self.in_flight.get(&id) {
                        flag.store(true, Ordering::Relaxed);
                    }
                }
            }
            DidOpenTextDocument::METHOD => {
                let Ok(p) = serde_json::from_value::<lsp_types::DidOpenTextDocumentParams>(n.params) else { return };
                let Some(path) = uri_to_path(&p.text_document.uri) else { return };
                self.docs.insert(path.clone(), OpenDoc { version: p.text_document.version, text: p.text_document.text.clone() });
                self.host.set_file(path.clone(), p.text_document.text.into(), Some(p.text_document.version));
                self.state_changed();
                self.schedule_diagnostics(&path, Duration::ZERO);
            }
            DidChangeTextDocument::METHOD => {
                let Ok(p) = serde_json::from_value::<lsp_types::DidChangeTextDocumentParams>(n.params) else { return };
                let Some(path) = uri_to_path(&p.text_document.uri) else { return };
                let Some(doc) = self.docs.get_mut(&path) else { return };
                line_index::apply_changes(&mut doc.text, p.content_changes, self.encoding);
                doc.version = p.text_document.version;
                let text: Arc<str> = doc.text.as_str().into();
                self.host.set_file(path.clone(), text, Some(p.text_document.version));
                self.state_changed();
                let delay = Duration::from_millis(self.config.diagnostics.debounce_ms);
                // Declarations may have changed for other open files too.
                self.schedule_open_files(delay * 2);
                // The edited file itself gets the shorter delay (push mode only).
                if !self.pull_diagnostics() && self.config.diagnostics.enabled {
                    self.diagnostics_due.insert(path.clone(), Instant::now() + delay);
                }
            }
            DidCloseTextDocument::METHOD => {
                let Ok(p) = serde_json::from_value::<lsp_types::DidCloseTextDocumentParams>(n.params) else { return };
                let Some(path) = uri_to_path(&p.text_document.uri) else { return };
                self.docs.remove(&path);
                // Fall back to the on-disk contents.
                match std::fs::read_to_string(&path) {
                    Ok(text) => self.host.set_file(path.clone(), text.into(), None),
                    Err(_) => self.host.remove_file(&path),
                }
                self.diagnostics_due.remove(&path);
                if !self.pull_diagnostics() {
                    self.publish(&path, None, Vec::new());
                }
                self.state_changed();
            }
            DidSaveTextDocument::METHOD => {
                let Ok(p) = serde_json::from_value::<lsp_types::DidSaveTextDocumentParams>(n.params) else { return };
                if let Some(path) = uri_to_path(&p.text_document.uri)
                    && is_build_file(&path) {
                        self.reload_due = Some(Instant::now() + Duration::from_millis(500));
                    }
            }
            DidChangeConfiguration::METHOD => {
                let Ok(p) = serde_json::from_value::<lsp_types::DidChangeConfigurationParams>(n.params) else { return };
                if self.supports_configuration_request() {
                    self.request_configuration();
                } else if !p.settings.is_null() {
                    let c = Config::from_json(&p.settings);
                    self.apply_config(c);
                }
            }
            DidChangeWatchedFiles::METHOD => {
                let Ok(p) = serde_json::from_value::<lsp_types::DidChangeWatchedFilesParams>(n.params) else { return };
                self.on_watched_files(p.changes);
            }
            DidChangeWorkspaceFolders::METHOD => {
                let Ok(p) = serde_json::from_value::<lsp_types::DidChangeWorkspaceFoldersParams>(n.params) else { return };
                for removed in p.event.removed {
                    if let Some(path) = uri_to_path(&removed.uri) {
                        self.folders.retain(|f| *f != path);
                    }
                }
                for added in p.event.added {
                    if let Some(path) = uri_to_path(&added.uri)
                        && !self.folders.contains(&path) {
                            self.folders.push(path);
                        }
                }
                self.start_load(false);
            }
            SetTrace::METHOD | WorkDoneProgressCancel::METHOD => {}
            _ => log::debug!("ignored notification {}", n.method),
        }
    }

    fn supports_configuration_request(&self) -> bool {
        self.caps.workspace.as_ref().and_then(|w| w.configuration).unwrap_or(false)
    }

    fn request_configuration(&mut self) {
        self.request::<lsp_types::request::WorkspaceConfiguration>(
            lsp_types::ConfigurationParams { items: vec![lsp_types::ConfigurationItem { scope_uri: None, section: Some("kotlin".into()) }] },
            Outgoing::Configuration,
        );
    }

    fn on_initialized(&mut self) {
        let dynamic_watch = self
            .caps
            .workspace
            .as_ref()
            .and_then(|w| w.did_change_watched_files.as_ref())
            .and_then(|d| d.dynamic_registration)
            .unwrap_or(false);
        if dynamic_watch {
            let watchers = ["**/*.kt", "**/*.kts", "**/*.java", "**/pom.xml", "**/*.gradle", "**/gradle.properties", "**/*.versions.toml"]
                .iter()
                .map(|g| lsp_types::FileSystemWatcher { glob_pattern: lsp_types::GlobPattern::String(g.to_string()), kind: None })
                .collect();
            let options = lsp_types::DidChangeWatchedFilesRegistrationOptions { watchers };
            self.request::<lsp_types::request::RegisterCapability>(
                lsp_types::RegistrationParams {
                    registrations: vec![lsp_types::Registration {
                        id: "kotlin-watched-files".into(),
                        method: lsp_types::notification::DidChangeWatchedFiles::METHOD.into(),
                        register_options: Some(serde_json::to_value(options).unwrap()),
                    }],
                },
                Outgoing::Other,
            );
        }
        if self.supports_configuration_request() {
            self.request_configuration();
        }
        self.start_load(false);
    }

    fn on_watched_files(&mut self, changes: Vec<lsp_types::FileEvent>) {
        let mut sources_changed = false;
        for change in changes {
            let Some(path) = uri_to_path(&change.uri) else { continue };
            if is_build_file(&path) {
                self.reload_due = Some(Instant::now() + Duration::from_millis(1500));
                continue;
            }
            if self.docs.contains_key(&path) || crate::analysis::Language::of(&path).is_none() {
                continue;
            }
            if change.typ == lsp_types::FileChangeType::DELETED {
                self.host.remove_file(&path);
            } else if let Ok(text) = std::fs::read_to_string(&path) {
                self.host.set_file(path, text.into(), None);
            }
            sources_changed = true;
        }
        if sources_changed {
            self.state_changed();
            self.schedule_open_files(Duration::from_millis(300));
        }
    }

    // --------------------------------------------------------------- tasks

    fn on_task(&mut self, task: Task) {
        match task {
            Task::Respond(resp) => {
                if self.in_flight.remove(&resp.id).is_some() {
                    self.send(resp);
                }
            }
            Task::Diagnostics { path, version, diagnostics, coverage } => {
                // Drop results for stale versions.
                let current = self.docs.get(&path).map(|d| d.version);
                if current.is_none() || current != version {
                    return;
                }
                if let Coverage::SyntaxOnly(reason) = &coverage
                    && !self.loading {
                        self.show_once(MessageType::INFO, format!("Kotlin: only syntax is checked for some files: {reason}"));
                    }
                self.publish(&path, version, diagnostics);
            }
            Task::Loaded(loaded) => self.on_loaded(*loaded),
            Task::Progress(ev) => self.on_progress(ev),
        }
    }

    fn on_progress(&mut self, ev: ProgressEvent) {
        let supported = self.caps.window.as_ref().and_then(|w| w.work_done_progress).unwrap_or(false);
        use lsp_types::{ProgressParams, ProgressParamsValue, WorkDoneProgress, WorkDoneProgressBegin, WorkDoneProgressEnd, WorkDoneProgressReport};
        let value = match ev {
            ProgressEvent::Begin(title) => {
                if supported {
                    let token = lsp_types::NumberOrString::String(format!("kotlin-import-{}", self.load_generation));
                    self.request::<lsp_types::request::WorkDoneProgressCreate>(lsp_types::WorkDoneProgressCreateParams { token: token.clone() }, Outgoing::Other);
                    self.progress_token = Some(token);
                }
                self.log(MessageType::INFO, title.clone());
                WorkDoneProgress::Begin(WorkDoneProgressBegin { title, cancellable: Some(false), message: None, percentage: None })
            }
            ProgressEvent::Report(message) => {
                log::info!("{message}");
                WorkDoneProgress::Report(WorkDoneProgressReport { cancellable: None, message: Some(message), percentage: None })
            }
            ProgressEvent::End(message) => {
                self.log(MessageType::INFO, message.clone());
                WorkDoneProgress::End(WorkDoneProgressEnd { message: Some(message) })
            }
        };
        if let Some(token) = &self.progress_token {
            let end = matches!(value, WorkDoneProgress::End(_));
            self.notify::<lsp_types::notification::Progress>(ProgressParams { token: token.clone(), value: ProgressParamsValue::WorkDone(value) });
            if end {
                self.progress_token = None;
            }
        }
    }

    fn start_load(&mut self, force: bool) {
        if self.loading {
            self.pending_reload = true;
            return;
        }
        self.loading = true;
        self.load_generation += 1;
        let folders = self.folders.clone();
        let config = self.config.clone();
        let generation = self.load_generation;
        let tx = self.tasks_tx.clone();
        std::thread::Builder::new()
            .name("project-loader".into())
            .spawn(move || {
                let tx2 = tx.clone();
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| loader::load(folders, config, generation, force, tx)));
                if r.is_err() {
                    log::error!("project loading panicked");
                    let _ = tx2.send(Task::Progress(ProgressEvent::End("Project import failed".into())));
                    let _ = tx2.send(Task::Loaded(Box::new(Loaded {
                        generation,
                        workspace: crate::analysis::Workspace::empty(),
                        files: Vec::new(),
                        notes: vec!["Project import failed unexpectedly; see the server log".into()],
                    })));
                }
            })
            .expect("spawn loader thread");
    }

    fn on_loaded(&mut self, loaded: Loaded) {
        self.loading = false;
        if loaded.generation != self.load_generation {
            return;
        }
        self.host.set_workspace(loaded.workspace);
        for f in loaded.files {
            // Open documents keep their editor contents.
            if !self.docs.contains_key(&f.path) {
                self.host.insert_file(f);
            }
        }
        for note in &loaded.notes {
            self.log(MessageType::WARNING, note.clone());
        }
        if !loaded.notes.is_empty() {
            let n = loaded.notes.len();
            self.show_once(
                MessageType::WARNING,
                format!("Kotlin: {n} problem(s) while importing the project; semantic checks may be limited. See the output log for details."),
            );
        }
        self.state_changed();
        if self.pull_diagnostics() {
            let refresh = self.caps.workspace.as_ref().and_then(|w| w.diagnostic.as_ref()).and_then(|d| d.refresh_support).unwrap_or(false);
            if refresh {
                self.request::<lsp_types::request::WorkspaceDiagnosticRefresh>((), Outgoing::Other);
            }
        } else {
            self.schedule_open_files(Duration::ZERO);
        }
        if std::mem::take(&mut self.pending_reload) {
            self.start_load(true);
        }
    }

    // ----------------------------------------------------------- scheduling

    /// Cancel work running against outdated snapshots.
    fn state_changed(&mut self) {
        self.snapshot_flag.store(true, Ordering::Relaxed);
        for flag in self.in_flight.values() {
            flag.store(true, Ordering::Relaxed);
        }
        self.snapshot_flag = handlers::new_flag();
    }

    fn schedule_diagnostics(&mut self, path: &Path, delay: Duration) {
        if self.pull_diagnostics() || !self.config.diagnostics.enabled {
            return;
        }
        let due = Instant::now() + delay;
        let e = self.diagnostics_due.entry(path.to_path_buf()).or_insert(due);
        *e = due.max(*e);
        if delay.is_zero() {
            *e = due;
        }
    }

    fn schedule_open_files(&mut self, delay: Duration) {
        let paths: Vec<PathBuf> = self.docs.keys().cloned().collect();
        for p in paths {
            self.schedule_diagnostics(&p, delay);
        }
    }

    fn run_due(&mut self) {
        let now = Instant::now();
        if self.reload_due.is_some_and(|d| d <= now) {
            self.reload_due = None;
            self.start_load(true);
        }
        let due: Vec<PathBuf> = self.diagnostics_due.iter().filter(|(_, d)| **d <= now).map(|(p, _)| p.clone()).collect();
        if due.is_empty() {
            return;
        }
        for p in &due {
            self.diagnostics_due.remove(p);
        }
        let snap = self.host.snapshot();
        let flag = self.snapshot_flag.clone();
        let enc = self.encoding;
        for path in due {
            let Some(version) = self.docs.get(&path).map(|d| d.version) else { continue };
            let snap: Snapshot = snap.clone();
            let flag = flag.clone();
            let tx = self.tasks_tx.clone();
            rayon::spawn(move || {
                let cx = handlers::Ctx { snap: &snap, enc, cancelled: &flag };
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handlers::file_diagnostics(&cx, &path)));
                match r {
                    Ok(Ok((diagnostics, coverage))) => {
                        let _ = tx.send(Task::Diagnostics { path, version: Some(version), diagnostics, coverage });
                    }
                    Ok(Err(Cancelled)) => {}
                    Err(_) => log::error!("diagnostics panicked for {}", path.display()),
                }
            });
        }
    }

    fn publish(&self, path: &Path, version: Option<i32>, diagnostics: Vec<lsp_types::Diagnostic>) {
        self.notify::<lsp_types::notification::PublishDiagnostics>(lsp_types::PublishDiagnosticsParams {
            uri: path_to_uri(path),
            diagnostics,
            version,
        });
    }

    fn path_of(&self, uri: &Uri) -> Option<PathBuf> {
        uri_to_path(uri)
    }
}

pub fn is_build_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    name == "pom.xml"
        || name.ends_with(".gradle")
        || name.ends_with(".gradle.kts")
        || name == "gradle.properties"
        || name.ends_with(".versions.toml")
        || name == "settings.xml"
}

#[cfg(test)]
mod tests;
