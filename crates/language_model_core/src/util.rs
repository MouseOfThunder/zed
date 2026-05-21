use std::str::FromStr;

/// Parses tool call arguments JSON, treating empty strings as empty objects.
///
/// Many LLM providers return empty strings for tool calls with no arguments.
/// This helper normalizes that behavior by converting empty strings to `{}`.
pub fn parse_tool_arguments(arguments: &str) -> Result<serde_json::Value, serde_json::Error> {
    let arguments = arguments.trim();
    if arguments.is_empty() {
        Ok(serde_json::Value::Object(Default::default()))
    } else {
        serde_json::Value::from_str(arguments)
    }
}

/// `partial_json_fixer::fix_json` converts a trailing `\` inside a string into `\\`
/// (a literal backslash). When used for incremental parsing (comparing successive
/// parses to extract deltas), this produces a spurious backslash character that
/// doesn't exist in the final text, corrupting the output.
///
/// This function strips any trailing incomplete escape sequence before fixing,
/// so each intermediate parse produces a true prefix of the final string value.
pub fn fix_streamed_json(partial_json: &str) -> String {
    let json = strip_trailing_incomplete_escape(partial_json);
    partial_json_fixer::fix_json(json)
}

fn strip_trailing_incomplete_escape(json: &str) -> &str {
    let trailing_backslashes = json
        .as_bytes()
        .iter()
        .rev()
        .take_while(|&&b| b == b'\\')
        .count();
    if trailing_backslashes % 2 == 1 {
        &json[..json.len() - 1]
    } else {
        json
    }
}

/// Parses a "prompt is too long: N tokens ..." message and extracts the token count.
pub fn parse_prompt_too_long(message: &str) -> Option<u64> {
    // Legacy cloud error format:
    // "prompt is too long: 12345 tokens ..."
    if let Some(rest) = message.strip_prefix("prompt is too long: ") {
        if let Some(tokens) = parse_first_u64(rest) {
            return Some(tokens);
        }
    }

    let lower = message.to_ascii_lowercase();

    // Common OpenAI-style format:
    // "... your messages resulted in 77490 tokens"
    if lower.contains("maximum context length")
        && let Some(tokens) = parse_u64_after_case_insensitive(message, "resulted in")
    {
        return Some(tokens);
    }

    // Another common format:
    // "... you requested 77490 tokens"
    if let Some(tokens) = parse_u64_after_case_insensitive(message, "requested") {
        return Some(tokens);
    }

    // vLLM-style logs / messages:
    // "prompt_tokens=77490 ... max_tokens=65536"
    if let Some(tokens) = parse_u64_after_case_insensitive(message, "prompt_tokens=") {
        return Some(tokens);
    }

    // Alternative shape:
    // "input token count (77490) exceeds max context length (65536)"
    if let Some(tokens) = parse_u64_after_case_insensitive(message, "input token count") {
        return Some(tokens);
    }

    None
}

/// Returns true if the message strongly indicates prompt/context-window overflow.
pub fn is_context_window_overflow_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();

    lower.contains("context_length_exceeded")
        || lower.contains("prompt is too long")
        || lower.contains("prompt too large")
        || lower.contains("maximum context length")
        || lower.contains("max context length")
        || (lower.contains("context window")
            && (lower.contains("exceed") || lower.contains("too long") || lower.contains("too large")))
        || (lower.contains("input token count") && lower.contains("exceed"))
        || (lower.contains("prompt_tokens=") && lower.contains("max_tokens="))
        || (lower.contains("prompt tokens") && lower.contains("max tokens"))
}

fn parse_u64_after_case_insensitive(message: &str, marker: &str) -> Option<u64> {
    let lower = message.to_ascii_lowercase();
    let marker_lower = marker.to_ascii_lowercase();
    let idx = lower.find(&marker_lower)?;
    let rest = &message[idx + marker_lower.len()..];
    parse_first_u64(rest)
}

fn parse_first_u64(input: &str) -> Option<u64> {
    let bytes = input.as_bytes();
    let mut start = None;

    for (ix, byte) in bytes.iter().enumerate() {
        if byte.is_ascii_digit() {
            start = Some(ix);
            break;
        }
    }

    let start = start?;
    let mut end = start;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }

    input[start..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fix_streamed_json_strips_incomplete_escape() {
        let fixed = fix_streamed_json(r#"{"text": "hello\"#);
        let parsed: serde_json::Value = serde_json::from_str(&fixed).expect("valid json");
        assert_eq!(parsed["text"], "hello");
    }

    #[test]
    fn test_fix_streamed_json_preserves_complete_escape() {
        let fixed = fix_streamed_json(r#"{"text": "hello\\"#);
        let parsed: serde_json::Value = serde_json::from_str(&fixed).expect("valid json");
        assert_eq!(parsed["text"], "hello\\");
    }

    #[test]
    fn test_fix_streamed_json_strips_escape_after_complete_escape() {
        let fixed = fix_streamed_json(r#"{"text": "hello\\\"#);
        let parsed: serde_json::Value = serde_json::from_str(&fixed).expect("valid json");
        assert_eq!(parsed["text"], "hello\\");
    }

    #[test]
    fn test_fix_streamed_json_no_escape_at_end() {
        let fixed = fix_streamed_json(r#"{"text": "hello"#);
        let parsed: serde_json::Value = serde_json::from_str(&fixed).expect("valid json");
        assert_eq!(parsed["text"], "hello");
    }

    #[test]
    fn test_fix_streamed_json_newline_escape_boundary() {
        let fixed = fix_streamed_json(r#"{"text": "line1\"#);
        let parsed: serde_json::Value = serde_json::from_str(&fixed).expect("valid json");
        assert_eq!(parsed["text"], "line1");

        let fixed = fix_streamed_json(r#"{"text": "line1\nline2"#);
        let parsed: serde_json::Value = serde_json::from_str(&fixed).expect("valid json");
        assert_eq!(parsed["text"], "line1\nline2");
    }

    #[test]
    fn test_fix_streamed_json_incremental_delta_correctness() {
        let chunk1 = r#"{"replacement_text": "fn foo() {\"#;
        let fixed1 = fix_streamed_json(chunk1);
        let parsed1: serde_json::Value = serde_json::from_str(&fixed1).expect("valid json");
        let text1 = parsed1["replacement_text"].as_str().expect("string");
        assert_eq!(text1, "fn foo() {");

        let chunk2 = r#"{"replacement_text": "fn foo() {\n    return bar;\n}"}"#;
        let fixed2 = fix_streamed_json(chunk2);
        let parsed2: serde_json::Value = serde_json::from_str(&fixed2).expect("valid json");
        let text2 = parsed2["replacement_text"].as_str().expect("string");
        assert_eq!(text2, "fn foo() {\n    return bar;\n}");

        let delta = &text2[text1.len()..];
        assert_eq!(delta, "\n    return bar;\n}");
    }

    #[test]
    fn test_parse_prompt_too_long_openai_max_context_message() {
        let message =
            "This model's maximum context length is 65536 tokens. However, your messages resulted in 77490 tokens.";
        assert_eq!(parse_prompt_too_long(message), Some(77490));
    }

    #[test]
    fn test_parse_prompt_too_long_vllm_style_message() {
        let message = "request=abc prompt_tokens=77490 tokens_to_prefill=77490 max_tokens=65536";
        assert_eq!(parse_prompt_too_long(message), Some(77490));
    }

    #[test]
    fn test_is_context_window_overflow_message_detects_common_shapes() {
        assert!(is_context_window_overflow_message(
            "Error: context_length_exceeded for this request"
        ));
        assert!(is_context_window_overflow_message(
            "input token count (77490) exceeds max context length (65536)"
        ));
        assert!(is_context_window_overflow_message(
            "prompt_tokens=77490 max_tokens=65536"
        ));
        assert!(!is_context_window_overflow_message(
            "invalid request format: missing field messages"
        ));
    }
}
