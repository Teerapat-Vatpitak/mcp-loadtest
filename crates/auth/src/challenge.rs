//! Typed parsing for OAuth Bearer challenges.

use url::Url;

use crate::{AuthError, AuthResult, ScopeSet};

/// Values carried by an MCP OAuth `WWW-Authenticate: Bearer` challenge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BearerChallenge {
    /// RFC 9728 protected-resource metadata URL, when supplied.
    pub resource_metadata: Option<Url>,
    /// Requested scopes from the challenge.
    pub scopes: ScopeSet,
    /// Sanitized OAuth error code, when supplied.
    pub error: Option<String>,
}

impl BearerChallenge {
    /// Parse one or more `WWW-Authenticate` field values and return the first
    /// Bearer challenge.
    ///
    /// Quoted commas and backslash-escaped quoted-string characters are
    /// handled without naïvely splitting the field at every comma.
    pub fn parse(values: &[&str]) -> AuthResult<Option<Self>> {
        for value in values {
            if let Some(parameters) = bearer_parameters(value)? {
                let mut challenge = Self::default();
                for (name, value) in parameters {
                    match name.as_str() {
                        "resource_metadata" => {
                            challenge.resource_metadata =
                                Some(Url::parse(&value).map_err(|_| AuthError::InvalidChallenge)?);
                        }
                        "scope" => challenge.scopes = ScopeSet::parse(&value),
                        "error" => {
                            challenge.error = Some(sanitize_error_code(&value));
                        }
                        _ => {}
                    }
                }
                return Ok(Some(challenge));
            }
        }
        Ok(None)
    }
}

fn bearer_parameters(value: &str) -> AuthResult<Option<Vec<(String, String)>>> {
    let segments = split_quoted(value)?;
    let mut current_is_bearer = false;
    let mut found = false;
    let mut parameters = Vec::new();

    for segment in segments {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }

        let (scheme, remainder) = possible_scheme(segment);
        if let Some(scheme) = scheme {
            current_is_bearer = scheme.eq_ignore_ascii_case("bearer");
            if current_is_bearer {
                found = true;
                if !remainder.trim().is_empty() {
                    parameters.push(parse_parameter(remainder.trim())?);
                }
            }
            continue;
        }

        if current_is_bearer {
            parameters.push(parse_parameter(segment)?);
        }
    }

    Ok(found.then_some(parameters))
}

fn possible_scheme(segment: &str) -> (Option<&str>, &str) {
    let end = segment.find(char::is_whitespace).unwrap_or(segment.len());
    let first = &segment[..end];
    if first.contains('=') {
        (None, segment)
    } else if first
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        (Some(first), &segment[end..])
    } else {
        (None, segment)
    }
}

fn split_quoted(value: &str) -> AuthResult<Vec<&str>> {
    let bytes = value.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if quoted => escaped = true,
            b'"' => quoted = !quoted,
            b',' if !quoted => {
                parts.push(&value[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if quoted || escaped {
        return Err(AuthError::InvalidChallenge);
    }
    parts.push(&value[start..]);
    Ok(parts)
}

fn parse_parameter(segment: &str) -> AuthResult<(String, String)> {
    let (name, raw_value) = segment.split_once('=').ok_or(AuthError::InvalidChallenge)?;
    let name = name.trim().to_ascii_lowercase();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(AuthError::InvalidChallenge);
    }
    let raw_value = raw_value.trim();
    let value = if raw_value.starts_with('"') {
        if !raw_value.ends_with('"') || raw_value.len() < 2 {
            return Err(AuthError::InvalidChallenge);
        }
        unescape_quoted(&raw_value[1..raw_value.len() - 1])?
    } else {
        if raw_value.is_empty()
            || !raw_value
                .chars()
                .all(|c| c.is_ascii_graphic() && c != ',' && c != '"')
        {
            return Err(AuthError::InvalidChallenge);
        }
        raw_value.to_owned()
    };
    Ok((name, value))
}

fn unescape_quoted(value: &str) -> AuthResult<String> {
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character == '\\' {
            let escaped = chars.next().ok_or(AuthError::InvalidChallenge)?;
            if !escaped.is_ascii() {
                return Err(AuthError::InvalidChallenge);
            }
            result.push(escaped);
        } else if character.is_control() {
            return Err(AuthError::InvalidChallenge);
        } else {
            result.push(character);
        }
    }
    Ok(result)
}

fn sanitize_error_code(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        .take(64)
        .collect();
    if sanitized.is_empty() {
        "unknown_error".to_owned()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bearer_after_another_challenge_and_quoted_comma() {
        let challenge = BearerChallenge::parse(&[
            r#"Basic realm="one,two", Bearer resource_metadata="https://mcp.example/.well-known/oauth-protected-resource", scope="mcp:read mcp:write""#,
        ])
        .expect("challenge parses")
        .expect("Bearer exists");
        assert_eq!(
            challenge.resource_metadata.expect("metadata").as_str(),
            "https://mcp.example/.well-known/oauth-protected-resource"
        );
        assert!(challenge.scopes.contains("mcp:read"));
        assert!(challenge.scopes.contains("mcp:write"));
    }

    #[test]
    fn returns_none_when_no_bearer_exists() {
        assert_eq!(
            BearerChallenge::parse(&[r#"Basic realm="test""#]).expect("valid"),
            None
        );
    }

    #[test]
    fn rejects_unterminated_quotes() {
        assert_eq!(
            BearerChallenge::parse(&[r#"Bearer scope="broken"#]),
            Err(AuthError::InvalidChallenge)
        );
    }
}
