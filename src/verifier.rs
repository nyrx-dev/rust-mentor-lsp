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
    let stripped = strip_code_fences_and_attrs(raw_refactor);
    if stripped.trim().is_empty() {
        return None;
    }

    let formatted = format_with_rustfmt(&stripped);
    let cleaned = normalize_indentation(&formatted, base_indent);

    let rep_lines = cleaned.lines().count().max(1);
    let fn_start_one = (start_line_zero as usize) + 1;
    let fn_end_one = fn_start_one + rep_lines - 1;

    let spliced = splice_lines(
        original_file,
        start_line_zero as usize,
        end_line_zero as usize,
        &cleaned,
    )?;

    if syn::parse_file(&spliced).is_err() {
        return None;
    }

    let rustc_ok =
        tokio::task::spawn_blocking(move || check_with_rustc(&spliced, fn_start_one, fn_end_one))
            .await
            .unwrap_or(false);

    if rustc_ok { Some(cleaned) } else { None }
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

fn splice_lines(
    original: &str,
    start_zero: usize,
    end_zero: usize,
    replacement: &str,
) -> Option<String> {
    let lines: Vec<&str> = original.lines().collect();
    if start_zero > end_zero || end_zero >= lines.len() {
        return None;
    }

    let mut out = Vec::new();
    out.extend_from_slice(&lines[..start_zero]);
    for rep_line in replacement.lines() {
        out.push(rep_line);
    }
    if end_zero + 1 < lines.len() {
        out.extend_from_slice(&lines[(end_zero + 1)..]);
    }

    Some(out.join("\n"))
}

fn check_with_rustc(source: &str, fn_start_one: usize, fn_end_one: usize) -> bool {
    let rmeta_path = next_temp_rmeta_path("verify");
    let Ok(mut child) = Command::new("rustc")
        .args([
            "--edition=2024",
            "--crate-type=lib",
            "--emit=metadata",
            "-o",
            &rmeta_path,
            "--error-format=json",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    else {
        return true;
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(source.as_bytes());
    }

    let Ok(output) = child.wait_with_output() else {
        let _ = std::fs::remove_file(&rmeta_path);
        return true;
    };
    let _ = std::fs::remove_file(&rmeta_path);

    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stderr.lines() {
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if json.get("level").and_then(|l| l.as_str()) != Some("error") {
            continue;
        }

        let in_refactored_fn = json
            .get("spans")
            .and_then(|s| s.as_array())
            .is_some_and(|spans| {
                spans.iter().any(|sp| {
                    let l_start =
                        sp.get("line_start").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    let l_end = sp.get("line_end").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    l_start <= fn_end_one && l_end >= fn_start_one
                })
            });

        if !in_refactored_fn {
            continue;
        }

        let code = json
            .get("code")
            .and_then(|c| c.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or_default();

        if !matches!(code, "E0432" | "E0433" | "E0412" | "E0425" | "E0405") {
            return false;
        }
    }

    true
}
