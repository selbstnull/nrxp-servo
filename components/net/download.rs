/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Downloads taken over by the embedder (nrxp-servo N3b).
//!
//! A navigation whose response is a download (`Content-Disposition: attachment`, or a type the
//! engine cannot display) is offered to the embedder before the document is created. If the
//! embedder accepts, the body is streamed to it and the navigation is aborted like a 204, so the
//! current page stays; if it declines, nothing changes.

use bytes::Bytes;
use http::HeaderMap;
use http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE};
use servo_url::ServoUrl;

/// What the embedder is told about a download offer.
#[derive(Clone, Debug)]
pub struct DownloadInfo {
    /// The URL the response came from, after redirects.
    pub url: ServoUrl,
    /// File name from `Content-Disposition` (`filename*` preferred), as sent: **not sanitized**.
    pub suggested_filename: Option<String>,
    /// `Content-Type` without parameters, lowercase.
    pub mime: Option<String>,
    /// `Content-Length`, if known.
    pub content_length: Option<u64>,
    /// The request was a `POST` (a download cannot be re-fetched by URL).
    pub is_post: bool,
}

/// What the engine sends to an accepting embedder.
#[derive(Debug)]
pub enum DownloadEvent {
    Chunk(Bytes),
    /// The whole body was delivered.
    Done,
    /// The transfer broke off.
    Failed(String),
}

/// Whether a navigation response should be offered as a download.
pub(crate) fn is_download_response(headers: &HeaderMap) -> bool {
    if let Some(disposition) = headers
        .get(CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
    {
        let kind = disposition.split(';').next().unwrap_or("").trim();
        if kind.eq_ignore_ascii_case("attachment") {
            return true;
        }
    }
    mime_of(headers).is_some_and(|m| !is_displayable(&m))
}

pub(crate) fn mime_of(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(CONTENT_TYPE)?.to_str().ok()?;
    let essence = value.split(';').next()?.trim().to_ascii_lowercase();
    (!essence.is_empty()).then_some(essence)
}

/// Only `application/*` types that are neither markup, script nor data the engine shows are
/// treated as downloads; everything else is left to the engine as before.
fn is_displayable(mime: &str) -> bool {
    let Some(sub) = mime.strip_prefix("application/") else {
        return true;
    };
    matches!(
        sub,
        "xhtml+xml"
            | "xml"
            | "json"
            | "javascript"
            | "x-javascript"
            | "ecmascript"
            | "pdf"
            | "rss+xml"
            | "atom+xml"
            | "ld+json"
            | "manifest+json"
            | "x-www-form-urlencoded"
            | "wasm"
            | "x-ndjson"
            | "x-httpd-php"
    ) || sub.ends_with("+xml")
        || sub.ends_with("+json")
}

pub(crate) fn content_length_of(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// `filename*=UTF-8''…` wins over `filename=…`.
pub(crate) fn filename_of(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(CONTENT_DISPOSITION)?.to_str().ok()?;
    let mut plain = None;
    let mut extended = None;
    for param in split_params(value).into_iter().skip(1) {
        let Some((name, val)) = param.split_once('=') else {
            continue;
        };
        let val = val.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "filename*" => extended = decode_ext_value(val),
            "filename" => plain = Some(unquote(val)),
            _ => {},
        }
    }
    extended.or(plain).filter(|n| !n.is_empty())
}

/// Splits on `;` outside quotes.
fn split_params(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut quoted, mut escaped) = (0, false, false);
    for (i, c) in value.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => {
                out.push(&value[start..i]);
                start = i + 1;
            },
            _ => {},
        }
    }
    out.push(&value[start..]);
    out
}

fn unquote(val: &str) -> String {
    let Some(inner) = val.strip_prefix('"') else {
        return val.to_owned();
    };
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            '"' => break,
            c => out.push(c),
        }
    }
    out
}

/// RFC 8187 `charset'language'percent-encoded`; only UTF-8 and ISO-8859-1.
fn decode_ext_value(val: &str) -> Option<String> {
    let mut parts = val.splitn(3, '\'');
    let charset = parts.next()?;
    parts.next()?;
    let encoded = parts.next()?;
    let mut bytes = Vec::with_capacity(encoded.len());
    let raw = encoded.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' {
            let hex = std::str::from_utf8(raw.get(i + 1..i + 3)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    if charset.eq_ignore_ascii_case("utf-8") {
        String::from_utf8(bytes).ok()
    } else if charset.eq_ignore_ascii_case("iso-8859-1") {
        Some(bytes.into_iter().map(char::from).collect())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;

    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn attachment_is_a_download_inline_is_not() {
        let attach = headers(&[("content-disposition", "Attachment; filename=a.txt")]);
        assert!(is_download_response(&attach));
        let inline = headers(&[
            ("content-disposition", "inline; filename=a.txt"),
            ("content-type", "text/plain"),
        ]);
        assert!(!is_download_response(&inline));
    }

    #[test]
    fn undisplayable_application_types_are_downloads() {
        for mime in [
            "application/octet-stream",
            "application/zip",
            "application/x-tar; x=y",
        ] {
            assert!(
                is_download_response(&headers(&[("content-type", mime)])),
                "{mime}"
            );
        }
        for mime in [
            "text/html; charset=utf-8",
            "image/png",
            "video/mp4",
            "application/json",
            "application/pdf",
            "application/xhtml+xml",
            "application/vnd.api+json",
        ] {
            assert!(
                !is_download_response(&headers(&[("content-type", mime)])),
                "{mime}"
            );
        }
        assert!(!is_download_response(&HeaderMap::new()));
    }

    #[test]
    fn filename_forms() {
        let get = |v: &str| filename_of(&headers(&[("content-disposition", v)]));
        assert_eq!(get("attachment; filename=a.zip").as_deref(), Some("a.zip"));
        assert_eq!(
            get(r#"attachment; filename="a b; c.zip""#).as_deref(),
            Some("a b; c.zip")
        );
        assert_eq!(
            get(r#"attachment; filename="q\"x.txt""#).as_deref(),
            Some("q\"x.txt")
        );
        assert_eq!(
            get("attachment; filename=fallback.txt; filename*=UTF-8''%D0%BE%D1%82%D1%87%D1%91%D1%82.pdf")
                .as_deref(),
            Some("отчёт.pdf")
        );
        assert_eq!(
            get("attachment; filename*=iso-8859-1'en'caf%E9.txt").as_deref(),
            Some("café.txt")
        );
        assert_eq!(get("attachment").as_deref(), None);
        assert_eq!(get("attachment; filename=").as_deref(), None);
        // Bad percent-escape: the plain name still wins.
        assert_eq!(
            get("attachment; filename=ok.txt; filename*=UTF-8''%zz").as_deref(),
            Some("ok.txt")
        );
    }

    #[test]
    fn content_length_parses() {
        assert_eq!(
            content_length_of(&headers(&[("content-length", " 42 ")])),
            Some(42)
        );
        assert_eq!(
            content_length_of(&headers(&[("content-length", "x")])),
            None
        );
    }
}
