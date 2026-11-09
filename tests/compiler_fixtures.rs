use rust_mentor_lsp::verifier::verify_refactor;
use std::process::Command;

#[tokio::test]
async fn ownership_fixtures_fail_for_the_expected_reason_and_fixes_compile() {
    let fixtures = [
        (
            "use_after_move.rs",
            "E0382",
            "fn main() { let value = String::from(\"hello\"); let moved = &value; println!(\"{} {}\", value, moved); }",
        ),
        (
            "conflicting_borrows.rs",
            "E0502",
            "fn main() { let mut data = vec![1, 2, 3]; data.push(4); let first = &data[0]; println!(\"{}\", first); }",
        ),
        (
            "short_lifetime.rs",
            "E0597",
            "fn main() { let value = String::from(\"hello\"); let reference = &value; println!(\"{}\", reference); }",
        ),
    ];
    for (file, code, fix) in fixtures {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(file);
        let output = Command::new("rustc")
            .args(["--edition=2024", "--emit=metadata", "--error-format=json"])
            .arg("-o")
            .arg(std::env::temp_dir().join(format!(
                "mentor-fixture-{}-{file}.rmeta",
                std::process::id()
            )))
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "{file} must actually contain an error"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(code),
            "{file}: expected {code}"
        );
        let source = std::fs::read_to_string(path).unwrap();
        assert!(
            verify_refactor(&source, 0, source.lines().count() as u32 - 1, "", fix)
                .await
                .is_some(),
            "{file}: valid fix rejected"
        );
    }
}
