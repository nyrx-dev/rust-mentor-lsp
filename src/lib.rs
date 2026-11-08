use dashmap::DashMap;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
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
        let uri = params.text_document.uri.to_string();
        self.documents
            .insert(uri.clone(), params.text_document.text);
        self.spawn_background_analysis(uri);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        if let Some(change) = params.content_changes.into_iter().last() {
            self.documents.insert(uri, change.text);
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        self.documents.remove(&uri);
        self.line_index.retain(|(u, _), _| u != &uri);
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
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
        let mut actions = Vec::new();
        let mut seen_codes = HashSet::new();
        let uri_url = params.text_document.uri;
        let uri = uri_url.to_string();

        for diag in params.context.diagnostics {
            let error_code = match &diag.code {
                Some(NumberOrString::String(s)) => s.clone(),
                Some(NumberOrString::Number(n)) => n.to_string(),
                None => continue,
            };

            if !filter::is_candidate_error_code(&error_code)
                || !seen_codes.insert(error_code.clone())
            {
                continue;
            }

            let line = diag.range.start.line;
            let sliced = context::slice_context(&uri, line, &error_code, &self.documents);

            if let Some(ref ctx) = sliced
                && let Some(cached) =
                    self.cache
                        .get(&(uri.clone(), error_code.clone(), ctx.context_hash))
            {
                self.line_index
                    .insert((uri.clone(), line), (error_code.clone(), ctx.context_hash));

                if let (Some(fn_range), Some(refactored_code)) =
                    (ctx.function_range, &cached.verified_refactor)
                {
                    let mut changes = HashMap::new();
                    changes.insert(
                        uri_url.clone(),
                        vec![TextEdit {
                            range: fn_range,
                            new_text: refactored_code.clone(),
                        }],
                    );

                    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                        title: format!("Apply Verified Rust Mentor Refactor ({})", error_code),
                        kind: Some(CodeActionKind::QUICKFIX),
                        diagnostics: Some(vec![diag.clone()]),
                        edit: Some(WorkspaceEdit {
                            changes: Some(changes),
                            ..Default::default()
                        }),
                        is_preferred: Some(true),
                        ..Default::default()
                    }));
                }

                let show_title = format!("Show Cached Rust Mentor Explanation ({})", error_code);
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: show_title.clone(),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![diag]),
                    command: Some(Command {
                        title: show_title,
                        command: "rust_mentor.show_cached".to_string(),
                        arguments: None,
                    }),
                    ..Default::default()
                }));
                continue;
            }

            let action_title = format!("Explain error {} with Rust Mentor", error_code);

            let full_message = diag
                .data
                .as_ref()
                .and_then(|d| d.get("rendered"))
                .and_then(|r| r.as_str())
                .unwrap_or(&diag.message)
                .to_string();

            let cmd = Command {
                title: action_title.clone(),
                command: "rust_mentor.explain".to_string(),
                arguments: Some(vec![
                    serde_json::Value::String(uri.clone()),
                    serde_json::Value::Number(serde_json::Number::from(line)),
                    serde_json::Value::String(error_code),
                    serde_json::Value::String(full_message),
                ]),
            };

            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: action_title,
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![diag]),
                command: Some(cmd),
                ..Default::default()
            }));
        }

        if actions.is_empty() {
            let cursor_line = params.range.start.line;
            if let Some(index_ref) = self.line_index.get(&(uri.clone(), cursor_line)) {
                let (error_code, hash) = index_ref.value().clone();
                if let Some(cached) = self.cache.get(&(uri.clone(), error_code.clone(), hash))
                    && let Some(ctx) =
                        context::slice_context(&uri, cursor_line, &error_code, &self.documents)
                    && ctx.context_hash == hash
                {
                    if let (Some(fn_range), Some(refactored_code)) =
                        (ctx.function_range, &cached.verified_refactor)
                    {
                        let mut changes = HashMap::new();
                        changes.insert(
                            uri_url.clone(),
                            vec![TextEdit {
                                range: fn_range,
                                new_text: refactored_code.clone(),
                            }],
                        );

                        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                            title: format!("Apply Verified Rust Mentor Refactor ({})", error_code),
                            kind: Some(CodeActionKind::QUICKFIX),
                            edit: Some(WorkspaceEdit {
                                changes: Some(changes),
                                ..Default::default()
                            }),
                            is_preferred: Some(true),
                            ..Default::default()
                        }));
                    }

                    let show_title =
                        format!("Show Cached Rust Mentor Explanation ({})", error_code);
                    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                        title: show_title.clone(),
                        kind: Some(CodeActionKind::QUICKFIX),
                        command: Some(Command {
                            title: show_title,
                            command: "rust_mentor.show_cached".to_string(),
                            arguments: None,
                        }),
                        ..Default::default()
                    }));
                }
            }
        }

        if actions.is_empty() {
            Ok(None)
        } else {
            Ok(Some(actions))
        }
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
        }
    }

    pub fn spawn_background_analysis(&self, uri: String) {
        if self.gemini.is_none() {
            return;
        }
        let backend = self.clone();
        tokio::spawn(async move {
            let errors = verifier::discover_file_errors(&uri).await;
            if !errors.is_empty() {
                backend.line_index.retain(|(u, _), _| u != &uri);
            }
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
        let Some(sliced) = context::slice_context(uri, line, error_code, &self.documents) else {
            return false;
        };

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

        let _permit = self.semaphore.acquire().await.ok();

        let should_ask = match &self.jev {
            Some(jev) => filter::should_explain(jev, error_code, message).await,
            None => true,
        };

        if !should_ask {
            self.in_flight.remove(&key);
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

        self.in_flight.remove(&key);

        let Ok(mentor_resp) = res else {
            return false;
        };

        let verified_refactor = if let Some(raw_fn) = &mentor_resp.refactored_function {
            verifier::verify_refactor(
                &sliced.file_content,
                sliced.start_line,
                sliced.end_line,
                &sliced.base_indent,
                raw_fn,
            )
            .await
        } else {
            None
        };

        let entry = CachedAnalysis {
            error_code: error_code.to_string(),
            explanation: mentor_resp.explanation,
            verified_refactor,
        };

        self.cache
            .retain(|(u, c, h), _| u != uri || c != error_code || *h == sliced.context_hash);
        self.cache.insert(key, entry);
        self.line_index.insert(
            (uri.to_string(), line),
            (error_code.to_string(), sliced.context_hash),
        );

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

        let dev_path =
            std::path::Path::new(&home).join("projects/personal/LearnRust/lsp_server/.env");
        dotenvy::from_path(dev_path).ok();
    }

    let input = tokio::io::stdin();
    let output = tokio::io::stdout();
    let (service, socket) = tower_lsp::LspService::new(Backend::new);
    tower_lsp::Server::new(input, output, socket)
        .serve(service)
        .await;
}
