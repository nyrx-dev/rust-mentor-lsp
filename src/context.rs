use dashmap::DashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use syn::visit::{self, Visit};
use syn::{
    Fields, ImplItemFn, ItemEnum, ItemFn, ItemImpl, ItemStruct, ItemTrait, Signature, TraitItemFn,
    Visibility,
};
use tower_lsp::lsp_types::{Position, Range, Url};

#[derive(Clone, Debug)]
pub struct SlicedContext {
    pub file_content: String,
    pub function_code: String,
    pub types_context: String,
    pub prompt_context: String,
    pub function_range: Option<Range>,
    pub start_line: u32,
    pub end_line: u32,
    pub base_indent: String,
    pub context_hash: u64,
}

pub fn slice_context(
    uri_str: &str,
    target_line_zero_based: u32,
    error_code: &str,
    documents: &DashMap<String, String>,
) -> Option<SlicedContext> {
    if let Some(doc) = documents.get(uri_str) {
        return Some(slice_from_str(
            doc.value(),
            target_line_zero_based,
            error_code,
        ));
    }
    let url = Url::parse(uri_str).ok()?;
    let path = url.to_file_path().ok()?;
    let content = std::fs::read_to_string(path).ok()?;
    Some(slice_from_str(&content, target_line_zero_based, error_code))
}

fn fn_start_line(vis: Option<&Visibility>, sig: &Signature) -> usize {
    let mut line = sig.fn_token.span.start().line;
    if let Some(abi) = &sig.abi {
        line = line.min(abi.extern_token.span.start().line);
    }
    match &sig.safety {
        syn::Safety::Unsafe(tok) => line = line.min(tok.span.start().line),
        syn::Safety::Safe(tok) => line = line.min(tok.span.start().line),
        syn::Safety::Default => {}
    }
    if let Some(asyncness) = &sig.asyncness {
        line = line.min(asyncness.span.start().line);
    }
    if let Some(constness) = &sig.constness {
        line = line.min(constness.span.start().line);
    }
    if let Some(Visibility::Public(pub_tok)) = vis {
        line = line.min(pub_tok.span.start().line);
    } else if let Some(Visibility::Restricted(res)) = vis {
        line = line.min(res.pub_token.span.start().line);
    }
    line
}

struct AstCollector<'a> {
    lines: &'a [&'a str],
    target_line_one: usize,
    fn_bounds: Option<(usize, usize)>,
    enclosing_impl_header: Option<String>,
    type_defs: Vec<String>,
}

impl<'ast, 'a> Visit<'ast> for AstCollector<'a> {
    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        let start = fn_start_line(Some(&node.vis), &node.sig);
        let end = node.block.brace_token.span.close().end().line;
        if self.target_line_one >= start && self.target_line_one <= end {
            self.fn_bounds = Some((start, end));
            self.enclosing_impl_header = None;
        }
        visit::visit_item_fn(self, node);
    }

    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        let impl_start = node.impl_token.span.start().line;
        let impl_open_brace = node.brace_token.span.open().start().line;
        let header = extract_lines(self.lines, impl_start, impl_open_brace)
            .map(|h| h.trim().trim_end_matches('{').trim().to_string());

        for item in &node.items {
            if let syn::ImplItem::Fn(ImplItemFn {
                vis, sig, block, ..
            }) = item
            {
                let start = fn_start_line(Some(vis), sig);
                let end = block.brace_token.span.close().end().line;
                if self.target_line_one >= start && self.target_line_one <= end {
                    self.fn_bounds = Some((start, end));
                    self.enclosing_impl_header = header.clone();
                }
            }
        }
        visit::visit_item_impl(self, node);
    }

    fn visit_item_struct(&mut self, node: &'ast ItemStruct) {
        let start = node.struct_token.span.start().line;
        let end = node
            .semi_token
            .map(|t| t.span.end().line)
            .unwrap_or_else(|| match &node.fields {
                Fields::Named(f) => f.brace_token.span.close().end().line,
                Fields::Unnamed(f) => f.paren_token.span.close().end().line,
                Fields::Unit => node.ident.span().end().line,
            });
        if let Some(slice) = extract_lines(self.lines, start, end) {
            self.type_defs.push(slice);
        }
        visit::visit_item_struct(self, node);
    }

    fn visit_item_enum(&mut self, node: &'ast ItemEnum) {
        let start = node.enum_token.span.start().line;
        let end = node.brace_token.span.close().end().line;
        if let Some(slice) = extract_lines(self.lines, start, end) {
            self.type_defs.push(slice);
        }
        visit::visit_item_enum(self, node);
    }

    fn visit_item_trait(&mut self, node: &'ast ItemTrait) {
        let start = node.trait_token.span.start().line;
        let end = node.brace_token.span.close().end().line;
        if let Some(slice) = extract_lines(self.lines, start, end) {
            self.type_defs.push(slice);
        }

        let trait_open_brace = node.brace_token.span.open().start().line;
        let header = extract_lines(self.lines, start, trait_open_brace)
            .map(|h| h.trim().trim_end_matches('{').trim().to_string());

        for item in &node.items {
            if let syn::TraitItem::Fn(TraitItemFn {
                sig,
                default: Some(block),
                ..
            }) = item
            {
                let fn_start = fn_start_line(None, sig);
                let fn_end = block.brace_token.span.close().end().line;
                if self.target_line_one >= fn_start && self.target_line_one <= fn_end {
                    self.fn_bounds = Some((fn_start, fn_end));
                    self.enclosing_impl_header = header.clone();
                }
            }
        }
        visit::visit_item_trait(self, node);
    }
}

pub fn slice_from_str(
    content: &str,
    target_line_zero_based: u32,
    error_code: &str,
) -> SlicedContext {
    let lines: Vec<&str> = content.lines().collect();
    let target_line_one_based = (target_line_zero_based + 1) as usize;

    let mut collector = AstCollector {
        lines: &lines,
        target_line_one: target_line_one_based,
        fn_bounds: None,
        enclosing_impl_header: None,
        type_defs: Vec::new(),
    };

    if let Ok(ast) = syn::parse_file(content) {
        collector.visit_file(&ast);
    }

    let (start_one, end_one, has_exact_fn) = match collector.fn_bounds {
        Some((s, e)) => (s, e, true),
        None => {
            let s = target_line_one_based.saturating_sub(10).max(1);
            let e = (target_line_one_based + 10).min(lines.len().max(1));
            (s, e, false)
        }
    };

    let function_code = extract_lines(&lines, start_one, end_one).unwrap_or_default();
    let numbered_fn = extract_numbered_lines(&lines, start_one, end_one);
    let types_context = collector.type_defs.join("\n\n");

    let impl_prefix = collector
        .enclosing_impl_header
        .map(|h| format!("Enclosing Impl: `{}`\n\n", h))
        .unwrap_or_default();

    let prompt_context = if types_context.is_empty() {
        format!(
            "{}Enclosing Function (lines {}-{}):\n{}",
            impl_prefix, start_one, end_one, numbered_fn
        )
    } else {
        format!(
            "Local Types in File:\n{}\n\n{}Enclosing Function (lines {}-{}):\n{}",
            types_context, impl_prefix, start_one, end_one, numbered_fn
        )
    };

    let start_zero = (start_one.saturating_sub(1)) as u32;
    let end_zero = (end_one.saturating_sub(1)) as u32;
    let end_col = lines
        .get(end_zero as usize)
        .map(|l| l.len() as u32)
        .unwrap_or(0);

    let base_indent = lines
        .get(start_zero as usize)
        .map(|l| {
            let trimmed_len = l.trim_start().len();
            l[..(l.len() - trimmed_len)].to_string()
        })
        .unwrap_or_default();

    let function_range = if has_exact_fn {
        Some(Range {
            start: Position {
                line: start_zero,
                character: 0,
            },
            end: Position {
                line: end_zero,
                character: end_col,
            },
        })
    } else {
        None
    };

    let mut hasher = DefaultHasher::new();
    error_code.hash(&mut hasher);
    function_code.trim().hash(&mut hasher);
    let context_hash = hasher.finish();

    SlicedContext {
        file_content: content.to_string(),
        function_code,
        types_context,
        prompt_context,
        function_range,
        start_line: start_zero,
        end_line: end_zero,
        base_indent,
        context_hash,
    }
}

fn extract_lines(lines: &[&str], start_one: usize, end_one: usize) -> Option<String> {
    if lines.is_empty() || start_one == 0 || start_one > end_one {
        return None;
    }
    let s = start_one.saturating_sub(1).min(lines.len());
    let e = end_one.min(lines.len());
    Some(lines[s..e].join("\n"))
}

fn extract_numbered_lines(lines: &[&str], start_one: usize, end_one: usize) -> String {
    if lines.is_empty() || start_one == 0 || start_one > end_one {
        return String::new();
    }
    let s = start_one.saturating_sub(1).min(lines.len());
    let e = end_one.min(lines.len());
    lines[s..e]
        .iter()
        .enumerate()
        .map(|(idx, line)| format!("{:4} | {}", s + idx + 1, line))
        .collect::<Vec<_>>()
        .join("\n")
}
