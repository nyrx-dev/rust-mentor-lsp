use crate::filter;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static RMETA_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_temp_rmeta_path(prefix: &str) -> String {
    let id = RMETA_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("/tmp/rust_mentor_{}_{}_{}.rmeta", prefix, pid, id)
}

#[derive(Clone, Debug)]
pub struct DiscoveredError {
    pub code: String,
    pub line_zero_based: u32,
    pub rendered: String,
}

pub async fn discover_file_errors(uri_str: &str) -> Vec<DiscoveredError> {
    let Ok(url) = tower_lsp::lsp_types::Url::parse(uri_str) else {
        return Vec::new();
    };
    let Ok(path) = url.to_file_path() else {
        return Vec::new();
    };

    tokio::task::spawn_blocking(move || discover_file_errors_sync(&path))
        .await
        .unwrap_or_default()
}

fn discover_file_errors_sync(path: &Path) -> Vec<DiscoveredError> {
    let mut results = Vec::new();
    let mut had_unresolved_import = false;
    let rmeta_path = next_temp_rmeta_path("discover");

    if let Ok(output) = Command::new("rustc")
        .args([
            "--edition=2024",
            "--crate-type=lib",
            "--emit=metadata",
            "-o",
            &rmeta_path,
            "--error-format=json",
        ])
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
    {
        let _ = std::fs::remove_file(&rmeta_path);
        let stderr = String::from_utf8_lossy(&output.stderr);
        for line in stderr.lines() {
            let Ok(json) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if json.get("level").and_then(|l| l.as_str()) != Some("error") {
                continue;
            }
            let Some(code) = json
                .get("code")
                .and_then(|c| c.get("code"))
                .and_then(|c| c.as_str())
            else {
                continue;
            };

            if matches!(code, "E0432" | "E0433") {
                had_unresolved_import = true;
            }

            if !filter::is_candidate_error_code(code) {
                continue;
            }

            if let Some(err) = parse_diagnostic_json(&json, code) {
                results.push(err);
            }
        }
    } else {
        let _ = std::fs::remove_file(&rmeta_path);
    }

    if results.is_empty()
        && had_unresolved_import
        && let Some(parent) = path.parent()
        && let Ok(output) = Command::new("cargo")
            .args([
                "check",
                "--message-format=json",
                "--all-targets",
                "--target-dir",
                "/tmp/rust_mentor_target",
            ])
            .current_dir(parent)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        for line in stdout.lines() {
            let Ok(wrapper) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if wrapper.get("reason").and_then(|r| r.as_str()) != Some("compiler-message") {
                continue;
            }
            let Some(json) = wrapper.get("message") else {
                continue;
            };
            let Some(code) = json
                .get("code")
                .and_then(|c| c.get("code"))
                .and_then(|c| c.as_str())
            else {
                continue;
            };
            if !filter::is_candidate_error_code(code) {
                continue;
            }
            let matches_file = json
                .get("spans")
                .and_then(|s| s.as_array())
                .is_some_and(|spans| {
                    spans.iter().any(|sp| {
                        sp.get("file_name")
                            .and_then(|f| f.as_str())
                            .is_some_and(|f| f.ends_with(file_name))
                    })
                });
            if matches_file && let Some(err) = parse_diagnostic_json(json, code) {
                results.push(err);
            }
        }
    }

    results
}

fn parse_diagnostic_json(json: &serde_json::Value, code: &str) -> Option<DiscoveredError> {
    let spans = json.get("spans")?.as_array()?;
    let span = spans
        .iter()
        .find(|s| s.get("is_primary").and_then(|p| p.as_bool()) == Some(true))
        .or_else(|| spans.first())?;

    let line_one = span.get("line_start")?.as_u64()? as u32;
    let line_zero_based = line_one.saturating_sub(1);
    let rendered = json
        .get("rendered")
        .and_then(|r| r.as_str())
        .or_else(|| json.get("message").and_then(|m| m.as_str()))
        .unwrap_or_default()
        .to_string();

    Some(DiscoveredError {
        code: code.to_string(),
        line_zero_based,
        rendered,
    })
}

pub async fn verify_refactor(
    original_file: &str,
    start_line_zero: u32,
    end_line_zero: u32,
    base_indent: &str,
    raw_refactor: &str,
) -> Option<String> {
    let mut context = crate::context::slice_from_str(original_file, start_line_zero, "");
    let range = context.byte_range.clone()?;
    if context.start_line != start_line_zero || context.end_line != end_line_zero {
        return None;
    }
    // This compatibility API describes whole-line edits, so reject inline neighbors.
    if !original_file[..range.start]
        .rsplit('\n')
        .next()?
        .trim()
        .is_empty()
        || !original_file[range.end..]
            .split('\n')
            .next()?
            .trim()
            .is_empty()
    {
        return None;
    }
    context.base_indent = base_indent.to_string();
    let raw = raw_refactor.to_string();
    tokio::task::spawn_blocking(move || {
        let (replacement, spliced, probe) = prepare_refactor(&context, &raw)?;
        (check_with_rustc(&spliced, 0, 0) && confirms_probe(&probe, "rustc"))
            .then(|| format!("{}{}", context.base_indent, replacement))
    })
    .await
    .ok()
    .flatten()
}

fn format_with_rustfmt(code: &str) -> String {
    let Ok(mut child) = Command::new("rustfmt")
        .args(["--edition", "2024"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return code.to_string();
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(code.as_bytes());
    }

    if let Ok(output) = child.wait_with_output()
        && output.status.success()
    {
        let formatted = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !formatted.is_empty() {
            return formatted;
        }
    }

    code.to_string()
}

fn is_fn_signature_start(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("fn ")
        || t.starts_with("pub ")
        || t.starts_with("pub(")
        || t.starts_with("async ")
        || t.starts_with("unsafe ")
        || t.starts_with("const ")
        || t.starts_with("extern ")
}

fn strip_code_fences_and_attrs(input: &str) -> String {
    let trimmed = input.trim();
    let mut lines: Vec<&str> = trimmed.lines().collect();

    if lines.first().is_some_and(|l| l.trim().starts_with("```")) {
        lines.remove(0);
    }
    if lines.last().is_some_and(|l| l.trim().starts_with("```")) {
        lines.pop();
    }

    if let Some(sig_idx) = lines.iter().position(|l| is_fn_signature_start(l)) {
        lines.drain(..sig_idx);
    }

    lines.join("\n")
}

fn normalize_indentation(code: &str, target_base_indent: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let Some(first_line) = lines.first() else {
        return String::new();
    };

    let current_first_indent = first_line.len() - first_line.trim_start().len();
    if current_first_indent == target_base_indent.len() {
        return code.to_string();
    }

    lines
        .into_iter()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else if line.len() >= current_first_indent
                && line[..current_first_indent]
                    .chars()
                    .all(char::is_whitespace)
            {
                format!("{}{}", target_base_indent, &line[current_first_indent..])
            } else {
                format!("{}{}", target_base_indent, line)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn check_with_rustc(source: &str, _fn_start_one: usize, _fn_end_one: usize) -> bool {
    check_with_compiler(source, "rustc")
}

fn run_compiler(source: &str, compiler: &str, test: bool) -> Option<std::process::Output> {
    let rmeta_path = next_temp_rmeta_path("verify");
    let mut command = Command::new(compiler);
    command.args([
        "--edition=2024",
        "--crate-type=lib",
        "--emit=metadata",
        "-o",
        &rmeta_path,
        "--error-format=json",
        "-",
    ]);
    if test {
        command.arg("--test");
    }
    let result = (|| {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;
        let wrote = child.stdin.take()?.write_all(source.as_bytes()).is_ok();
        let output = child.wait_with_output().ok()?;
        wrote.then_some(output)
    })();
    let _ = std::fs::remove_file(&rmeta_path);
    result
}

fn check_with_compiler(source: &str, compiler: &str) -> bool {
    [false, true].into_iter().all(|test| {
        run_compiler(source, compiler, test).is_some_and(|output| output.status.success())
    })
}

fn confirms_probe(source: &str, compiler: &str) -> bool {
    let mut active = false;
    for test in [false, true] {
        let Some(output) = run_compiler(source, compiler, test) else {
            return false;
        };
        active |= !output.status.success()
            && String::from_utf8_lossy(&output.stderr).lines().any(|line| {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                    return false;
                };
                value["level"] == "error" && value["message"] == "rust-mentor-active-function-probe"
            });
    }
    active
}

/// Verify an exact edit against an isolated copy of the Cargo workspace when available.
/// A failed or unavailable check never produces an applicable refactor.
pub async fn verify_context(
    uri: &str,
    context: &crate::context::SlicedContext,
    raw_refactor: &str,
    documents: Vec<(String, String)>,
) -> Option<String> {
    let path = tower_lsp::lsp_types::Url::parse(uri)
        .ok()?
        .to_file_path()
        .ok()?;
    let context = context.clone();
    let raw_refactor = raw_refactor.to_string();
    tokio::task::spawn_blocking(move || {
        let (replacement, spliced, probe_source) = prepare_refactor(&context, &raw_refactor)?;
        let ok = if path
            .ancestors()
            .skip(1)
            .any(|p| p.join("Cargo.toml").is_file())
        {
            check_project(&path, &spliced, &probe_source, &documents)
        } else {
            check_with_rustc(&spliced, 0, 0) && confirms_probe(&probe_source, "rustc")
        };
        ok.then_some(replacement)
    })
    .await
    .ok()
    .flatten()
}

fn prepare_refactor(
    context: &crate::context::SlicedContext,
    raw_refactor: &str,
) -> Option<(String, String, String)> {
    let range = context.byte_range.clone()?;
    let stripped = strip_code_fences_and_attrs(raw_refactor);
    // Accept one function, including methods with a self parameter, never extra items.
    syn::parse_str::<syn::ImplItemFn>(&stripped).ok()?;
    let formatted = format_with_rustfmt(&stripped);
    let normalized = normalize_indentation(&formatted, &context.base_indent);
    let replacement = normalized.trim_start().to_string();
    let mut spliced = context.file_content.clone();
    spliced.replace_range(range.clone(), &replacement);
    syn::parse_file(&spliced).ok()?;
    let function = syn::parse_str::<syn::ImplItemFn>(&replacement).ok()?;
    let mut probe = replacement.clone();
    probe.insert_str(
        function.block.brace_token.span.open().byte_range().end,
        "::core::compile_error!(\"rust-mentor-active-function-probe\");",
    );
    let mut probe_source = context.file_content.clone();
    probe_source.replace_range(range, &probe);
    Some((replacement, spliced, probe_source))
}

struct ScratchDir(std::path::PathBuf);
impl ScratchDir {
    fn new() -> std::io::Result<Self> {
        loop {
            let id = RMETA_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("rust_mentor_{}_{}", std::process::id(), id));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }
}
impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_workspace(source: &Path, destination: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if path == target || name == "target" || name == ".git" {
            continue;
        }
        let kind = entry.file_type()?;
        // Do not follow links outside the snapshot or risk modifying their targets.
        if kind.is_symlink() {
            return Err(std::io::Error::other("symlink in workspace snapshot"));
        }
        let dest = destination.join(name);
        if kind.is_dir() {
            copy_workspace(&path, &dest, target)?;
        } else if kind.is_file() {
            std::fs::copy(path, dest)?;
        }
    }
    Ok(())
}

fn check_project(path: &Path, source: &str, probe: &str, documents: &[(String, String)]) -> bool {
    let check = || -> Option<bool> {
        let parent = path.parent()?;
        let metadata = Command::new("cargo")
            .args(["metadata", "--offline", "--no-deps", "--format-version=1"])
            .current_dir(parent)
            .output()
            .ok()?;
        if !metadata.status.success() {
            return None;
        }
        let metadata: serde_json::Value = serde_json::from_slice(&metadata.stdout).ok()?;
        let root = Path::new(metadata.get("workspace_root")?.as_str()?);
        let target = Path::new(metadata.get("target_directory")?.as_str()?);
        let scratch = ScratchDir::new().ok()?;
        let snapshot = scratch.0.join("workspace");
        copy_workspace(root, &snapshot, target).ok()?;
        // Overlay every open buffer so unsaved sibling modules are checked too.
        for (uri, text) in documents {
            let file = tower_lsp::lsp_types::Url::parse(uri)
                .ok()?
                .to_file_path()
                .ok()?;
            if let Ok(relative) = file.strip_prefix(root) {
                std::fs::write(snapshot.join(relative), text).ok()?;
            }
        }
        let snapshot_file = snapshot.join(path.strip_prefix(root).ok()?);
        let run = || {
            Command::new("cargo")
                .args([
                    "check",
                    "--offline",
                    "--workspace",
                    "--all-targets",
                    "--message-format=json",
                ])
                .arg("--target-dir")
                .arg(scratch.0.join("target"))
                .arg("--manifest-path")
                .arg(snapshot.join("Cargo.toml"))
                .current_dir(parent)
                .output()
                .ok()
        };
        std::fs::write(&snapshot_file, source).ok()?;
        if !run()?.status.success() {
            return Some(false);
        }
        // Cargo can succeed without compiling an orphan file or a cfg-disabled function.
        // Prove that the exact function participated in this check, in the same snapshot.
        std::fs::write(&snapshot_file, probe).ok()?;
        let output = run()?;
        let active = String::from_utf8_lossy(&output.stdout).lines().any(|line| {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                return false;
            };
            value["message"]["level"] == "error"
                && value["message"]["message"] == "rust-mentor-active-function-probe"
        });
        Some(!output.status.success() && active)
    };
    check().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_compiler_is_not_success() {
        assert!(!confirms_probe(
            "fn f() {}",
            "/nonexistent/rust-mentor-rustc"
        ));
        assert!(!check_with_compiler(
            "fn f() {}",
            "/nonexistent/rust-mentor-rustc"
        ));
    }

    #[test]
    fn rejects_unresolved_symbols() {
        assert!(!check_with_rustc("fn f() { missing(); }", 1, 1));
    }

    #[test]
    fn rejects_broken_callers_outside_replacement() {
        assert!(!check_with_rustc(
            "fn f(_: u8) {}\nfn caller() { f(); }",
            1,
            1
        ));
    }

    #[test]
    fn checks_test_configuration() {
        assert!(!check_with_rustc(
            "#[cfg(test)]\nmod tests { fn f() { missing(); } }",
            2,
            2
        ));
    }

    #[test]
    fn accepts_valid_standalone_code() {
        assert!(check_with_rustc(
            "fn f() { let s = String::new(); println!(\"{}\", s); }",
            1,
            1
        ));
    }

    #[tokio::test]
    async fn rejects_a_borrow_error() {
        let source = "fn f() {}";
        assert!(
            verify_refactor(
                source,
                0,
                0,
                "",
                "fn f() { let s = String::new(); let t = s; println!(\"{}\", s); }"
            )
            .await
            .is_none()
        );
    }
}

#[cfg(test)]
mod project_tests {
    use super::*;
    use crate::context::slice_from_str;

    fn project(source: &str) -> ScratchDir {
        let dir = ScratchDir::new().unwrap();
        std::fs::create_dir(dir.0.join("src")).unwrap();
        std::fs::write(dir.0.join("Cargo.toml"), "[package]\nname='verification_fixture'\nversion='0.1.0'\nedition='2021'\n[workspace]\n").unwrap();
        std::fs::write(dir.0.join("src/lib.rs"), source).unwrap();
        dir
    }

    async fn verify(
        dir: &ScratchDir,
        file: &str,
        line: u32,
        fix: &str,
        buffers: Vec<(String, String)>,
    ) -> Option<String> {
        let path = dir.0.join(file).canonicalize().unwrap();
        let source = std::fs::read_to_string(&path).unwrap();
        let ctx = slice_from_str(&source, line, "E0382");
        verify_context(
            tower_lsp::lsp_types::Url::from_file_path(path)
                .unwrap()
                .as_str(),
            &ctx,
            fix,
            buffers,
        )
        .await
    }

    #[tokio::test]
    async fn verifies_modules_in_project_context_without_editing_original() {
        let dir = project("mod helper;\npub fn value() -> u8 { helper::value() }\n");
        std::fs::write(
            dir.0.join("src/helper.rs"),
            "pub fn value() -> u8 { missing() }",
        )
        .unwrap();
        assert!(
            verify(
                &dir,
                "src/helper.rs",
                0,
                "pub fn value() -> u8 { 42 }",
                vec![]
            )
            .await
            .is_some()
        );
        assert!(
            std::fs::read_to_string(dir.0.join("src/helper.rs"))
                .unwrap()
                .contains("missing()")
        );
    }

    #[tokio::test]
    async fn rejects_project_caller_regressions() {
        let dir = project("pub fn value() -> u8 { 42 }\npub fn caller() -> u8 { value() }");
        assert!(
            verify(
                &dir,
                "src/lib.rs",
                0,
                "pub fn value(_: u8) -> u8 { 42 }",
                vec![]
            )
            .await
            .is_none()
        );
    }

    #[tokio::test]
    async fn includes_unsaved_sibling_buffers() {
        let dir = project("mod helper;\npub fn value() -> u8 { helper::value() }");
        let path = dir.0.join("src/helper.rs");
        std::fs::write(&path, "pub fn value() -> u8 { 42 }").unwrap();
        let uri = tower_lsp::lsp_types::Url::from_file_path(path.canonicalize().unwrap()).unwrap();
        assert!(
            verify(
                &dir,
                "src/lib.rs",
                1,
                "pub fn value() -> u8 { helper::value() }",
                vec![(
                    uri.to_string(),
                    "pub fn value() -> String { String::new() }".into()
                )]
            )
            .await
            .is_none()
        );
    }

    #[tokio::test]
    async fn rejects_files_not_part_of_a_target() {
        let dir = project("pub fn value() {}");
        std::fs::write(dir.0.join("src/orphan.rs"), "pub fn orphan() {}").unwrap();
        assert!(
            verify(
                &dir,
                "src/orphan.rs",
                0,
                "pub fn orphan() { missing(); }",
                vec![]
            )
            .await
            .is_none()
        );
    }

    #[tokio::test]
    async fn rejects_functions_disabled_by_cfg() {
        let dir = project("#[cfg(any())]\npub fn value() {}");
        assert!(
            verify(
                &dir,
                "src/lib.rs",
                1,
                "pub fn value() { missing(); }",
                vec![]
            )
            .await
            .is_none()
        );
    }

    #[tokio::test]
    async fn resolves_local_dependencies_and_default_features() {
        let dir = project("#[cfg(feature = \"enabled\")]\npub fn value() -> u8 { dep::value() }");
        std::fs::create_dir_all(dir.0.join("dep/src")).unwrap();
        std::fs::write(
            dir.0.join("dep/Cargo.toml"),
            "[package]\nname='dep'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap();
        std::fs::write(dir.0.join("dep/src/lib.rs"), "pub fn value() -> u8 { 42 }").unwrap();
        let manifest = dir.0.join("Cargo.toml");
        let mut text = std::fs::read_to_string(&manifest).unwrap();
        text.push_str(
            "[dependencies]\ndep={path='dep'}\n[features]\ndefault=['enabled']\nenabled=[]\n",
        );
        std::fs::write(manifest, text).unwrap();
        assert!(
            verify(
                &dir,
                "src/lib.rs",
                1,
                "pub fn value() -> u8 { dep::value() + 1 }",
                vec![]
            )
            .await
            .is_some()
        );
    }

    #[tokio::test]
    async fn exact_edit_preserves_inline_neighbors_and_indentation() {
        let dir = ScratchDir::new().unwrap();
        let source = "mod m {\n    fn f() { missing(); } const X: u8 = 1;\n}";
        let path = dir.0.join("standalone.rs");
        std::fs::write(&path, source).unwrap();
        let ctx = slice_from_str(source, 1, "E0425");
        let uri = tower_lsp::lsp_types::Url::from_file_path(path).unwrap();
        let replacement = verify_context(uri.as_str(), &ctx, "fn f() { let _ = \"🦀\"; }", vec![])
            .await
            .unwrap();
        let mut result = source.to_string();
        result.replace_range(ctx.byte_range.unwrap(), &replacement);
        assert!(result.contains("    fn f()"));
        assert!(result.contains("} const X: u8 = 1;"));
        assert!(check_with_rustc(&result, 0, 0));
    }

    #[tokio::test]
    async fn rejects_extra_generated_items() {
        let dir = project("pub fn value() {}");
        assert!(
            verify(
                &dir,
                "src/lib.rs",
                0,
                "pub fn value() {} fn injected() {}",
                vec![]
            )
            .await
            .is_none()
        );
    }

    #[tokio::test]
    async fn verifies_test_only_functions() {
        let dir = project("#[cfg(test)]\nmod tests {\n fn example() { missing(); }\n}");
        assert!(
            verify(&dir, "src/lib.rs", 2, "fn example() {}", vec![])
                .await
                .is_some()
        );
    }
}
