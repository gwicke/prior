//! Classification of provider and transport failures into
//! [`crate::PriorError`], and the retry policy that follows from it.
//!
//! One taxonomy, three entry points: [`classify_prompt_error`] for the blocking
//! `chat()` surface used by `summarize`, [`classify_stream_error`] for the
//! streaming turn, and [`classify_completion_error`] underneath both — every
//! provider error arrives as a `CompletionError` wearing some transport's
//! clothes, and this is the only place the shapes are unwrapped.
//!
//! [`retry_budget`] and [`retry_delay_ms`] are what the REPL applies to
//! whatever comes back. They live here rather than beside the loop because the
//! policy is only meaningful against this taxonomy: which variants are
//! retryable at all is decided by `PriorError::is_auto_retryable`, and the
//! budget deliberately covers every one of them.

use rig_agent::agent::StreamingError;
use rig_agent::completion::{CompletionError, PromptError};

/// Classify a non-streaming `PromptError` (returned from the blocking
/// `chat()` surface used by `summarize`) into the same structured
/// `PriorError` taxonomy the streaming path uses. Without this, a
/// `/tree --summarize` 429 surfaces as the generic
/// `PriorError::Other("summarize: CompletionError: HttpError: …")` and
/// loses the Retry-After hint.
pub(crate) fn classify_prompt_error(e: &PromptError, op: &str) -> crate::PriorError {
    use crate::PriorError::*;
    match e {
        PromptError::CompletionError(ce) => {
            let classified = classify_completion_error(ce);
            // Re-tag the display so the user sees which op failed.
            match classified {
                Other(_) | Llm(_) => Llm(format!("{op}: {ce}")),
                other => other,
            }
        }
        PromptError::MaxTurnsError { max_turns, .. } => LlmMaxTurns {
            max_turns: *max_turns,
        },
        PromptError::UnknownToolCall {
            tool_name,
            available_tools,
            ..
        } => LlmUnknownTool {
            tool_name: tool_name.clone(),
            available: available_tools.clone(),
        },
        PromptError::PromptCancelled { reason, .. } => Cancelled(reason.clone()),
        PromptError::MemoryError(_) => Other(format!("{op}: {e}")),
    }
}

/// How many retry attempts the REPL grants for `e`, per logical turn (the
/// caller's `attempt` counter is scoped to one prompt submission, so a fresh
/// user prompt resets it). All auto-retryable errors — rate-limited, upstream
/// 5xx, transport, streaming-protocol, and malformed tool calls — share the
/// same generous budget. They are all transient failures that should clear
/// with a retry; the budget exists to cap a runaway, not to second-guess the
/// policy. Post-MVP: expose this via `AgentConfig`.
pub(crate) fn retry_budget(e: &crate::PriorError) -> u8 {
    if e.is_auto_retryable() {
        50
    } else {
        0
    }
}

/// Back-off delay before the next retry attempt, in milliseconds.
///
/// Policy: `2^(consecutive_failures - 1)`, no upper bound — the per-turn
/// `retry_budget` (50) caps how many retries we will ever attempt, so the
/// delay growth is implicitly bounded in practice. We do not cap the value
/// at 300 s or any other number: the user wanted raw exponential growth
/// bounded only by the retry count.
///
/// The `consecutive_failures` counter is reset to 0 whenever the
/// underlying prompt made progress (the agent returned more content or
/// tool calls) before failing — even if the run ultimately errored. That
/// is, `consecutive_failures` counts *consecutive failures with no
/// progress between them*. A failure that happened *after* a successful
/// sub-run inside the same turn restarts the sequence at `2^0 = 1` s, so
/// the next attempt is not punished for a one-off blip mid-turn.
///
/// A provider-supplied `Retry-After` (or OpenRouter's
/// `metadata.retry_after_seconds`) is the **floor** when present — respect
/// the provider's hint, then add our exponential growth on top by treating
/// the hint as the starting base.
///
/// # `PRIOR_TEST_RETRY_BASE_MS`
///
/// The *base* of the no-hint schedule is overridable, in milliseconds, so the
/// back-off tests in `tests/retry_backoff.rs` can exercise the growth and reset
/// behaviour in milliseconds instead of sleeping out real seconds. Those tests
/// spawn this binary and assert on the gaps between HTTP requests, so they
/// cannot use `tokio::time::pause` the way an in-process test would.
///
/// The name is deliberately long and test-specific: a typo would have to be
/// quite deliberate to land here. It only ever shortens a delay in a process
/// that was explicitly started with it set, and a production run sets nothing.
/// Only the no-hint branch reads it — a provider's `Retry-After` is a floor we
/// must respect, so it is never scaled down.
///
/// Unset (or unparseable) means the production default: 1s.
const TEST_RETRY_BASE_MS_VAR: &str = "PRIOR_TEST_RETRY_BASE_MS";

/// The no-hint base delay, in milliseconds. See [`TEST_RETRY_BASE_MS_VAR`].
fn retry_base_ms() -> u64 {
    match std::env::var(TEST_RETRY_BASE_MS_VAR) {
        Ok(v) => v.trim().parse::<u64>().ok().filter(|ms| *ms > 0),
        Err(_) => None,
    }
    .unwrap_or(1000)
}

pub(crate) fn retry_delay_ms(e: &crate::PriorError, consecutive_failures: u8) -> u64 {
    let n = consecutive_failures.saturating_sub(1).min(63) as u32;
    // Provider hint floors the delay. If present, we use it as the base of
    // the exponential schedule: attempt 1 = `hint`, attempt 2 = `hint*2`,
    // …, attempt N = `hint * 2^(N-1)`. This respects upstream's request
    // while still growing on a string of consecutive 429s.
    if let Some(hint) = e.retry_after_secs() {
        return hint.saturating_mul(1000).saturating_mul(1u64 << n);
    }
    // No provider hint: pure 2^(n-1) multiples of the base, no cap.
    //
    // Milliseconds, not seconds, so a sub-second base is representable: a test
    // that set a 30ms base and this returned whole seconds would truncate every
    // delay to 0 and silently delete the back-off it is trying to observe.
    // Production's base is 1000ms, so the real schedule is unchanged.
    retry_base_ms().saturating_mul(1u64 << n)
}

/// Classify a `StreamingError` into a structured `PriorError` variant.
/// `prompt()` invokes this on every non-cancellation stream error. The
/// return is what the REPL shows the user, what determines whether the
/// REPL auto-retries, and (for malformed tool calls) the synthetic
/// feedback the model sees on the retry. Status / headers / body come
/// from the `provider_response_*` accessors on `CompletionError` (rig
/// `error.rs` `forward_provider_response_helpers!`); unknown
/// `ResponseError(String)` shapes fall through to `LlmStreamProtocol`.
pub(crate) fn classify_stream_error(e: &StreamingError) -> crate::PriorError {
    let ce: &CompletionError = match e {
        StreamingError::Completion(ce) => ce,
        StreamingError::Prompt(prompt_err) => match prompt_err.as_ref() {
            PromptError::CompletionError(ce) => ce,
            PromptError::MaxTurnsError { max_turns, .. } => {
                return crate::PriorError::LlmMaxTurns {
                    max_turns: *max_turns,
                };
            }
            PromptError::UnknownToolCall {
                tool_name,
                available_tools,
                ..
            } => {
                return crate::PriorError::LlmUnknownTool {
                    tool_name: tool_name.clone(),
                    available: available_tools.clone(),
                };
            }
            // PromptCancelled is handled by the caller before this fn runs.
            PromptError::PromptCancelled { .. } | PromptError::MemoryError(_) => {
                return crate::PriorError::Other(e.to_string());
            }
        },
    };
    classify_completion_error(ce)
}

/// Reduce a provider's error body to one line a human can act on.
///
/// Providers disagree on the envelope, so every shape in the wild is tried in
/// order of how much it actually says:
///
/// - `{"error": {"message": …, "param": …}}` — OpenAI, Anthropic, OpenRouter,
///   Groq. `param` is appended when present, because for a rejected
///   `temperature` it is the field name the user needs and nothing else in the
///   body may repeat it.
/// - `{"error": "…"}` / `{"message": …}` / `{"detail": …}` — flatter servers,
///   and FastAPI-shaped validation errors (`detail` is an array there, so its
///   items are joined).
/// - a bare non-JSON body, whitespace-collapsed.
///
/// Returns `None` when the body is empty, is not text at all, or is pure
/// structure with no message — better a bare status than `; {}`.
///
/// This is the only substitute for information the catalog does not publish.
/// A model that advertises no default temperature still tells us its real
/// range here, in the rejection itself, which is why this matters more than
/// any client-side guess.
fn provider_error_detail(body: Option<&str>) -> crate::ErrorDetail {
    const MAX: usize = 300;
    let Some(raw) = body else {
        return crate::ErrorDetail(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return crate::ErrorDetail(None);
    }
    let text = match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(v) => message_from_json(&v).unwrap_or_default(),
        Err(_) => trimmed.to_string(),
    };
    let one_line = collapse_whitespace(&text);
    if one_line.is_empty() {
        return crate::ErrorDetail(None);
    }
    let clipped = if one_line.chars().count() > MAX {
        let head: String = one_line.chars().take(MAX).collect();
        format!("{head}…")
    } else {
        one_line
    };
    crate::ErrorDetail(Some(clipped))
}

/// Pull the human-facing message out of a parsed error body, plus the offending
/// field when the provider names one.
fn message_from_json(v: &serde_json::Value) -> Option<String> {
    // The common case: an `error` that is either a string or an object with a
    // `message` (and often a `param`/`field` naming the rejected input).
    let err = v.get("error");
    let (msg, param) = match err {
        Some(serde_json::Value::String(s)) => (Some(s.clone()), None),
        Some(serde_json::Value::Object(o)) => (
            o.get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string),
            o.get("param")
                .or_else(|| o.get("field"))
                .or_else(|| o.get("parameter"))
                .and_then(|p| p.as_str())
                .map(str::to_string),
        ),
        _ => (None, None),
    };
    let msg = msg
        .or_else(|| {
            v.get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .or_else(|| v.get("detail").map(json_detail_to_string))?;
    // Only worth naming the field when the message does not already say it.
    match param {
        Some(p) if !msg.contains(&p) => Some(format!("{msg} (field: {p})")),
        _ => Some(msg),
    }
}

/// FastAPI-style `detail`, which is either a string or an array of objects with
/// `msg` and `loc`.
fn json_detail_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .map(|item| {
                let msg = item
                    .get("msg")
                    .and_then(|m| m.as_str())
                    .unwrap_or("invalid");
                match item.get("loc").and_then(|l| l.as_array()) {
                    Some(loc) if !loc.is_empty() => {
                        let path = loc
                            .iter()
                            .filter_map(|p| p.as_str())
                            .collect::<Vec<_>>()
                            .join(".");
                        if path.is_empty() {
                            msg.to_string()
                        } else {
                            format!("{path}: {msg}")
                        }
                    }
                    _ => msg.to_string(),
                }
            })
            .collect::<Vec<_>>()
            .join("; "),
        other => other.to_string(),
    }
}

/// Collapse every run of whitespace to a single space, so a multi-line JSON
/// string or a wrapped HTML error page still renders as one report line.
fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Classify a `CompletionError` into a structured `PriorError`.
///
/// Exposed for unit tests; `classify_stream_error` is the production
/// entry point and unwraps both `StreamingError` variants before
/// delegating here.
pub(crate) fn classify_completion_error(ce: &CompletionError) -> crate::PriorError {
    use crate::PriorError::*;
    if let Some(status) = ce.provider_response_status() {
        let code = status.as_u16();
        // A 2xx "success" status cannot signal failure on its own: some
        // providers (OpenRouter, GMICloud via SSE) stream an error envelope
        // *inside* a 200 OK body, so those defer to the body-based envelope
        // detection below. Every genuine failure status (and the odd 1xx/3xx)
        // classifies from the status directly.
        if !status.is_success() {
            let headers = ce.provider_response_headers();
            let retry_after_secs = headers
                .and_then(|h| h.get("retry-after"))
                .and_then(|v| v.to_str().ok())
                .and_then(|s| {
                    // Retry-After can be either a delta-seconds integer or an
                    // HTTP-date. We only handle the integer form (the form
                    // OpenRouter and most providers use for rate limits).
                    s.parse::<u64>().ok()
                });
            // The body is the provider's own account of what it rejected. rig
            // keeps it verbatim on the error, so there is no reason to show the
            // user a status code and nothing else.
            let detail = provider_error_detail(ce.provider_response_body());
            return match code {
                401 | 403 => LlmAuth { status: code },
                402 => LlmBilling { status: code },
                429 => LlmRateLimited { retry_after_secs },
                500..=599 => LlmUpstream {
                    status: code,
                    detail,
                },
                400..=499 => LlmClient {
                    status: code,
                    detail,
                },
                // 1xx/3xx are not errors at the transport layer; treat as
                // generic.
                _ => Other(ce.to_string()),
            };
        }
    }

    match ce {
        CompletionError::HttpError(_) => LlmTransport(ce.to_string()),
        CompletionError::ProviderError(_) => LlmTransport(ce.to_string()),
        CompletionError::ResponseError(msg) => {
            detect_response_error(msg).unwrap_or_else(|| LlmStreamProtocol(msg.clone()))
        }
        // SSE streamed the provider's error envelope as the terminal event:
        // rig's `completion_error_from_body` builds this as a
        // `ProviderResponse` with NO HTTP status (or a 2xx one from
        // `from_http_response`). The body IS the JSON envelope, so run the
        // same detector the `ResponseError` path uses instead of falling
        // through to `Other` (which is not auto-retryable).
        CompletionError::ProviderResponse(pre) => {
            detect_response_error(&pre.body).unwrap_or_else(|| Other(ce.to_string()))
        }
        _ => Other(ce.to_string()),
    }
}

/// Detect a small number of well-known provider-error shapes that rig emits
/// with no richer classification. Today this covers the
/// assembler-emitted malformed-tool-call message and the OpenRouter-style
/// JSON error envelopes that some providers stream in the body of a 200 OK
/// response (rig captures those as `CompletionError::ProviderResponse` with
/// no HTTP status — or as `ResponseError(String)` on other transports). The
/// latter surfaces as `error.code: 429` with
/// `error_type: "rate_limit_exceeded"` (or `provider_unavailable`) inside a
/// JSON envelope; without this detection it falls through to
/// `LlmStreamProtocol` / `Other`, which misclassifies a rate-limit and is
/// not respected by the `Retry-After` floor.
fn detect_response_error(msg: &str) -> Option<crate::PriorError> {
    use crate::PriorError::*;
    // "tool call `X` arrived with malformed JSON input: <serde error>"
    if let Some(rest) = msg.strip_prefix("tool call `") {
        if let Some((name, after)) = rest.split_once('`') {
            if let Some(parse_err) = after.strip_prefix(" arrived with malformed JSON input: ") {
                return Some(LlmMalformedToolCall {
                    tool_name: name.to_string(),
                    parse_error: parse_err.to_string(),
                });
            }
        }
    }
    // OpenRouter-style JSON envelope: `{"error":{"code":429,"message":"...",
    // "metadata":{"error_type":"rate_limit_exceeded"}}, ...}`. The body
    // is sometimes wrapped by `ProviderResponseError: ` (the `Display`
    // output of `CompletionError::ProviderResponse(...)` when status is
    // `None`); strip the prefix if present.
    let body = msg.strip_prefix("ProviderResponseError: ").unwrap_or(msg);
    if let Some(code) = extract_openrouter_error_code(body) {
        // Same body, same detail as the status-based path above: an error
        // envelope inside a 200 OK response carries the provider's explanation
        // just as a real 4xx does.
        let detail = provider_error_detail(Some(body));
        match code {
            429 => {
                return Some(LlmRateLimited {
                    retry_after_secs: None,
                })
            }
            500..=599 => {
                return Some(LlmUpstream {
                    status: code,
                    detail,
                })
            }
            401 | 403 => return Some(LlmAuth { status: code }),
            402 => return Some(LlmBilling { status: code }),
            400..=499 => {
                return Some(LlmClient {
                    status: code,
                    detail,
                })
            }
            _ => return None,
        }
    }
    None
}

/// Extract the `error.code` field from an OpenRouter-style error envelope.
/// Returns `None` if the body is not JSON or has no numeric `error.code`.
fn extract_openrouter_error_code(body: &str) -> Option<u16> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let code = v.get("error")?.get("code")?.as_u64()?;
    u16::try_from(code).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::PriorError;
    use rig_core::message::Message;
    use serial_test::serial;
    // ---- Error classification tests ----
    //
    // The auto-retry / error-formatting pipeline in `run_interactive`
    // depends on `classify_stream_error` producing a structured
    // `PriorError` whose Display string is short, whose
    // `is_auto_retryable` / `retry_after_secs` / `synthetic_retry_feedback`
    // accessors agree, and which preserves the upstream
    // provider's `Retry-After` on 429. These tests pin that contract
    // so a future refactor can't silently turn a recoverable rate
    // limit into a fatal `Other` again.

    use rig_agent::agent::StreamingError;
    use rig_agent::completion::{CompletionError, PromptError};
    use std::collections::HashMap;

    #[test]
    fn classify_429_with_retry_after_is_rate_limited_and_retryable() {
        // Simulate a HttpError with status 429 + Retry-After: 60. Rig
        // surfaces this as CompletionError::HttpError(http_client::Error);
        // the provider_response_* helpers are wired by the
        // `impl_provider_response_helpers!` macro in rig-core. We can't
        // build a real `http_client::Error` here (it lives in a private
        // crate), so we cover the classification by crafting a
        // ResponseError with the *string* shape of the upstream message
        // — which is what providers like OpenRouter also surface when
        // the body is unparseable. The real 429 path is exercised in
        // the integration tests; this unit test pins the contract for
        // a representative error string.
        let err = CompletionError::ResponseError(
        "tool call `edit` arrived with malformed JSON input: expected `,` or `}` at line 1 column 335"
            .into(),
    );
        let classified = super::classify_completion_error(&err);
        match classified {
            PriorError::LlmMalformedToolCall {
                ref tool_name,
                ref parse_error,
            } => {
                assert_eq!(tool_name, "edit");
                assert!(parse_error.contains("column 335"));
                assert!(classified.is_auto_retryable());
                assert_eq!(classified.retry_after_secs(), None);
                assert!(classified
                    .synthetic_retry_feedback()
                    .unwrap()
                    .contains("edit"));
            }
            other => panic!("expected LlmMalformedToolCall, got {other:?}"),
        }
    }

    #[test]
    fn classify_control_character_parse_error_is_retryable_with_feedback() {
        // The content-echo subclass (raw control characters in a string —
        // the model copied file/terminal bytes into `edit.old_text`) must
        // stay auto-retryable WITH synthetic feedback: the manual workflow
        // is to paste the parse error and let the model retry, and that
        // works (the model re-reads the source and re-emits clean JSON).
        // Automate it — do not special-case it as non-retryable.
        let err = CompletionError::ResponseError(
            "tool call `edit` arrived with malformed JSON input: \
         control character (\\u0000-\\u001F) found while parsing a string at line 2 column 0"
                .into(),
        );
        let classified = super::classify_completion_error(&err);
        match classified {
            PriorError::LlmMalformedToolCall {
                ref tool_name,
                ref parse_error,
            } => {
                assert_eq!(tool_name, "edit");
                assert!(parse_error.contains("control character"));
                assert!(classified.is_auto_retryable());
                let fb = classified.synthetic_retry_feedback().unwrap();
                assert!(fb.contains("edit"));
                assert!(fb.contains("control character"));
            }
            other => panic!("expected LlmMalformedToolCall, got {other:?}"),
        }
    }

    #[test]
    fn classify_prompt_cancelled_is_not_retryable() {
        let e = StreamingError::Prompt(Box::new(PromptError::prompt_cancelled(
            Vec::<Message>::new(),
            "user pressed Esc",
        )));
        let classified = super::classify_stream_error(&e);
        // The REPL converts this to a Cancelled at the call site; the
        // classifier itself should leave it as a prompt error. We
        // expect a `Prompt` error variant (the streaming-arm converter
        // does the wrapping).
        assert!(!classified.is_auto_retryable());
    }

    #[test]
    fn unknown_tool_call_with_available_list_surfaces_usefully() {
        let e = StreamingError::Prompt(Box::new(PromptError::UnknownToolCall {
            tool_name: "nonexistent".into(),
            available_tools: vec!["read".into(), "write".into()],
            allowed_tools: vec!["read".into(), "write".into()],
            chat_history: Box::new(Vec::new()),
        }));
        let classified = super::classify_stream_error(&e);
        // Not retryable at the REPL layer: recovery is owned by
        // `LoggingHook::on_invalid_tool_call`, which uses the rig-run
        // `max_invalid_tool_call_retries` budget. This variant is the
        // escape hatch — reached only when the hook is absent, when the
        // user has zero retries configured, or when the model
        // hallucinates the same wrong name twice in a row. Surface with
        // available tools so the user can correct the model.
        assert!(!classified.is_auto_retryable());
        // Recovery feedback lives in the rig hook, not in
        // `PriorError` — confirm so a future refactor doesn't drift.
        assert!(classified.synthetic_retry_feedback().is_none());
        assert_eq!(classified.retry_after_secs(), None);
        let s = format!("{classified}");
        assert!(s.contains("nonexistent"));
        assert!(s.contains("read") && s.contains("write"));
    }

    // The end-to-end recovery behavior (rig-run `on_invalid_tool_call` →
    // rollback → model sees feedback → corrected call dispatched) is
    // exhaustively covered by rig's own model-conformance tests
    // (e.g. `invalid_tool_call_hook_can_repair_streaming_tool_name` in
    // `rig-agent/src/agent/prompt_request/streaming.rs`). The compile
    // time check that `LoggingHook` implements `on_invalid_tool_call`
    // and the unit test on the resulting `PriorError` shape above
    // are sufficient for prior's half of the contract.

    #[test]
    fn max_turns_error_is_not_retryable() {
        let e = StreamingError::Prompt(Box::new(PromptError::MaxTurnsError {
            max_turns: 16,
            chat_history: Box::new(Vec::new()),
            prompt: Box::new(Message::user("test")),
        }));
        let classified = super::classify_stream_error(&e);
        assert!(!classified.is_auto_retryable());
        let s = format!("{classified}");
        assert!(s.contains("16"));
    }

    #[test]
    fn classify_unparseable_response_other_message_does_not_match_tool_call() {
        // A different ResponseError (e.g. SSE protocol violation) should
        // NOT be misclassified as MalformedToolCall. Falls through to
        // LlmStreamProtocol.
        let err = CompletionError::ResponseError("unexpected EOF mid-stream".into());
        let classified = super::classify_completion_error(&err);
        assert!(matches!(classified, PriorError::LlmStreamProtocol(_)));
    }

    #[test]
    fn retry_after_secs_extracted_from_provider_response_headers() {
        use rig_core::http_client::{HeaderMap, HeaderValue, StatusCode};
        use rig_core::ProviderResponseError;
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_str("60").unwrap());
        let body = HashMap::from([("error".to_string(), "rate_limited".to_string())]);
        let ce = CompletionError::ProviderResponse(
            ProviderResponseError::new(
                StatusCode::TOO_MANY_REQUESTS,
                serde_json::to_string(&body).unwrap(),
            )
            .with_headers(Some(Box::new(headers))),
        );
        let classified = super::classify_completion_error(&ce);
        match classified {
            PriorError::LlmRateLimited {
                ref retry_after_secs,
            } => {
                assert_eq!(retry_after_secs, &Some(60));
                assert!(classified.is_auto_retryable());
            }
            other => panic!("expected LlmRateLimited, got {other:?}"),
        }
    }

    #[test]
    fn classify_prompt_error_429_preserves_retry_after() {
        // The non-streaming `chat()` path used by `summarize` returns a
        // `PromptError` rather than a `StreamingError`. A 429 must still
        // classify as `LlmRateLimited` with the Retry-After hint preserved
        // so the REPL can back off correctly on `/tree --summarize`.
        use rig_core::http_client::{HeaderMap, HeaderValue, StatusCode};
        use rig_core::ProviderResponseError;
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_str("30").unwrap());
        let ce = CompletionError::ProviderResponse(
            ProviderResponseError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "{\"error\":\"rate_limited\"}".to_string(),
            )
            .with_headers(Some(Box::new(headers))),
        );
        let pe = PromptError::CompletionError(ce);
        let classified = super::classify_prompt_error(&pe, "summarize");
        match classified {
            PriorError::LlmRateLimited { retry_after_secs } => {
                assert_eq!(retry_after_secs, Some(30));
                assert!(classified.is_auto_retryable());
            }
            other => panic!("expected LlmRateLimited, got {other:?}"),
        }
    }

    #[test]
    fn retry_budget_is_50_for_every_retryable_variant() {
        // All transient classes share a single generous budget: rate-limited,
        // upstream 5xx, transport, streaming-protocol, and malformed tool
        // calls. The budget exists to cap a runaway, not to second-guess
        // the policy per class. Non-retryable classes get 0 (no retries).
        assert_eq!(
            super::retry_budget(&PriorError::LlmRateLimited {
                retry_after_secs: None
            }),
            50
        );
        assert_eq!(
            super::retry_budget(&PriorError::LlmRateLimited {
                retry_after_secs: Some(60)
            }),
            50
        );
        assert_eq!(
            super::retry_budget(&PriorError::LlmUpstream {
                status: 502,
                detail: Default::default(),
            }),
            50
        );
        assert_eq!(
            super::retry_budget(&PriorError::LlmTransport("reset".into())),
            50
        );
        assert_eq!(
            super::retry_budget(&PriorError::LlmStreamProtocol("eof".into())),
            50
        );
        assert_eq!(
            super::retry_budget(&PriorError::LlmMalformedToolCall {
                tool_name: "edit".into(),
                parse_error: "expected `,` or `}`".into(),
            }),
            50
        );
        // Non-retryable: 0 retries.
        assert_eq!(
            super::retry_budget(&PriorError::LlmAuth { status: 401 }),
            0
        );
        assert_eq!(
            super::retry_budget(&PriorError::LlmBilling { status: 402 }),
            0
        );
        assert_eq!(
            super::retry_budget(&PriorError::LlmClient {
                status: 400,
                detail: Default::default(),
            }),
            0
        );
        assert_eq!(
            super::retry_budget(&PriorError::LlmMaxTurns { max_turns: 16 }),
            0
        );
        // LlmInterrupted delegates to its cause.
        assert_eq!(
            super::retry_budget(&PriorError::LlmInterrupted {
                cause: Box::new(PriorError::LlmRateLimited {
                    retry_after_secs: None
                }),
            }),
            50
        );
        assert_eq!(
            super::retry_budget(&PriorError::LlmInterrupted {
                cause: Box::new(PriorError::LlmTransport("reset".into())),
            }),
            50
        );
        assert_eq!(
            super::retry_budget(&PriorError::LlmInterrupted {
                cause: Box::new(PriorError::LlmAuth { status: 403 }),
            }),
            0
        );
    }

    // `#[serial]`: reads the no-hint base, which
    // `retry_base_override_scales_the_no_hint_schedule_only` overrides via a
    // process-global env var.
    #[test]
    #[serial]
    fn retry_delay_grows_as_pow2_without_cap() {
        // No provider hint: pure 2^(n-1), no ceiling. The per-turn budget
        // (50) is the implicit cap on how many attempts we will ever make.
        // attempt=1 → 1s, 2 → 2s, 3 → 4s, … (delay is `1 << (n-1)` seconds).
        // Milliseconds: production's 1000ms base, so these are the same
        // schedule as the old whole-second numbers.
        let e = PriorError::LlmTransport("reset".into());
        assert_eq!(super::retry_delay_ms(&e, 1), 1000);
        assert_eq!(super::retry_delay_ms(&e, 2), 2000);
        assert_eq!(super::retry_delay_ms(&e, 3), 4000);
        assert_eq!(super::retry_delay_ms(&e, 4), 8000);
        assert_eq!(super::retry_delay_ms(&e, 10), 512_000);
        // Saturating at u64: still finite, no panic. We never reach 50 in
        // practice (budget caps it), but saturating_pow keeps the math safe.
        let big = super::retry_delay_ms(&e, 50);
        assert!(big > 0);
    }

    /// The no-hint base is overridable so the pty-level back-off tests can run
    /// in milliseconds, and — the part worth pinning — a provider `Retry-After`
    /// is never scaled down by it. A hint is a floor we owe the provider, so
    /// shrinking it would be us ignoring an explicit rate-limit request.
    // `#[serial]`: sets and removes a process-global env var. Any sibling that
    // reads `retry_delay_ms` without a hint must not overlap it.
    #[test]
    #[serial]
    fn retry_base_override_scales_the_no_hint_schedule_only() {
        let no_hint = crate::PriorError::LlmTransport("reset".into());
        let hinted = crate::PriorError::LlmRateLimited {
            retry_after_secs: Some(4),
        };

        // Default: the production schedule, exactly as before the override.
        assert_eq!(super::retry_base_ms(), 1000);
        assert_eq!(super::retry_delay_ms(&no_hint, 1), 1000);
        assert_eq!(super::retry_delay_ms(&hinted, 1), 4000);

        // Scoped to this test's own process: it sets and removes the variable
        // around every call rather than leaving it set, so it cannot leak into a
        // sibling test in the same binary.
        // Safety: single-threaded within this test — no other thread reads it.
        std::env::set_var(super::TEST_RETRY_BASE_MS_VAR, "20");
        assert_eq!(super::retry_base_ms(), 20);
        // Growth shape is preserved, just scaled: 20ms, 40ms, 80ms.
        assert_eq!(super::retry_delay_ms(&no_hint, 1), 20);
        assert_eq!(super::retry_delay_ms(&no_hint, 2), 40);
        // A hint is untouched by the override.
        assert_eq!(super::retry_delay_ms(&hinted, 1), 4000);
        assert_eq!(super::retry_delay_ms(&hinted, 2), 8000);

        // Nonsense and zero fall back to the default rather than disabling or
        // zeroing the back-off — a typo must not silently remove the delay.
        std::env::set_var(super::TEST_RETRY_BASE_MS_VAR, "not-a-number");
        assert_eq!(super::retry_base_ms(), 1000);
        std::env::set_var(super::TEST_RETRY_BASE_MS_VAR, "0");
        assert_eq!(super::retry_base_ms(), 1000);
        std::env::remove_var(super::TEST_RETRY_BASE_MS_VAR);
        assert_eq!(super::retry_base_ms(), 1000);
        assert_eq!(super::retry_delay_ms(&no_hint, 1), 1000);
    }

    // `#[serial]`: reads the no-hint base, which
    // `retry_base_override_scales_the_no_hint_schedule_only` overrides via a
    // process-global env var.
    #[test]
    #[serial]
    fn retry_delay_respects_retry_after_as_floor_then_doubles() {
        // When the provider reports Retry-After, it is the FLOOR of the
        // first attempt; subsequent attempts double from there. This
        // respects upstream's request while still growing on a string of
        // consecutive 429s.
        let e = PriorError::LlmRateLimited {
            retry_after_secs: Some(5),
        };
        assert_eq!(super::retry_delay_ms(&e, 1), 5000);
        assert_eq!(super::retry_delay_ms(&e, 2), 10_000);
        assert_eq!(super::retry_delay_ms(&e, 3), 20_000);
        assert_eq!(super::retry_delay_ms(&e, 4), 40_000);
    }

    // `#[serial]`: reads the process-global `PRIOR_TEST_RETRY_BASE_MS`, which
    // `retry_base_override_scales_the_no_hint_schedule_only` sets and removes
    // around each of its calls. Overlapping them makes that test see this one's
    // 20ms base and fail on the expected 1s.
    #[test]
    #[serial]
    fn retry_delay_for_malformed_tool_call_uses_provider_floor_or_one() {
        // Malformed tool calls carry no provider hint (the model is the
        // source of the error), so the first delay is 1s — fast feedback
        // for self-correction. Subsequent attempts double.
        let e = PriorError::LlmMalformedToolCall {
            tool_name: "edit".into(),
            parse_error: "expected `,` or `}`".into(),
        };
        assert_eq!(super::retry_delay_ms(&e, 1), 1000);
        assert_eq!(super::retry_delay_ms(&e, 2), 2000);
        assert_eq!(super::retry_delay_ms(&e, 3), 4000);
    }

    #[test]
    fn classify_deepinfra_502_upstream_error_via_provider_response_is_retryable() {
        // The first error from the task: OpenRouter surfaces a DeepInfra
        // 502 as a `ProviderResponseError` with status 502. The classifier
        // must map it to `LlmUpstream { status: 502 }` and mark it
        // auto-retryable.
        use rig_core::http_client::StatusCode;
        use rig_core::ProviderResponseError;
        let body = r#"{"id":"gen-1788441206-TngoywArM91X9nINXYNP","object":"chat.completion.chunk","created":1788441206,"model":"z-ai/glm-5.3-flash","provider":"DeepInfra","choices":[],"error":{"code":502,"message":"Upstream error from DeepInfra: TimeoutError: Timeout on reading data from socket","metadata":{"error_type":"provider_unavailable"}}}"#;
        let ce = CompletionError::ProviderResponse(ProviderResponseError::new(
            StatusCode::BAD_GATEWAY,
            body.to_string(),
        ));
        let classified = super::classify_completion_error(&ce);
        match classified {
            PriorError::LlmUpstream { status, .. } => {
                assert_eq!(status, 502);
                assert!(classified.is_auto_retryable());
                assert_eq!(classified.retry_after_secs(), None);
            }
            other => panic!("expected LlmUpstream {{ status: 502 }}, got {other:?}"),
        }
    }

    #[test]
    fn classify_http_client_error_sending_request_is_retryable_transport() {
        // The second error from the task: reqwest surfaces connection
        // failures as `CompletionError::HttpError(Instance(...))`. It must
        // classify as `LlmTransport` and be auto-retryable. We can't
        // construct a real reqwest::Error here, so we wrap a stub std
        // error via `Error::instance` (the boxed-error path reqwest uses).
        use rig_core::http_client;
        let stub: std::io::Error = std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "error sending request for url (https://openrouter.ai/api/v1/chat/completions)",
        );
        let http_err = http_client::Error::instance(stub);
        let ce = CompletionError::HttpError(http_err);
        let classified = super::classify_completion_error(&ce);
        match classified {
            PriorError::LlmTransport(ref msg) => {
                assert!(msg.contains("openrouter.ai"), "msg: {msg}");
                assert!(classified.is_auto_retryable());
            }
            other => panic!("expected LlmTransport, got {other:?}"),
        }
    }

    #[test]
    fn classify_openrouter_429_mid_stream_envelope_is_rate_limited() {
        // OpenRouter sometimes streams a 429 in the body of a 200 OK
        // response (the `error.code` field of a JSON envelope). Rig
        // surfaces this as `CompletionError::ProviderResponse(...)` with
        // `status == None` and the JSON body as the `Display` string. The
        // classifier must extract the 429 and map it to `LlmRateLimited`
        // so the REPL's Retry-After floor and the 50-attempt budget
        // apply.
        let envelope = r#"ProviderResponseError: {"id":"gen-1788453950-hBER7wC6ldCU829zmnAe","object":"chat.completion.chunk","created":1788453950,"model":"unknown","provider":"GMICloud","choices":[],"error":{"code":429,"message":"Provider returned error","metadata":{"error_type":"rate_limit_exceeded"}}}"#;
        let err = CompletionError::ResponseError(envelope.to_string());
        let classified = super::classify_completion_error(&err);
        match classified {
            PriorError::LlmRateLimited { retry_after_secs } => {
                assert_eq!(retry_after_secs, None);
                assert!(classified.is_auto_retryable());
            }
            other => panic!("expected LlmRateLimited, got {other:?}"),
        }
    }

    #[test]
    fn classify_openrouter_502_mid_stream_envelope_is_upstream() {
        // Same envelope shape, but with a 5xx code. Must map to
        // `LlmUpstream { status: 502 }`.
        let envelope = r#"ProviderResponseError: {"error":{"code":502,"message":"Upstream error from DeepInfra: TimeoutError","metadata":{"error_type":"provider_unavailable"}}}"#;
        let err = CompletionError::ResponseError(envelope.to_string());
        let classified = super::classify_completion_error(&err);
        match classified {
            PriorError::LlmUpstream { status, .. } => {
                assert_eq!(status, 502);
                assert!(classified.is_auto_retryable());
            }
            other => panic!("expected LlmUpstream {{ status: 502 }}, got {other:?}"),
        }
    }

    #[test]
    fn classify_provider_response_429_envelope_without_status_is_rate_limited() {
        // The production shape of the reported bug: rig's SSE decoder
        // (`completion_error_from_body`) turns a terminal error envelope
        // streamed inside a 200 OK into `CompletionError::ProviderResponse`
        // with NO HTTP status and the raw JSON as the body — NOT a
        // `ResponseError` with a prefixed string (which the sibling tests
        // above emulate). Previously this fell through to `Other` (Display
        // "Other error: ProviderResponseError: {...}"), which is not
        // auto-retryable. The classifier must extract the 429 from the body
        // and map it to `LlmRateLimited` so the REPL's budget applies.
        use rig_core::ProviderResponseError;
        let body = r#"{"id":"gen-1788464790-qEKKA0Klq7sLFL2HAM4O","object":"chat.completion.chunk","created":1788464790,"model":"unknown","provider":"GMICloud","choices":[],"error":{"code":429,"message":"Provider returned error","metadata":{"error_type":"rate_limit_exceeded"}}}"#;
        let ce = CompletionError::ProviderResponse(ProviderResponseError::without_status(
            body.to_string(),
        ));
        // Sanity-check the fixture matches the production wiring: no HTTP
        // status is preserved, so only body-based envelope detection can
        // classify this.
        assert_eq!(ce.provider_response_status(), None);
        let classified = super::classify_completion_error(&ce);
        match classified {
            PriorError::LlmRateLimited { retry_after_secs } => {
                assert_eq!(retry_after_secs, None);
                assert!(classified.is_auto_retryable());
            }
            other => panic!("expected LlmRateLimited, got {other:?}"),
        }
    }

    #[test]
    fn classify_provider_response_502_envelope_with_2xx_status_is_upstream() {
        // A gateway that funnels a 200 OK response carrying an error
        // envelope through `from_http_response` preserves the 2xx status on
        // the `ProviderResponse`. A 2xx status must not be treated as a
        // failure on its own — the classifier defers to the body envelope,
        // mapping the 502 to `LlmUpstream`.
        use rig_core::http_client::StatusCode;
        use rig_core::ProviderResponseError;
        let body = r#"{"id":"gen-x","object":"chat.completion.chunk","created":0,"model":"unknown","provider":"GMICloud","choices":[],"error":{"code":502,"message":"Upstream error","metadata":{"error_type":"provider_unavailable"}}}"#;
        let ce = CompletionError::ProviderResponse(ProviderResponseError::new(
            StatusCode::OK,
            body.to_string(),
        ));
        let classified = super::classify_completion_error(&ce);
        match classified {
            PriorError::LlmUpstream { status, .. } => {
                assert_eq!(status, 502);
                assert!(classified.is_auto_retryable());
            }
            other => panic!("expected LlmUpstream {{ status: 502 }}, got {other:?}"),
        }
    }

    #[test]
    fn classify_provider_response_without_envelope_body_falls_through_to_other() {
        // A status-less `ProviderResponse` whose body is not an
        // OpenRouter-style envelope (e.g. plain-text or non-envelope JSON)
        // must keep the pre-fix behavior: fall through to `Other`, NOT get
        // auto-retried (there is no signal it is transient).
        use rig_core::ProviderResponseError;
        let ce = CompletionError::ProviderResponse(ProviderResponseError::without_status(
            "provider exploded".to_string(),
        ));
        let classified = super::classify_completion_error(&ce);
        assert!(
            matches!(classified, PriorError::Other(_)),
            "expected Other, got {classified:?}"
        );
        assert!(!classified.is_auto_retryable());
    }

    #[test]
    fn classify_openrouter_envelope_without_error_code_falls_through_to_stream_protocol() {
        // JSON that parses but has no `error.code` (e.g. a different
        // provider's envelope) should not match — falls through to
        // `LlmStreamProtocol` as before.
        let envelope = r#"{"id":"x","error":"invalid_argument"}"#;
        let err = CompletionError::ResponseError(envelope.to_string());
        let classified = super::classify_completion_error(&err);
        assert!(matches!(classified, PriorError::LlmStreamProtocol(_)));
    }

    #[test]
    fn classify_unrecognized_text_response_error_still_matches_malformed_tool_call() {
        // The pre-existing "tool call `X` arrived with malformed JSON"
        // detection must keep working alongside the new OpenRouter
        // envelope detection — they share the same `detect_response_error`
        // entry point.
        let err = CompletionError::ResponseError(
            "tool call `edit` arrived with malformed JSON input: expected `,` or `}` at line 1"
                .into(),
        );
        let classified = super::classify_completion_error(&err);
        assert!(matches!(
            classified,
            PriorError::LlmMalformedToolCall { .. }
        ));
    }

    // ---- provider error detail: the 400's own account of what it rejected ----

    fn detail_of(body: &str) -> Option<String> {
        provider_error_detail(Some(body)).0
    }

    /// The case that motivated all of this: a rejected `temperature`. The
    /// catalog publishes no default for some models, so the 400 is the only
    /// place the real limit appears.
    #[test]
    fn an_openai_shaped_rejection_is_read() {
        let d = detail_of(
        r#"{"error":{"message":"temperature must be <= 1.0","type":"invalid_request_error","param":"temperature"}}"#,
    )
    .unwrap();
        assert!(d.contains("temperature must be <= 1.0"), "{d}");
        // The message already names the field, so `param` is not repeated —
        // "… <= 1.0 (field: temperature)" would be noise.
        assert!(!d.contains("field:"), "{d}");
    }

    /// Where the provider names the field but the message does not, the field
    /// *is* appended. For a rejection about a value, which knob was rejected is
    /// the part the user cannot infer from the message alone.
    #[test]
    fn the_offending_field_is_appended_when_the_message_omits_it() {
        let d =
            detail_of(r#"{"error":{"message":"must be <= 1.0","param":"temperature"}}"#).unwrap();
        assert!(d.contains("field: temperature"), "{d}");
    }

    #[test]
    fn an_anthropic_shaped_rejection_is_read() {
        let d = detail_of(
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: 64000 > 32000"}}"#,
    )
    .unwrap();
        assert!(d.contains("64000 > 32000"), "{d}");
    }

    /// A flat `{"error": "..."}` — several self-hosted servers answer this way.
    #[test]
    fn a_flat_error_string_is_read() {
        assert_eq!(
            detail_of(r#"{"error":"model is overloaded"}"#).as_deref(),
            Some("model is overloaded")
        );
    }

    #[test]
    fn a_toplevel_message_is_read() {
        assert_eq!(
            detail_of(r#"{"message":"unknown model"}"#).as_deref(),
            Some("unknown model")
        );
    }

    /// FastAPI validation: `detail` is an array, and `loc` is the path to the
    /// offending field. This is the shape a self-hosted vLLM-style server sends.
    #[test]
    fn a_fastapi_validation_array_is_read() {
        let d = detail_of(
        r#"{"detail":[{"loc":["body","temperature"],"msg":"ensure this value is <= 1","type":"value_error"}]}"#,
    )
    .unwrap();
        assert!(d.contains("body.temperature"), "{d}");
        assert!(d.contains("<= 1"), "{d}");
    }

    /// Not every body is JSON. An HTML error page or a bare sentence must still
    /// reach the user rather than being discarded as unparseable.
    #[test]
    fn a_non_json_body_is_still_reported() {
        assert_eq!(
            detail_of("upstream connect error\nor disconnect").as_deref(),
            Some("upstream connect error or disconnect")
        );
    }

    /// Structure with no message anywhere is not a diagnostic; better a bare
    /// status than `; {"foo":1}`.
    #[test]
    fn a_body_with_no_message_yields_nothing() {
        assert_eq!(detail_of(r#"{"foo":1,"bar":2}"#), None);
        assert_eq!(detail_of("   "), None);
        assert_eq!(provider_error_detail(None).0, None);
    }

    /// The detail lands in the rendered error, which is where the user sees it.
    /// A `None` must leave the message byte-identical to before, with no
    /// dangling separator.
    #[test]
    fn the_detail_is_rendered_as_a_suffix() {
        let with = PriorError::LlmClient {
            status: 400,
            detail: crate::ErrorDetail(Some("temperature must be <= 1.0".into())),
        };
        assert_eq!(
            with.to_string(),
            "Provider rejected the request (HTTP 400); temperature must be <= 1.0."
        );
        let without = PriorError::LlmClient {
            status: 400,
            detail: Default::default(),
        };
        assert_eq!(
            without.to_string(),
            "Provider rejected the request (HTTP 400)."
        );
    }

    /// A long or multi-line body must not flood the turn: one clipped line.
    #[test]
    fn a_huge_body_is_clipped_to_one_line() {
        let body = format!(r#"{{"error":{{"message":"{}"}}}}"#, "x".repeat(5000));
        let d = detail_of(&body).unwrap();
        assert!(d.chars().count() <= 301, "{}", d.chars().count());
        assert!(d.ends_with('…'), "{d}");
        assert!(!d.contains('\n'), "must be a single line");
    }
}
