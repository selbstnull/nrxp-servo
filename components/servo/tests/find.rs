/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Tests for `WebView::find` — find in page without touching the DOM (nrxp-servo N5b).

mod common;

use std::cell::RefCell;
use std::rc::Rc;

use http_body_util::combinators::BoxBody;
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::{Request as HyperRequest, Response as HyperResponse};
use net::test_util::{make_body, make_server};
use servo::{FindAction, FindRequest, JSValue, WebView, WebViewBuilder};

use crate::common::{
    ServoTest, WebViewDelegateImpl, evaluate_javascript,
    show_webview_and_wait_for_rendering_to_be_ready, wait_for_webview_scene_to_be_up_to_date,
};

const PAGE: &str = r#"<!DOCTYPE html><title>find</title>
<body style="margin:0;font:20px sans-serif;background:#fff;color:#000">
<p>foo bar Foo baz</p>
<p>hel<b>lo</b> world, hello again</p>
<p>The   quick
   foo</p>
<div style="height:3000px"></div>
<p id="z">foo at the very bottom</p>
</body>"#;

fn handler(
    _request: HyperRequest<Incoming>,
    response: &mut HyperResponse<BoxBody<Bytes, hyper::Error>>,
) {
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html"));
    *response.body_mut() = make_body(PAGE.as_bytes().to_vec());
}

struct Fixture {
    servo_test: ServoTest,
    webview: WebView,
    delegate: Rc<WebViewDelegateImpl>,
    _server: net::test_util::Server,
}

impl Fixture {
    fn new() -> Self {
        let servo_test = ServoTest::new();
        let (server, url) = make_server(handler);
        let delegate = Rc::new(WebViewDelegateImpl::default());
        let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
            .delegate(delegate.clone())
            .url(url.as_str().parse().unwrap())
            .build();
        show_webview_and_wait_for_rendering_to_be_ready(&servo_test, &webview, &delegate);
        Self {
            servo_test,
            webview,
            delegate,
            _server: server,
        }
    }

    /// `(count, active)` of one find request.
    fn find(
        &self,
        action: FindAction,
        query: &str,
        case_sensitive: bool,
    ) -> (usize, Option<usize>) {
        let answer = Rc::new(RefCell::new(None));
        let sink = answer.clone();
        self.webview.find(
            FindRequest {
                action,
                query: query.to_owned(),
                case_sensitive,
            },
            move |result| *sink.borrow_mut() = Some(result),
        );
        self.servo_test.spin(|| answer.borrow().is_none());
        let Some(Ok(JSValue::String(json))) = answer.borrow_mut().take() else {
            panic!("find did not return a string");
        };
        // {"count":N,"active":I|null}
        let number = |key: &str| -> Option<usize> {
            let rest = json.split(&format!("\"{key}\":")).nth(1)?;
            rest.trim_end_matches('}').split(',').next()?.parse().ok()
        };
        (number("count").expect("count"), number("active"))
    }

    fn js(&self, script: &str) -> JSValue {
        evaluate_javascript(&self.servo_test, self.webview.clone(), script).expect("script")
    }

    /// Number of orange/yellow highlight pixels in a fresh screenshot.
    fn highlight_pixels(&self) -> (usize, usize) {
        wait_for_webview_scene_to_be_up_to_date(&self.servo_test, &self.webview);
        let shot = Rc::new(RefCell::new(None));
        let sink = shot.clone();
        self.webview
            .take_screenshot(None, move |result| *sink.borrow_mut() = Some(result));
        self.servo_test.spin(|| shot.borrow().is_none());
        let image = shot.borrow_mut().take().unwrap().expect("screenshot");
        let (mut yellow, mut orange) = (0, 0);
        for pixel in image.pixels() {
            let [r, g, b, _] = pixel.0;
            // The readiness helper leaves the body background green (0,128,0); the translucent highlights over it:
            // yellow (1,.92,.23 @ .6) → (153,192,35), orange (1,.6,0 @ .85) → (217,149,0).
            if (140..=170).contains(&r) && (180..=205).contains(&g) && (20..=50).contains(&b) {
                yellow += 1;
            } else if (205..=230).contains(&r) && (135..=165).contains(&g) && b < 20 {
                orange += 1;
            }
        }
        (yellow, orange)
    }
}

#[test]
fn test_counts_and_stepping() {
    let f = Fixture::new();
    // Case-insensitive: three "foo" up top (one is "Foo"), one at the bottom.
    assert_eq!(f.find(FindAction::Search, "foo", false), (4, Some(0)));
    assert_eq!(f.find(FindAction::Next, "foo", false), (4, Some(1)));
    assert_eq!(f.find(FindAction::Next, "foo", false), (4, Some(2)));
    assert_eq!(f.find(FindAction::Previous, "foo", false), (4, Some(1)));
    assert_eq!(f.find(FindAction::Previous, "foo", false), (4, Some(0)));
    // Wraps around.
    assert_eq!(f.find(FindAction::Previous, "foo", false), (4, Some(3)));
    assert_eq!(f.find(FindAction::Next, "foo", false), (4, Some(0)));
    // Case-sensitive.
    assert_eq!(f.find(FindAction::Search, "Foo", true), (1, Some(0)));
    // Across inline elements and collapsed whitespace.
    assert_eq!(f.find(FindAction::Search, "hello", false), (2, Some(0)));
    assert_eq!(f.find(FindAction::Search, "quick foo", false), (1, Some(0)));
    assert_eq!(f.find(FindAction::Search, "foo  bar", false), (1, Some(0)));
    // No match / blank query / not across blocks.
    assert_eq!(f.find(FindAction::Search, "zzz", false), (0, None));
    assert_eq!(f.find(FindAction::Search, "   ", false), (0, None));
    assert_eq!(f.find(FindAction::Search, "baz hel", false), (0, None));
}

#[test]
fn test_scrolls_to_the_active_match() {
    let f = Fixture::new();
    assert_eq!(f.js("window.scrollY"), JSValue::Number(0.0));
    f.find(FindAction::Search, "bottom", false);
    let JSValue::Number(y) = f.js("window.scrollY") else {
        panic!()
    };
    assert!(y > 500.0, "did not scroll to the match: {y}");
    // Refresh keeps the active match and does not scroll.
    f.js("window.scrollTo(0, 0)");
    assert_eq!(f.find(FindAction::Refresh, "bottom", false), (1, Some(0)));
    assert_eq!(f.js("window.scrollY"), JSValue::Number(0.0));
}

#[test]
fn test_dom_and_selection_are_left_alone() {
    let f = Fixture::new();
    f.js("window.__records = 0; new MutationObserver(r => { window.__records += r.length; }) \
          .observe(document, {subtree: true, childList: true, attributes: true, characterData: true}); \
          window.__html = document.documentElement.outerHTML; 0");
    f.find(FindAction::Search, "foo", false);
    f.find(FindAction::Next, "foo", false);
    f.find(FindAction::Clear, "foo", false);
    // Let queued mutation records (if any) be delivered.
    f.js("new Promise(r => setTimeout(r, 100)).then(() => 0); 0");
    assert_eq!(
        f.js("window.__records"),
        JSValue::Number(0.0),
        "find mutated the DOM"
    );
    assert_eq!(
        f.js("document.documentElement.outerHTML === window.__html"),
        JSValue::Boolean(true)
    );
    assert_eq!(
        f.js("getSelection().rangeCount"),
        JSValue::Number(0.0),
        "find touched the selection"
    );
}

#[test]
fn test_highlights_are_painted_and_cleared() {
    let f = Fixture::new();
    let _ = &f.delegate;
    assert_eq!(
        f.highlight_pixels(),
        (0, 0),
        "page is not plain white/black"
    );
    f.find(FindAction::Search, "foo", false);
    let (yellow, orange) = f.highlight_pixels();
    assert!(yellow > 300, "other matches not painted: {yellow}");
    assert!(orange > 200, "active match not painted: {orange}");
    f.find(FindAction::Clear, "", false);
    assert_eq!(
        f.highlight_pixels(),
        (0, 0),
        "highlights stayed after Clear"
    );
}
