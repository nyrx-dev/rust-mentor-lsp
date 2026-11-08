use jev::{AsyncTypeSafeClient, State, Verdict};

pub fn is_candidate_error_code(code: &str) -> bool {
    if matches!(
        code,
        "E0106"
            | "E0277"
            | "E0373"
            | "E0382"
            | "E0499"
            | "E0502"
            | "E0503"
            | "E0505"
            | "E0506"
            | "E0507"
            | "E0515"
            | "E0521"
            | "E0596"
            | "E0597"
            | "E0716"
    ) {
        return true;
    }

    if let Ok(extra) = std::env::var("RUST_MENTOR_EXTRA_ERROR_CODES") {
        return extra.split(',').any(|c| c.trim().eq_ignore_ascii_case(code));
    }

    false
}

pub async fn should_explain(
    client: &AsyncTypeSafeClient,
    error_code: &str,
    error_text: &str,
) -> bool {
    if !is_candidate_error_code(error_code) {
        return false;
    }

    let state = State::text(error_text);
    let prompt = "Is this a Rust compiler error that benefits from an architectural explanation or code refactor?";

    !matches!(client.decide(&state, prompt, 0.6).await, Ok(Verdict::No))
}
