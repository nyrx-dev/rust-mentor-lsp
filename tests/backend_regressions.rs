use rust_mentor_lsp::{Backend, CachedAnalysis, context::slice_from_str};
use tower_lsp::{LanguageServer, LspService, lsp_types::*};

fn backend() -> Backend {
    let (service, _) = LspService::new(|client| {
        let mut b = Backend::new(client);
        b.gemini = None;
        b.jev = None;
        b
    });
    service.inner().clone()
}

async fn open(b: &Backend, uri: &Url, text: &str) {
    b.did_open(DidOpenTextDocumentParams {
        text_document: TextDocumentItem {
            uri: uri.clone(),
            language_id: "rust".into(),
            version: 7,
            text: text.into(),
        },
    })
    .await;
}

fn seed(b: &Backend, uri: &Url, text: &str, line: u32, fix: &str) {
    let ctx = slice_from_str(text, line, "E0382");
    b.cache.insert(
        (uri.to_string(), "E0382".into(), ctx.context_hash),
        CachedAnalysis {
            error_code: "E0382".into(),
            explanation: "Explanation".into(),
            verified_refactor: Some(fix.into()),
        },
    );
    b.line_index
        .insert((uri.to_string(), line), ("E0382".into(), ctx.context_hash));
}

#[tokio::test]
async fn changes_and_close_clear_cached_analysis() {
    let b = backend();
    let uri = Url::parse("file:///tmp/mentor-cache-test.rs").unwrap();
    open(&b, &uri, "fn f() {}").await;
    seed(&b, &uri, "fn f() {}", 0, "fn f() {}");
    b.did_change(DidChangeTextDocumentParams {
        text_document: VersionedTextDocumentIdentifier {
            uri: uri.clone(),
            version: 8,
        },
        content_changes: vec![TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: "fn f() { }".into(),
        }],
    })
    .await;
    assert!(b.cache.is_empty());
    assert!(b.line_index.is_empty());
    seed(&b, &uri, "fn f() { }", 0, "fn f() {}");
    b.did_close(DidCloseTextDocumentParams {
        text_document: TextDocumentIdentifier { uri },
    })
    .await;
    assert!(b.cache.is_empty());
}

#[tokio::test]
async fn edits_include_document_version() {
    let b = backend();
    let uri = Url::parse("file:///tmp/mentor-version-test.rs").unwrap();
    open(&b, &uri, "fn f() {}").await;
    seed(&b, &uri, "fn f() {}", 0, "fn f() {}");
    let params = serde_json::from_value(serde_json::json!({
        "textDocument": {"uri": uri},
        "range": {"start": {"line":0,"character":0}, "end": {"line":0,"character":0}},
        "context": {"diagnostics": []}
    }))
    .unwrap();
    let actions = b.code_action(params).await.unwrap().unwrap();
    let json = serde_json::to_value(actions).unwrap();
    let edit = &json[0]["edit"];
    assert_eq!(edit["documentChanges"][0]["textDocument"]["version"], 7);
    assert!(edit.get("changes").is_none());
}
