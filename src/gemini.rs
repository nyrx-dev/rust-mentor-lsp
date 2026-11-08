use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Serialize)]
struct GenerateRequest {
    contents: Vec<Content>,
    #[serde(rename = "generationConfig")]
    generation_config: GenerationConfig,
}

#[derive(Serialize)]
struct GenerationConfig {
    temperature: f32,
    #[serde(rename = "responseMimeType")]
    response_mime_type: String,
    #[serde(rename = "responseSchema")]
    response_schema: serde_json::Value,
    #[serde(rename = "thinkingConfig")]
    thinking_config: ThinkingConfig,
}

#[derive(Serialize)]
struct ThinkingConfig {
    #[serde(rename = "thinkingBudget")]
    thinking_budget: i32,
}

#[derive(Serialize)]
struct Content {
    parts: Vec<Part>,
}

#[derive(Serialize, Deserialize)]
struct Part {
    text: String,
}

#[derive(Deserialize)]
struct GenerateResponse {
    candidates: Option<Vec<Candidate>>,
}

#[derive(Deserialize)]
struct Candidate {
    content: Option<ResponseContent>,
}

#[derive(Deserialize)]
struct ResponseContent {
    parts: Option<Vec<Part>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StringOrVec {
    Single(String),
    Multiple(Vec<String>),
}

#[derive(Deserialize)]
struct RawMentorResponse {
    explanation: StringOrVec,
    refactored_function: Option<String>,
}

#[derive(Clone, Debug)]
pub struct MentorResponse {
    pub explanation: String,
    pub refactored_function: Option<String>,
}

#[derive(Clone)]
pub struct GeminiClient {
    http: Client,
    api_key: String,
    model: String,
}

impl GeminiClient {
    pub fn new(api_key: String) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            http,
            api_key,
            model: "gemini-2.5-flash".to_string(),
        }
    }

    pub fn from_env() -> Result<Self, String> {
        let api_key = std::env::var("GEMINI_API_KEY")
            .map_err(|_| "GEMINI_API_KEY environment variable not set".to_string())?;
        Ok(Self::new(api_key))
    }

    pub async fn analyze_error(
        &self,
        error_code: &str,
        error_message: &str,
        prompt_context: &str,
        raw_function: &str,
    ) -> Result<MentorResponse, String> {
        let prompt = format!(
            "You are Rust Mentor LSP. Be extremely brief and direct.\n\n\
            Error Code: {}\n\
            Compiler Diagnostic:\n{}\n\n\
            AST Context:\n{}\n\n\
            Original Enclosing Function:\n```rust\n{}\n```\n\n\
            Return a JSON object with two fields:\n\
            1. \"explanation\": A string with MAXIMUM 2 bullet points (one short sentence each, NO markdown headings, NO filler):\n\
               \"- **Why:** <1 short sentence on what ownership/borrow rule broke>\\n- **Fix:** <1 short sentence on the idiomatic fix (prefer borrowing/reordering over .clone())>\"\n\
            2. \"refactored_function\": The complete replacement Rust code for the Original Enclosing Function only (from `fn ...` to closing `}}`), fixing the error idiomatically (avoid `.clone()` if borrowing or reordering works) with zero comments and no markdown fences.",
            error_code, error_message, prompt_context, raw_function
        );

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            self.model, self.api_key
        );

        let schema = serde_json::json!({
            "type": "OBJECT",
            "properties": {
                "explanation": { "type": "STRING" },
                "refactored_function": { "type": "STRING" }
            },
            "required": ["explanation", "refactored_function"]
        });

        let body = GenerateRequest {
            contents: vec![Content {
                parts: vec![Part { text: prompt }],
            }],
            generation_config: GenerationConfig {
                temperature: 0.1,
                response_mime_type: "application/json".to_string(),
                response_schema: schema,
                thinking_config: ThinkingConfig { thinking_budget: 0 },
            },
        };

        let response = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(format!("Gemini API error ({}): {}", status, text));
        }

        let parsed: GenerateResponse = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse response: {}", e))?;

        let raw_text = parsed
            .candidates
            .and_then(|mut c| {
                if c.is_empty() {
                    None
                } else {
                    Some(c.remove(0))
                }
            })
            .and_then(|c| c.content)
            .and_then(|c| {
                c.parts.and_then(|mut p| {
                    if p.is_empty() {
                        None
                    } else {
                        Some(p.remove(0))
                    }
                })
            })
            .map(|p| p.text)
            .ok_or_else(|| "No text in Gemini response".to_string())?;

        if let Ok(raw_parsed) = serde_json::from_str::<RawMentorResponse>(&raw_text) {
            let explanation = match raw_parsed.explanation {
                StringOrVec::Single(s) => s.replace(" - **Fix:**", "\n- **Fix:**"),
                StringOrVec::Multiple(v) => v.join("\n"),
            };
            Ok(MentorResponse {
                explanation,
                refactored_function: raw_parsed.refactored_function,
            })
        } else {
            Ok(MentorResponse {
                explanation: raw_text,
                refactored_function: None,
            })
        }
    }
}
