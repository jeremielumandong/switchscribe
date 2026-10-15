//! The blocking HTTP plumbing shared by the integrations: one agent configuration, status
//! codes handed back instead of turned into errors, and form / query encoding.

use std::time::Duration;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use crate::{Error, Result};

/// Characters left as-is in a query or form value (RFC 3986 unreserved).
const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// Largest body read from an API (a published calendar can be large).
const BODY_LIMIT: u64 = 64 * 1024 * 1024;

/// A finished exchange.
#[derive(Debug)]
pub(crate) struct Reply {
    /// HTTP status.
    pub status: u16,
    /// The body as text.
    pub body: String,
}

impl Reply {
    /// 2xx.
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// An agent that reports HTTP errors as statuses, with a global timeout.
pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .into()
}

/// Read a response (or the transport error) into a [`Reply`].
pub(crate) fn finish(
    service: &'static str,
    sent: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<Reply> {
    let mut resp = sent.map_err(|e| Error::Network {
        service,
        detail: e.to_string(),
    })?;
    let status = resp.status().as_u16();
    let body = resp
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_string()
        .map_err(|e| Error::Network {
            service,
            detail: e.to_string(),
        })?;
    tracing::debug!(service, status, bytes = body.len(), "http reply");
    Ok(Reply { status, body })
}

/// Percent-encode one query or form component.
pub(crate) fn enc(s: &str) -> String {
    utf8_percent_encode(s, UNRESERVED).to_string()
}

/// `a=1&b=2`, each part percent-encoded.
pub(crate) fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The first line of a service's message, trimmed and capped, for error text.
pub(crate) fn first_line(s: &str) -> String {
    let line = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let mut out: String = line.chars().take(300).collect();
    if line.chars().count() > 300 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_forms() {
        assert_eq!(
            form(&[("scope", "offline_access Calendars.Read"), ("a", "x&y=z")]),
            "scope=offline_access%20Calendars.Read&a=x%26y%3Dz"
        );
        assert_eq!(enc("Priya Nair-ü"), "Priya%20Nair-%C3%BC");
    }

    #[test]
    fn first_lines() {
        assert_eq!(first_line("\n  AADSTS1: no\r\nTrace ID: 1"), "AADSTS1: no");
        assert_eq!(first_line(""), "");
    }
}
