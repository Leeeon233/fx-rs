use std::borrow::Cow;

const REDACTED: &[u8] = b"[redacted]";

/// Masks common credential assignments and token formats before text reaches
/// a model, protocol notification, or durable tool-result sidecar.
pub fn redact_secrets(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut output = Vec::new();
    let mut copied = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        let Some((prefix_end, value_end)) = secret_span(bytes, index) else {
            index += 1;
            continue;
        };
        output.extend_from_slice(&bytes[copied..prefix_end]);
        output.extend_from_slice(REDACTED);
        copied = value_end;
        index = value_end;
    }
    if output.is_empty() {
        return Cow::Borrowed(text);
    }
    output.extend_from_slice(&bytes[copied..]);
    Cow::Owned(String::from_utf8(output).expect("redacting UTF-8 preserves UTF-8"))
}

fn secret_span(text: &[u8], start: usize) -> Option<(usize, usize)> {
    basic_auth_url(text, start)
        .or_else(|| aws_access_key(text, start))
        .or_else(|| sensitive_assignment(text, start))
        .or_else(|| inline_token(text, start))
}

fn basic_auth_url(text: &[u8], start: usize) -> Option<(usize, usize)> {
    let prefix = b"https://";
    if !text.get(start..)?.starts_with(prefix) {
        return None;
    }
    let credential_start = start + prefix.len();
    let mut colon = false;
    for (index, byte) in text.iter().copied().enumerate().skip(credential_start) {
        match byte {
            b':' => colon = true,
            b'@' if colon && index > credential_start => {
                return Some((credential_start, index));
            }
            b'/' | b'?' | b'#' | b'\n' | b'\r' | b'\t' | b' ' => return None,
            _ => {}
        }
    }
    None
}

fn aws_access_key(text: &[u8], start: usize) -> Option<(usize, usize)> {
    let candidate = text.get(start..start.saturating_add(20))?;
    if ![b"AKIA", b"ASIA", b"AIDA", b"AGPA", b"AROA", b"ANPA"]
        .iter()
        .any(|prefix| candidate.starts_with(*prefix))
        || !candidate
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        || start
            .checked_sub(1)
            .is_some_and(|index| is_token(text[index]))
        || text.get(start + 20).is_some_and(|byte| is_token(*byte))
    {
        return None;
    }
    Some((start, start + 20))
}

fn sensitive_assignment(text: &[u8], start: usize) -> Option<(usize, usize)> {
    if start
        .checked_sub(1)
        .is_some_and(|index| is_key(text[index]))
    {
        return None;
    }
    let quote = text
        .get(start)
        .copied()
        .filter(|byte| matches!(byte, b'\'' | b'"'));
    let key_start = start + usize::from(quote.is_some());
    if !text.get(key_start).is_some_and(|byte| is_key(*byte)) {
        return None;
    }
    let mut delimiter = key_start;
    while text.get(delimiter).is_some_and(|byte| is_key(*byte)) {
        delimiter += 1;
    }
    let key_end = delimiter;
    if let Some(quote) = quote {
        if text.get(delimiter) != Some(&quote) {
            return None;
        }
        delimiter += 1;
    }
    while text
        .get(delimiter)
        .is_some_and(|byte| byte.is_ascii_whitespace() && !matches!(byte, b'\n' | b'\r'))
    {
        delimiter += 1;
    }
    if !matches!(text.get(delimiter), Some(b'=' | b':'))
        || !sensitive_key(&text[key_start..key_end])
    {
        return None;
    }
    delimiter += 1;
    while text
        .get(delimiter)
        .is_some_and(|byte| byte.is_ascii_whitespace() && !matches!(byte, b'\n' | b'\r'))
    {
        delimiter += 1;
    }
    assignment_value(text, delimiter)
}

fn assignment_value(text: &[u8], start: usize) -> Option<(usize, usize)> {
    let first = *text.get(start)?;
    if matches!(first, b'\'' | b'"') {
        let content_start = start + 1;
        let mut end = content_start;
        let mut escaped = false;
        while let Some(byte) = text.get(end).copied() {
            if matches!(byte, b'\n' | b'\r') || (byte == first && !escaped) {
                break;
            }
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            }
            end += 1;
        }
        return (end > content_start).then_some((content_start, end));
    }
    let mut end = start;
    while text
        .get(end)
        .is_some_and(|byte| !byte.is_ascii_whitespace() && !matches!(*byte, b'\'' | b'"'))
    {
        end += 1;
    }
    (end > start).then_some((start, end))
}

fn inline_token(text: &[u8], start: usize) -> Option<(usize, usize)> {
    if start
        .checked_sub(1)
        .is_some_and(|index| is_token(text[index]))
    {
        return None;
    }
    let tail = text.get(start..)?;
    for prefix in [
        b"sk-".as_slice(),
        b"sk_live_",
        b"pk_live_",
        b"github_pat_",
        b"xoxb-",
        b"xoxp-",
        b"Bearer ",
    ] {
        if !tail.starts_with(prefix) {
            continue;
        }
        let mut end = start + prefix.len();
        while text.get(end).is_some_and(|byte| is_token(*byte)) {
            end += 1;
        }
        if end - start >= 16 {
            return Some((start, end));
        }
    }
    for prefix in [b"ghp_", b"gho_", b"ghu_", b"ghs_", b"ghr_"] {
        if !tail.starts_with(prefix) {
            continue;
        }
        let mut end = start + prefix.len();
        while text.get(end).is_some_and(|byte| is_token(*byte)) {
            end += 1;
        }
        if end - start >= 40 {
            return Some((start, end));
        }
    }
    None
}

fn sensitive_key(key: &[u8]) -> bool {
    let key = String::from_utf8_lossy(key).to_ascii_lowercase();
    [
        "password",
        "passwd",
        "api_key",
        "apikey",
        "secret",
        "token",
        "private_key",
        "access_key",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}

fn is_key(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_assignments_inline_tokens_aws_and_url_credentials() {
        let input = concat!(
            "API_KEY=plain-secret\n",
            "PASSWORD='quoted secret'\n",
            "OPENAI_API_KEY = \"spaced secret\"\n",
            "{\"access_token\": \"json secret\"}\n",
            "token sk-abcdefghijklmnop now\n",
            "aws=AKIA0123456789ABCDEF\n",
            "url=https://user:password@example.com/path"
        );
        let masked = redact_secrets(input);
        assert_eq!(masked.matches("[redacted]").count(), 7);
        assert!(!masked.contains("plain-secret"));
        assert!(!masked.contains("quoted secret"));
        assert!(!masked.contains("spaced secret"));
        assert!(!masked.contains("json secret"));
        assert!(!masked.contains("abcdefghijklmnop"));
        assert!(!masked.contains("AKIA0123456789ABCDEF"));
        assert!(masked.contains("https://[redacted]@example.com/path"));
    }

    #[test]
    fn preserves_non_secret_unicode_without_allocating() {
        let input = "PROJECT_NAME=secret-service 尾部";
        assert!(matches!(redact_secrets(input), Cow::Borrowed(_)));
        assert_eq!(redact_secrets(input), input);
    }
}
