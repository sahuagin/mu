//! Shared rendering of a non-2xx provider reply into the one-line error
//! string the agent loop classifies (mu-core `retryable_provider_error`)
//! and paces (`server_suggested_delay_ms`). Every lane used to build its
//! own `"{lane} returned {status}: {raw body}"`, which dropped two things
//! the OpenAI changelog of 2026-09-02 made load-bearing (mu #595):
//!
//! - the error body's `code` — `429 slow_down` (a traffic ramp, back off
//!   briefly) is not `429 usage_limit_reached` (a subscription cap, wait
//!   for the window), and `503 server_is_overloaded` (temporary, retry)
//!   is not a generic 503;
//! - the `Retry-After` header, which the server sends with both and which
//!   should win over exponential backoff.
//!
//! The rendered shape is `"{lane} returned {status} {code} ({type}):
//! {message} {other fields} (retry after {n}s)"`, each part present only
//! when the reply carried it: `type` only when it differs from `code`,
//! the other `error.*` fields as compact JSON so a discriminator that sits
//! in one of them (OpenRouter's `metadata`, say) still reaches the
//! substring classifier as the raw body did before. The `"{lane} returned {status}"` prefix is
//! unchanged from before, so every existing classifier rule keeps
//! matching; the code and the suffix are what the loop gains.
//!
//! Two body shapes are understood: OpenAI `{"error":{"code","type",
//! "message"}}` (the Responses API, the chatgpt backend, and vLLM
//! speaking chat-completions; OpenRouter uses the same envelope with a
//! NUMERIC `code`, which is accepted too) and Anthropic `{"type":"error",
//! "error":{"type","message"}}` (the Messages API and ollama's
//! implementation of it), where `error.type` plays the role of the code.
//! Anything else is rendered raw.

use reqwest::header::HeaderMap;
use reqwest::StatusCode;
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};

/// The parts of a provider error body the loop cares about, plus
/// everything else the envelope carried so nothing a classifier keys on
/// can vanish in rendering.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ErrorBody {
    /// OpenAI `error.code`; for the Anthropic shape, `error.type`.
    pub code: Option<String>,
    /// OpenAI `error.type` (absent for the Anthropic shape, whose `type`
    /// already became `code`).
    pub kind: Option<String>,
    pub message: Option<String>,
    /// Every other field of the body — `error.*` siblings of code/type/
    /// message, and top-level siblings of `error` — compact JSON,
    /// key-sorted. OpenRouter puts the upstream vendor's error under
    /// `error.metadata`, a proxy may add a top-level request id. Empty
    /// when there is none.
    pub extra: String,
}

/// Parse an OpenAI- or Anthropic-shaped error body. Fields that are not
/// there stay `None`; a body that is not JSON, or JSON without an `error`
/// object, yields the default.
pub fn parse_error_body(body: &str) -> ErrorBody {
    #[derive(Deserialize)]
    struct Envelope {
        error: Option<Inner>,
        /// Top-level siblings of `error` (a request id, a proxy's own
        /// `detail`, Anthropic's `"type":"error"`): carried like the
        /// nested extras so the rendered line drops nothing the raw body had.
        #[serde(flatten)]
        rest: std::collections::BTreeMap<String, serde_json::Value>,
    }
    #[derive(Deserialize)]
    struct Inner {
        /// A string on the OpenAI wire (`"slow_down"`), a number on
        /// OpenRouter's (`429`); anything else is treated as absent.
        #[serde(default)]
        code: Option<serde_json::Value>,
        #[serde(default, rename = "type")]
        type_: Option<String>,
        #[serde(default)]
        message: Option<String>,
        #[serde(flatten)]
        rest: std::collections::BTreeMap<String, serde_json::Value>,
    }
    let Ok(Envelope {
        error: Some(e),
        rest: top,
    }) = serde_json::from_str::<Envelope>(body)
    else {
        return ErrorBody::default();
    };
    let code = match e.code {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        _ => None,
    };
    // Anthropic's constant `"type":"error"` marker says nothing; every
    // other top-level sibling is kept alongside the nested extras.
    let mut all = e.rest;
    for (k, v) in top {
        if k == "type" && v == serde_json::Value::String("error".into()) {
            continue;
        }
        all.entry(k).or_insert(v);
    }
    let extra = if all.is_empty() {
        String::new()
    } else {
        serde_json::to_string(&all).unwrap_or_default()
    };
    match code {
        // OpenAI: `code` is the machine-readable discriminator.
        Some(code) => ErrorBody {
            code: Some(code),
            kind: e.type_,
            message: e.message,
            extra,
        },
        // Anthropic (and OpenAI bodies without a code): `type` is.
        None => ErrorBody {
            code: e.type_,
            kind: None,
            message: e.message,
            extra,
        },
    }
}

/// `Retry-After` as whole seconds from now: the delta-seconds form
/// verbatim, the HTTP-date form as its distance into the future. Absent,
/// unparseable, zero, or a date already past (including one that only
/// looks past because of clock skew) ⇒ `None`, so the loop falls back to
/// its own backoff rather than retrying at once against the server that
/// just refused it.
pub fn retry_after_secs(headers: &HeaderMap) -> Option<u64> {
    let raw = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    let secs = match raw.parse::<u64>() {
        Ok(secs) => secs,
        Err(_) => {
            let at = parse_imf_fixdate(raw)?;
            let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
            at.saturating_sub(now)
        }
    };
    (secs > 0).then_some(secs)
}

/// Parse an RFC 7231 IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) to
/// unix seconds. The one date form the header is required to use;
/// the two obsolete forms are not accepted (⇒ backoff instead). The
/// value is server-controlled, so every component is parsed unsigned and
/// range-checked before any arithmetic: the year to four digits, the day
/// to 1..=31, hour/minute/second to their clock ranges. With those bounds
/// the largest intermediate is under 2^38, so the i64 arithmetic below
/// cannot overflow.
fn parse_imf_fixdate(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    let [_wkday, dd, mon, yyyy, hms, "GMT"] = parts.as_slice() else {
        return None;
    };
    let day = i64::from(dd.parse::<u8>().ok().filter(|d| (1..=31).contains(d))?);
    let year = i64::from(
        yyyy.parse::<u16>()
            .ok()
            .filter(|y| (1970..=9999).contains(y))?,
    );
    let month: i64 = match *mon {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let mut t = hms.split(':');
    let h = i64::from(t.next()?.parse::<u8>().ok().filter(|h| *h <= 23)?);
    let m = i64::from(t.next()?.parse::<u8>().ok().filter(|m| *m <= 59)?);
    let sec = i64::from(t.next()?.parse::<u8>().ok().filter(|s| *s <= 60)?);
    if t.next().is_some() {
        return None;
    }
    // Days from civil (proleptic Gregorian), Howard Hinnant's algorithm.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3_600 + m * 60 + sec;
    u64::try_from(secs).ok()
}

/// Render a non-success reply. See the module docs for the shape.
pub fn render_http_error(
    lane: &str,
    status: StatusCode,
    headers: &HeaderMap,
    body: &str,
) -> String {
    render_with_retry_after(lane, status, retry_after_secs(headers), body)
}

/// The rendering step with `Retry-After` already reduced to seconds, so
/// callers holding only a body (and tests) can use it too.
pub fn render_with_retry_after(
    lane: &str,
    status: StatusCode,
    retry_after: Option<u64>,
    body: &str,
) -> String {
    let parsed = parse_error_body(body);
    // `code`, then `type` when the wire carried both and they differ —
    // the never-retry discriminators mu-core keys on live in either.
    let discriminator = match (&parsed.code, &parsed.kind) {
        (Some(code), Some(kind)) if kind != code => format!(" {code} ({kind})"),
        (Some(code), _) => format!(" {code}"),
        (None, _) => String::new(),
    };
    let mut out = match &parsed.message {
        Some(msg) => format!("{lane} returned {status}{discriminator}: {msg}"),
        None if !discriminator.is_empty() => format!("{lane} returned {status}{discriminator}"),
        None => format!("{lane} returned {status}: {}", body.trim()),
    };
    if !parsed.extra.is_empty() {
        out.push_str(&format!(" {}", parsed.extra));
    }
    if let Some(secs) = retry_after {
        out.push_str(&format!(" (retry after {secs}s)"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderValue, RETRY_AFTER};

    fn headers(retry_after: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(v) = retry_after {
            h.insert(RETRY_AFTER, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn openai_slow_down_with_retry_after_seconds() {
        let body = r#"{"error":{"code":"slow_down","type":"rate_limit_error","message":"Traffic is ramping too fast."}}"#;
        let msg = render_http_error(
            "openai",
            StatusCode::TOO_MANY_REQUESTS,
            &headers(Some("12")),
            body,
        );
        assert_eq!(
            msg,
            "openai returned 429 Too Many Requests slow_down (rate_limit_error): Traffic is ramping too fast. (retry after 12s)"
        );
        // Same code and type: said once.
        let body = r#"{"error":{"code":"insufficient_quota","type":"insufficient_quota","message":"No credit."}}"#;
        let msg = render_http_error(
            "openai",
            StatusCode::TOO_MANY_REQUESTS,
            &headers(None),
            body,
        );
        assert_eq!(
            msg,
            "openai returned 429 Too Many Requests insufficient_quota: No credit."
        );
    }

    /// Nothing the substring classifier keys on may vanish in rendering:
    /// a discriminator that sits outside code/type/message — OpenRouter's
    /// proxied upstream error under `metadata` — still reaches the string.
    #[test]
    fn other_envelope_fields_are_carried_not_dropped() {
        let body = r#"{"error":{"code":429,"message":"Provider returned error","metadata":{"provider_name":"Anthropic","raw":"{\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}"}}}"#;
        let msg = render_http_error(
            "openrouter",
            StatusCode::TOO_MANY_REQUESTS,
            &headers(None),
            body,
        );
        assert!(msg.starts_with("openrouter returned 429 Too Many Requests 429: Provider returned error {\"metadata\":"), "got: {msg}");
        assert!(msg.contains("overloaded_error"), "got: {msg}");
        assert!(msg.contains("provider_name"), "got: {msg}");
        // A never-retry token living only in an extra field is still visible.
        let body = r#"{"error":{"code":"x","message":"m","param":"usage_limit_reached"}}"#;
        let msg = render_http_error(
            "openai",
            StatusCode::TOO_MANY_REQUESTS,
            &headers(None),
            body,
        );
        assert!(msg.contains("usage_limit_reached"), "got: {msg}");
    }

    #[test]
    fn openai_server_is_overloaded_with_http_date() {
        let body = r#"{"error":{"code":"server_is_overloaded","type":"server_error","message":"The model is temporarily overloaded."}}"#;
        // A date far in the future: the delta is large and positive.
        let msg = render_http_error(
            "openrouter",
            StatusCode::SERVICE_UNAVAILABLE,
            &headers(Some("Sun, 06 Nov 2094 08:49:37 GMT")),
            body,
        );
        assert!(
            msg.starts_with("openrouter returned 503 Service Unavailable server_is_overloaded (server_error): The model is temporarily overloaded. (retry after "),
            "got: {msg}"
        );
        let secs: u64 = msg
            .rsplit("retry after ")
            .next()
            .unwrap()
            .trim_end_matches("s)")
            .parse()
            .unwrap();
        assert!(
            secs > 1_000_000_000,
            "far-future date should be a large delta, got {secs}"
        );
        // A date in the past (or one that only looks past through clock
        // skew) is no suggestion at all: the loop must back off, not fire.
        let past = render_http_error(
            "openrouter",
            StatusCode::SERVICE_UNAVAILABLE,
            &headers(Some("Sun, 06 Nov 1994 08:49:37 GMT")),
            body,
        );
        assert!(!past.contains("retry after"), "got: {past}");
        let zero = render_http_error(
            "openrouter",
            StatusCode::SERVICE_UNAVAILABLE,
            &headers(Some("0")),
            body,
        );
        assert!(!zero.contains("retry after"), "got: {zero}");
    }

    #[test]
    fn openrouter_numeric_code_is_accepted() {
        let body = r#"{"error":{"code":429,"message":"Rate limit exceeded","metadata":{"provider_name":"x"}}}"#;
        let msg = render_http_error(
            "openrouter",
            StatusCode::TOO_MANY_REQUESTS,
            &headers(Some("5")),
            body,
        );
        assert_eq!(
            msg,
            "openrouter returned 429 Too Many Requests 429: Rate limit exceeded {\"metadata\":{\"provider_name\":\"x\"}} (retry after 5s)"
        );
        assert_eq!(
            parse_error_body(r#"{"error":{"code":null,"type":"t","message":"m"}}"#),
            ErrorBody {
                code: Some("t".into()),
                kind: None,
                message: Some("m".into()),
                extra: String::new(),
            }
        );
    }

    #[test]
    fn absurd_dates_are_rejected_without_arithmetic() {
        assert_eq!(
            parse_imf_fixdate("Sun, 06 Nov 999999999999 08:49:37 GMT"),
            None
        );
        assert_eq!(parse_imf_fixdate("Sun, 06 Nov 1969 08:49:37 GMT"), None);
        assert_eq!(parse_imf_fixdate("Sun, 06 Nov -1 08:49:37 GMT"), None);
        // Time components are unsigned and clock-bounded: no sign, no overflow path.
        assert_eq!(
            parse_imf_fixdate("Sun, 06 Nov 1994 -9223372036854775:49:37 GMT"),
            None
        );
        assert_eq!(parse_imf_fixdate("Sun, 06 Nov 1994 08:-1:37 GMT"), None);
        assert_eq!(parse_imf_fixdate("Sun, 06 Nov 1994 24:00:00 GMT"), None);
        assert_eq!(parse_imf_fixdate("Sun, 32 Nov 1994 08:49:37 GMT"), None);
        assert_eq!(
            parse_imf_fixdate("Sun, 06 Nov 1994 23:59:60 GMT"),
            Some(784_166_400)
        );
        assert_eq!(
            parse_imf_fixdate("Fri, 31 Dec 9999 23:59:59 GMT"),
            Some(253_402_300_799)
        );
    }

    #[test]
    fn anthropic_overloaded_uses_error_type_as_code() {
        let body = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let msg = render_http_error(
            "anthropic",
            StatusCode::from_u16(529).unwrap(),
            &headers(Some("3")),
            body,
        );
        assert_eq!(msg, "anthropic returned 529 <unknown status code> overloaded_error: Overloaded (retry after 3s)");
    }

    #[test]
    fn raw_body_when_not_an_error_envelope() {
        let msg = render_http_error(
            "vllm",
            StatusCode::BAD_GATEWAY,
            &headers(None),
            "  upstream connect error\n",
        );
        assert_eq!(msg, "vllm returned 502 Bad Gateway: upstream connect error");
        let msg = render_http_error(
            "openai",
            StatusCode::INTERNAL_SERVER_ERROR,
            &headers(None),
            r#"{"detail":"boom"}"#,
        );
        assert_eq!(
            msg,
            r#"openai returned 500 Internal Server Error: {"detail":"boom"}"#
        );
    }

    #[test]
    fn unparseable_retry_after_is_ignored() {
        let body = r#"{"error":{"code":"slow_down","message":"x"}}"#;
        let msg = render_http_error(
            "openai",
            StatusCode::TOO_MANY_REQUESTS,
            &headers(Some("soon")),
            body,
        );
        assert_eq!(msg, "openai returned 429 Too Many Requests slow_down: x");
        assert_eq!(parse_imf_fixdate("Sunday, 06-Nov-94 08:49:37 GMT"), None);
        assert_eq!(parse_imf_fixdate("Sun Nov  6 08:49:37 1994"), None);
    }

    #[test]
    fn imf_fixdate_epoch_arithmetic() {
        // RFC 7231's own example.
        assert_eq!(
            parse_imf_fixdate("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(parse_imf_fixdate("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            parse_imf_fixdate("Tue, 29 Feb 2000 12:00:00 GMT"),
            Some(951_825_600)
        );
    }

    #[test]
    fn parse_error_body_shapes() {
        assert_eq!(
            parse_error_body(
                r#"{"error":{"code":"slow_down","type":"rate_limit_error","message":"m"}}"#
            ),
            ErrorBody {
                code: Some("slow_down".into()),
                kind: Some("rate_limit_error".into()),
                message: Some("m".into()),
                extra: String::new(),
            }
        );
        assert_eq!(
            parse_error_body(
                r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
            ),
            ErrorBody {
                code: Some("overloaded_error".into()),
                kind: None,
                message: Some("Overloaded".into()),
                extra: String::new(),
            }
        );
        assert_eq!(parse_error_body("not json"), ErrorBody::default());
        assert_eq!(
            parse_error_body(r#"{"error":"a bare string"}"#),
            ErrorBody::default()
        );
    }

    /// Top-level siblings of `error` are carried too; Anthropic's constant
    /// `"type":"error"` is the one sibling not worth repeating.
    #[test]
    fn top_level_siblings_of_error_are_carried() {
        let body = r#"{"type":"error","request_id":"req_123","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let msg = render_http_error(
            "anthropic",
            StatusCode::from_u16(529).unwrap(),
            &headers(None),
            body,
        );
        assert_eq!(
            msg,
            r#"anthropic returned 529 <unknown status code> overloaded_error: Overloaded {"request_id":"req_123"}"#
        );
        let body = r#"{"detail":"insufficient_quota","error":{"code":"x","message":"m"}}"#;
        let msg = render_http_error(
            "openai",
            StatusCode::TOO_MANY_REQUESTS,
            &headers(None),
            body,
        );
        assert!(msg.contains("insufficient_quota"), "got: {msg}");
        // Nested extras and top-level ones merge; the nested key wins a clash.
        let body = r#"{"note":"outer","error":{"code":"x","message":"m","note":"inner"}}"#;
        assert_eq!(parse_error_body(body).extra, r#"{"note":"inner"}"#);
    }
}
