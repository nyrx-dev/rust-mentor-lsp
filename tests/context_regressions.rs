use rust_mentor_lsp::context::slice_from_str;

#[test]
fn surrounding_types_invalidate_cached_analysis() {
    let a = slice_from_str("type Value = String;\nfn f(x: Value) {}", 1, "E0382");
    let b = slice_from_str("type Value = u32;\nfn f(x: Value) {}", 1, "E0382");
    assert_ne!(a.context_hash, b.context_hash);
}

#[test]
fn identical_functions_in_different_modules_have_distinct_keys() {
    let source = "mod a {\n fn f() {}\n}\nmod b {\n fn f() {}\n}";
    assert_ne!(
        slice_from_str(source, 1, "E0382").context_hash,
        slice_from_str(source, 4, "E0382").context_hash
    );
}

#[test]
fn inline_function_does_not_include_neighbor_items() {
    let ctx = slice_from_str("mod m { fn f() {} const X: u8 = 1; }", 0, "E0382");
    assert_eq!(ctx.function_code, "fn f() {}");
    let range = ctx.function_range.unwrap();
    assert_eq!(range.start.character, 8);
    assert_eq!(range.end.character, 17);
}

#[test]
fn edit_positions_use_utf16() {
    let source = "fn f() { let _ = \"🦀\"; }";
    let ctx = slice_from_str(source, 0, "E0382");
    assert_eq!(
        ctx.function_range.unwrap().end.character,
        source.encode_utf16().count() as u32
    );
}

#[test]
fn preserves_attributes_outside_the_replacement() {
    let ctx = slice_from_str("#[allow(dead_code)]\npub async fn f() {}", 1, "E0382");
    assert_eq!(ctx.function_code, "pub async fn f() {}");
    assert_eq!(ctx.function_range.unwrap().start.line, 1);
}

#[test]
fn invalid_syntax_has_no_edit_range() {
    assert!(
        slice_from_str("fn broken( {", 0, "E0382")
            .function_range
            .is_none()
    );
}

#[test]
fn ambiguous_functions_on_one_line_do_not_offer_an_edit() {
    let ctx = slice_from_str("fn first() {} fn second() {}", 0, "E0382");
    assert!(ctx.function_range.is_none());
}

#[test]
fn crlf_and_unicode_before_function_keep_correct_positions() {
    let source = "// header\r\nmod m { /* 🦀 */ pub fn f() {} }\r\n";
    let ctx = slice_from_str(source, 1, "E0382");
    assert_eq!(ctx.function_code, "pub fn f() {}");
    assert_eq!(
        ctx.function_range.unwrap().start.character,
        "mod m { /* 🦀 */ ".encode_utf16().count() as u32
    );
}
