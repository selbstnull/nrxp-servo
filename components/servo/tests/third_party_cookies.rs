/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Tests for `network_block_third_party_cookies` (nrxp-servo N4b).

mod common;

use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http_body_util::combinators::BoxBody;
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::{Request as HyperRequest, Response as HyperResponse};
use net::test_util::{make_body, make_server};
use servo::{CookieSource, Preferences, WebViewBuilder};

use crate::common::{ServoTest, WebViewDelegateImpl};

/// What the "tracker" origin saw: the `Cookie` header of each `/tp.js` request.
#[derive(Default)]
struct Seen {
    script_cookies: Mutex<Vec<Option<String>>>,
    script_hits: AtomicUsize,
}

fn tracker_handler(
    seen: Arc<Seen>,
) -> impl Fn(HyperRequest<Incoming>, &mut HyperResponse<BoxBody<Bytes, hyper::Error>>)
+ Send
+ Sync
+ 'static {
    move |request, response| {
        let (content_type, body, cookie) = match request.uri().path() {
            "/tp.js" => {
                seen.script_cookies.lock().unwrap().push(
                    request
                        .headers()
                        .get(header::COOKIE)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned),
                );
                seen.script_hits.fetch_add(1, Ordering::SeqCst);
                ("text/javascript", "document.title = 'loaded';", "tp=1")
            },
            _ => ("text/html", "<!DOCTYPE html><title>tracker</title>", "fp=1"),
        };
        let headers = response.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        headers.insert(header::SET_COOKIE, HeaderValue::from_static(cookie));
        // Every load must reach the server (the test counts requests).
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        *response.body_mut() = make_body(body.as_bytes().to_vec());
    }
}

fn page_handler(
    tracker_base: Arc<Mutex<String>>,
) -> impl Fn(HyperRequest<Incoming>, &mut HyperResponse<BoxBody<Bytes, hyper::Error>>)
+ Send
+ Sync
+ 'static {
    move |_request, response| {
        let page = format!(
            "<!DOCTYPE html><title>page</title><script src=\"{}/tp.js\"></script>",
            tracker_base.lock().unwrap()
        );
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html"));
        *response.body_mut() = make_body(page.into_bytes());
    }
}

fn wait(servo_test: &ServoTest, what: &str, done: impl Fn() -> bool) {
    let started = Instant::now();
    servo_test.spin(|| {
        assert!(
            started.elapsed() < Duration::from_secs(14),
            "timed out: {what}"
        );
        !done()
    });
}

fn run(block: bool) -> (Vec<String>, Vec<String>, Vec<Option<String>>) {
    let servo_test = ServoTest::new_with_builder(move |builder| {
        let mut preferences = Preferences::default();
        preferences.network_http_proxy_uri = String::new();
        preferences.network_https_proxy_uri = String::new();
        preferences.network_block_third_party_cookies = block;
        builder.preferences(preferences)
    });
    let seen = Arc::new(Seen::default());
    let (tracker, tracker_url) = make_server(tracker_handler(seen.clone()));
    // The test servers' own URLs use `localhost`; `127.0.0.1` is a different host, hence a different site.
    let tracker_base = format!("http://127.0.0.1:{}", tracker_url.port().unwrap());
    let (page, page_url) = make_server(page_handler(Arc::new(Mutex::new(tracker_base.clone()))));

    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .build();
    // The initial about:blank must be up before the first real load (as in the connector test).
    let started = delegate.clone();
    servo_test.spin(move || !started.url_changed.get());
    let cookie_names = |url: &str| -> Vec<String> {
        let mut names: Vec<String> = servo_test
            .servo()
            .site_data_manager()
            .cookies_for_url(url.parse().unwrap(), CookieSource::HTTP)
            .iter()
            .map(|c| c.name().to_owned())
            .collect();
        names.sort();
        names
    };

    // 1. A page on one site includes a script from another: third-party request.
    webview.load(page_url.as_str().parse().unwrap());
    wait(&servo_test, "third-party script", || {
        webview.page_title().as_deref() == Some("loaded")
    });
    let after_embed = cookie_names(&tracker_base);

    // 2. Visiting the tracker directly is first-party and must always work.
    webview.load(format!("{tracker_base}/fp").parse().unwrap());
    wait(&servo_test, "first-party visit", || {
        webview.page_title().as_deref() == Some("tracker")
    });
    let after_visit = cookie_names(&tracker_base);

    // 3. The page again: does the third-party request carry the first-party cookie?
    webview.load(page_url.as_str().parse().unwrap());
    wait(&servo_test, "second third-party script", || {
        seen.script_hits.load(Ordering::SeqCst) >= 2
    });
    let _ = tracker.close();
    let _ = page.close();
    let cookies_sent = seen.script_cookies.lock().unwrap().clone();
    (after_embed, after_visit, cookies_sent)
}

#[test]
fn test_third_party_cookies_are_blocked_when_enabled() {
    let (after_embed, after_visit, sent) = run(true);
    assert!(
        after_embed.is_empty(),
        "stored from a third party: {after_embed:?}"
    );
    assert!(
        after_visit.contains(&"fp".to_owned()),
        "first party must work: {after_visit:?}"
    );
    assert!(!after_visit.contains(&"tp".to_owned()), "{after_visit:?}");
    assert_eq!(sent[0], None);
    assert_eq!(sent[1], None, "cookie sent to a third party: {sent:?}");
}

#[test]
fn test_third_party_cookies_work_by_default() {
    let (after_embed, _, sent) = run(false);
    assert!(after_embed.contains(&"tp".to_owned()), "{after_embed:?}");
    assert!(
        sent[1].as_deref().is_some_and(|c| c.contains("tp=1")),
        "cookie not sent: {sent:?}"
    );
}
