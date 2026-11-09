use dashmap::DashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;
use tower_lsp::{
    Client, LanguageServer,
    jsonrpc::Result,
    lsp_types::{
        CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams,
        CodeActionProviderCapability, CodeActionResponse, Command, DidChangeTextDocumentParams,
        DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
        ExecuteCommandOptions, ExecuteCommandParams, Hover, HoverContents, HoverParams,
        HoverProviderCapability, InitializeParams, InitializeResult, MarkupContent, MarkupKind,
        MessageType, NumberOrString, ServerCapabilities, TextDocumentSyncCapability,
        TextDocumentSyncKind, TextEdit, WorkspaceEdit,
    },
};

pub mod context;
pub mod filter;
pub mod gemini;
pub mod verifier;

#[derive(Clone, Debug)]
pub struct CachedAnalysis {
    pub error_code: String,
    pub explanation: String,
    pub verified_refactor: Option<String>,
}

#[derive(Clone)]
pub struct Backend {
    pub client: Client,
    pub jev: Option<jev::AsyncTypeSafeClient>,
    pub gemini: Option<gemini::GeminiClient>,
    pub documents: Arc<DashMap<String, String>>,
    pub cache: Arc<DashMap<(String, String, u64), CachedAnalysis>>,
    pub line_index: Arc<DashMap<(String, u32), (String, u64)>>,
    pub in_flight: Arc<DashMap<(String, String, u64), ()>>,
    pub semaphore: Arc<Semaphore>,
    pub versions: Arc<DashMap<String, i32>>,
    pub revision: Arc<AtomicU64>,
    state_lock: Arc<Mutex<()>>,
}

struct InFlightGuard {
    map: Arc<DashMap<(String, String, u64), ()>>,
    key: (String, String, u64),
}
impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.map.remove(&self.key);
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![
                        "rust_mentor.explain".to_string(),
                        "rust_mentor.show_cached".to_string(),
                        "rust_mentor.cache_diagnostics".to_string(),
                    ],
                    ..Default::default()
                }),
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                ..Default::default()
            },
            server_info: None,
        })
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let _state = self.state_lock.lock().unwrap();
        let uri = params.text_document.uri.to_string();
        self.invalidate_analysis();
        self.versions
            .insert(uri.clone(), params.text_document.version);
        self.documents
            .insert(uri.clone(), params.text_document.text);
        self.spawn_background_analysis(uri);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let _state = self.state_lock.lock().unwrap();
        let uri = params.text_document.uri.to_string();
        if let Some(change) = params.content_changes.into_iter().last() {
            self.invalidate_analysis();
            self.versions
                .insert(uri.clone(), params.text_document.version);
            self.documents.insert(uri, change.text);
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let _state = self.state_lock.lock().unwrap();
        let uri = params.text_document.uri.to_string();
        self.invalidate_analysis();
        self.versions.remove(&uri);
        self.documents.remove(&uri);
        self.line_index.retain(|(u, _), _| u != &uri);
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let _state = self.state_lock.lock().unwrap();
        self.invalidate_analysis();
        let uri = params.text_document.uri.to_string();
        if let Some(text) = params.text {
            self.documents.insert(uri.clone(), text);
        }
        self.spawn_background_analysis(uri);
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let position = params.text_document_position_params.position;
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .to_string();

        if let Some(index_ref) = self.line_index.get(&(uri.clone(), position.line)) {
            let (code, hash) = index_ref.value().clone();
            if let Some(entry) = self.cache.get(&(uri.clone(), code.clone(), hash))
                && let Some(sliced) =
                    context::slice_context(&uri, position.line, &code, &self.documents)
                && sliced.context_hash == hash
            {
                return Ok(Some(build_hover(
                    &entry.explanation,
                    entry.verified_refactor.as_deref(),
                )));
            }
        }

        Ok(None)
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri_url = params.text_document.uri;
        let uri = uri_url.to_string();
        let (revision, version) = {
            let _state = self.state_lock.lock().unwrap();
            (
                self.revision.load(Ordering::SeqCst),
                self.versions.get(&uri).map(|v| *v),
            )
        };
        let mut requested = Vec::new();
        let mut seen = HashSet::new();
        for diag in params.context.diagnostics {
            let code = match &diag.code {
                Some(NumberOrString::String(s)) => s.clone(),
                Some(NumberOrString::Number(n)) => n.to_string(),
                None => continue,
            };
            let line = diag.range.start.line;
            if filter::is_candidate_error_code(&code) && seen.insert((code.clone(), line)) {
                requested.push((code, line, Some(diag)));
            }
        }
        if requested.is_empty()
            && let Some(index) = self.line_index.get(&(uri.clone(), params.range.start.line))
        {
            requested.push((index.value().0.clone(), params.range.start.line, None));
        }
        let mut actions = Vec::new();
        for (code, line, diag) in requested {
            let Some(ctx) = context::slice_context(&uri, line, &code, &self.documents) else {
                continue;
            };
            let cached = self
                .cache
                .get(&(uri.clone(), code.clone(), ctx.context_hash))
                .map(|entry| entry.value().clone());
            if let Some(cached) = cached {
                // Recheck on request: files outside this buffer may have changed on disk.
                if let (Some(range), Some(raw), Some(version)) =
                    (ctx.function_range, cached.verified_refactor, version)
                    && let Some(replacement) =
                        verifier::verify_context(&uri, &ctx, &raw, self.document_snapshot()).await
                {
                    use tower_lsp::lsp_types::{
                        DocumentChanges, OneOf, OptionalVersionedTextDocumentIdentifier,
                        TextDocumentEdit,
                    };
                    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                        title: format!("Apply Verified Rust Mentor Refactor ({code})"),
                        kind: Some(CodeActionKind::QUICKFIX),
                        diagnostics: diag.clone().map(|d| vec![d]),
                        edit: Some(WorkspaceEdit {
                            document_changes: Some(DocumentChanges::Edits(vec![
                                TextDocumentEdit {
                                    text_document: OptionalVersionedTextDocumentIdentifier {
                                        uri: uri_url.clone(),
                                        version: Some(version),
                                    },
                                    edits: vec![OneOf::Left(TextEdit {
                                        range,
                                        new_text: replacement,
                                    })],
                                },
                            ])),
                            ..Default::default()
                        }),
                        is_preferred: Some(false),
                        ..Default::default()
                    }));
                }
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Show Cached Rust Mentor Explanation ({code})"),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: diag.map(|d| vec![d]),
                    command: Some(Command {
                        title: "Show explanation".into(),
                        command: "rust_mentor.show_cached".into(),
                        arguments: None,
                    }),
                    ..Default::default()
                }));
            } else if let Some(diag) = diag {
                let message = diag
                    .data
                    .as_ref()
                    .and_then(|d| d.get("rendered"))
                    .and_then(|r| r.as_str())
                    .unwrap_or(&diag.message)
                    .to_string();
                let title = format!("Explain error {code} with Rust Mentor");
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: title.clone(),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![diag]),
                    command: Some(Command {
                        title,
                        command: "rust_mentor.explain".into(),
                        arguments: Some(vec![
                            uri.clone().into(),
                            line.into(),
                            code.into(),
                            message.into(),
                        ]),
                    }),
                    ..Default::default()
                }));
            }
        }
        if self.revision.load(Ordering::SeqCst) != revision {
            return Ok(None);
        }
        Ok((!actions.is_empty()).then_some(actions))
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<serde_json::Value>> {
        if params.command == "rust_mentor.show_cached" {
            return Ok(Some(serde_json::Value::Bool(true)));
        }

        if params.command == "rust_mentor.cache_diagnostics" {
            let mut args = params.arguments.into_iter();
            let uri = args
                .next()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default();
            let Some(diags) = args.next().and_then(|v| v.as_array().cloned()) else {
                return Ok(None);
            };

            self.line_index.retain(|(u, _), _| u != &uri);

            for d in diags {
                let Some(code) = d.get("code").and_then(|c| c.as_str()).map(String::from) else {
                    continue;
                };
                if !filter::is_candidate_error_code(&code) {
                    continue;
                }
                let line = d.get("line").and_then(|l| l.as_u64()).unwrap_or(0) as u32;
                let end_line = d
                    .get("end_line")
                    .and_then(|l| l.as_u64())
                    .map(|l| l as u32)
                    .unwrap_or(line);
                let message = d
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or_default()
                    .to_string();

                if let Some(sliced) = context::slice_context(&uri, line, &code, &self.documents) {
                    for l in line..=end_line.min(line + 10) {
                        self.line_index
                            .insert((uri.clone(), l), (code.clone(), sliced.context_hash));
                    }
                }

                let b = self.clone();
                let u = uri.clone();
                tokio::spawn(async move {
                    b.analyze_and_cache_single(&u, line, &code, &message).await;
                });
            }
            return Ok(None);
        }

        if params.command != "rust_mentor.explain" {
            return Ok(None);
        }

        let mut args = params.arguments.into_iter();
        let uri = args
            .next()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        let line = args.next().and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let error_code = args
            .next()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        let message = args
            .next()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();

        if self.gemini.is_none() {
            self.client
                .show_message(
                    MessageType::ERROR,
                    "Rust Mentor: GEMINI_API_KEY is not configured.",
                )
                .await;
            return Ok(None);
        };

        self.client
            .show_message(
                MessageType::INFO,
                format!("Rust Mentor: Analyzing {}...", error_code),
            )
            .await;

        if self
            .analyze_and_cache_single(&uri, line, &error_code, &message)
            .await
        {
            return Ok(Some(serde_json::Value::Bool(true)));
        }

        Ok(None)
    }
}

fn build_hover(explanation: &str, verified_refactor: Option<&str>) -> Hover {
    let value = match verified_refactor {
        Some(code) => format!(
            "{}\n\n```rust\n{}\n```\n*`<leader>ca` to apply refactor*",
            explanation.trim(),
            code.trim()
        ),
        None => explanation.trim().to_string(),
    };

    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: None,
    }
}

impl Backend {
    fn invalidate_analysis(&self) {
        self.revision.fetch_add(1, Ordering::SeqCst);
        self.cache.clear();
        self.line_index.clear();
    }

    fn document_snapshot(&self) -> Vec<(String, String)> {
        let _state = self.state_lock.lock().unwrap();
        self.documents
            .iter()
            .map(|d| (d.key().clone(), d.value().clone()))
            .collect()
    }

    pub fn new(client: Client) -> Self {
        let jev = jev::AsyncTypeSafeClient::from_env().ok();
        let gemini = gemini::GeminiClient::from_env().ok();

        Backend {
            client,
            jev,
            gemini,
            documents: Arc::new(DashMap::new()),
            cache: Arc::new(DashMap::new()),
            line_index: Arc::new(DashMap::new()),
            in_flight: Arc::new(DashMap::new()),
            semaphore: Arc::new(Semaphore::new(2)),
            versions: Arc::new(DashMap::new()),
            revision: Arc::new(AtomicU64::new(0)),
            state_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn spawn_background_analysis(&self, uri: String) {
        if self.gemini.is_none() {
            return;
        }
        let backend = self.clone();
        let revision = self.revision.load(Ordering::SeqCst);
        tokio::spawn(async move {
            let errors = verifier::discover_file_errors(&uri).await;
            if backend.revision.load(Ordering::SeqCst) != revision {
                return;
            }
            backend.line_index.retain(|(u, _), _| u != &uri);
            for err in errors {
                let b = backend.clone();
                let u = uri.clone();
                tokio::spawn(async move {
                    b.analyze_and_cache_single(&u, err.line_zero_based, &err.code, &err.rendered)
                        .await;
                });
            }
        });
    }

    pub async fn analyze_and_cache_single(
        &self,
        uri: &str,
        line: u32,
        error_code: &str,
        message: &str,
    ) -> bool {
        let Some(gemini) = &self.gemini else {
            return false;
        };
        let (revision, sliced, key) = {
            let _state = self.state_lock.lock().unwrap();
            let revision = self.revision.load(Ordering::SeqCst);
            let Some(sliced) = context::slice_context(uri, line, error_code, &self.documents)
            else {
                return false;
            };
            if !self.documents.contains_key(uri) {
                return false;
            }
            let key = (uri.to_string(), error_code.to_string(), sliced.context_hash);
            self.line_index.insert(
                (uri.to_string(), line),
                (error_code.to_string(), sliced.context_hash),
            );
            if self.cache.contains_key(&key) {
                return true;
            }
            if self.in_flight.insert(key.clone(), ()).is_some() {
                return false;
            }
            (revision, sliced, key)
        };

        let _flight = InFlightGuard {
            map: self.in_flight.clone(),
            key: key.clone(),
        };
        let Ok(_permit) = self.semaphore.acquire().await else {
            return false;
        };
        if self.revision.load(Ordering::SeqCst) != revision {
            return false;
        }

        let should_ask = match &self.jev {
            Some(jev) => filter::should_explain(jev, error_code, message).await,
            None => true,
        };

        if !should_ask {
            return false;
        }

        let res = gemini
            .analyze_error(
                error_code,
                message,
                &sliced.prompt_context,
                &sliced.function_code,
            )
            .await;

        let Ok(mentor_resp) = res else {
            return false;
        };

        let verified_refactor = if let Some(raw_fn) = &mentor_resp.refactored_function {
            verifier::verify_context(uri, &sliced, raw_fn, self.document_snapshot()).await
        } else {
            None
        };

        let entry = CachedAnalysis {
            error_code: error_code.to_string(),
            explanation: mentor_resp.explanation,
            verified_refactor,
        };

        self.publish_analysis(revision, key, line, entry)
    }

    fn publish_analysis(
        &self,
        revision: u64,
        key: (String, String, u64),
        line: u32,
        entry: CachedAnalysis,
    ) -> bool {
        let _state = self.state_lock.lock().unwrap();
        if self.revision.load(Ordering::SeqCst) != revision {
            return false;
        }
        let (uri, code, hash) = key.clone();
        self.cache.insert(key, entry);
        self.line_index.insert((uri, line), (code, hash));
        true
    }
}

pub async fn run_server() {
    dotenvy::dotenv().ok();

    if let Ok(exe_path) = std::env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        dotenvy::from_path(exe_dir.join(".env")).ok();
    }

    if let Ok(home) = std::env::var("HOME") {
        let global_config = std::path::Path::new(&home).join(".config/rust-mentor/.env");
        dotenvy::from_path(global_config).ok();
    }

    let input = tokio::io::stdin();
    let output = tokio::io::stdout();
    let (service, socket) = tower_lsp::LspService::new(Backend::new);
    tower_lsp::Server::new(input, output, socket)
        .serve(service)
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    fn backend() -> Backend {
        let (service, _) = tower_lsp::LspService::new(Backend::new);
        service.inner().clone()
    }
    fn entry() -> CachedAnalysis {
        CachedAnalysis {
            error_code: "E0382".into(),
            explanation: "why".into(),
            verified_refactor: None,
        }
    }
    #[test]
    fn completing_one_function_does_not_evict_another() {
        let b = backend();
        assert!(b.publish_analysis(
            0,
            ("file:///tmp/f.rs".into(), "E0382".into(), 1),
            0,
            entry()
        ));
        assert!(b.publish_analysis(
            0,
            ("file:///tmp/f.rs".into(), "E0382".into(), 2),
            5,
            entry()
        ));
        assert_eq!(b.cache.len(), 2);
    }
    #[test]
    fn stale_background_result_cannot_repopulate_invalidated_cache() {
        let b = backend();
        b.invalidate_analysis();
        assert!(!b.publish_analysis(
            0,
            ("file:///tmp/f.rs".into(), "E0382".into(), 1),
            0,
            entry()
        ));
        assert!(b.cache.is_empty());
        assert!(b.line_index.is_empty());
    }
    #[test]
    fn cancelled_analysis_releases_in_flight_slot() {
        let map = Arc::new(DashMap::new());
        let key = ("file:///tmp/f.rs".into(), "E0382".into(), 1);
        map.insert(key.clone(), ());
        {
            let _guard = InFlightGuard {
                map: map.clone(),
                key,
            };
        }
        assert!(map.is_empty());
    }
}
