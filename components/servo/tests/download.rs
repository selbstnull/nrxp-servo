/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Tests for the download hook (`WebViewDelegate::request_download`, nrxp-servo N3b).

mod common;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use http_body_util::combinators::BoxBody;
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::{Request as HyperRequest, Response as HyperResponse};
use net::test_util::{make_body, make_server};
use servo::{
    DownloadEvent, DownloadRequest, DownloadStream, WebView, WebViewBuilder, WebViewDelegate,
};

use crate::common::ServoTest;

#[derive(Clone, Copy, PartialEq)]
enum Policy {
    Accept,
    Decline,
}

struct Offer {
    filename: Option<String>,
    mime: Option<String>,
    length: Option<u64>,
    post: bool,
}

struct DownloadDelegate {
    policy: Cell<Policy>,
    offers: RefCell<Vec<Offer>>,
    stream: RefCell<Option<DownloadStream>>,
    url: RefCell<String>,
}

impl DownloadDelegate {
    fn new(policy: Policy) -> Rc<Self> {
        Rc::new(Self {
            policy: Cell::new(policy),
            offers: RefCell::default(),
            stream: RefCell::default(),
            url: RefCell::default(),
        })
    }
}

impl WebViewDelegate for DownloadDelegate {
    fn notify_url_changed(&self, _webview: WebView, url: url::Url) {
        *self.url.borrow_mut() = url.to_string();
    }

    fn request_download(&self, _webview: WebView, request: DownloadRequest) {
        self.offers.borrow_mut().push(Offer {
            filename: request.suggested_filename().map(str::to_owned),
            mime: request.mime().map(str::to_owned),
            length: request.content_length(),
            post: request.is_post(),
        });
        match self.policy.get() {
            Policy::Accept => *self.stream.borrow_mut() = Some(request.accept(4)),
            Policy::Decline => request.decline(),
        }
    }
}

const BIG: usize = 3 * 1024 * 1024;

fn big_body() -> Vec<u8> {
    (0..BIG).map(|i| (i % 251) as u8).collect()
}

fn handler(
    request: HyperRequest<Incoming>,
    response: &mut HyperResponse<BoxBody<Bytes, hyper::Error>>,
) {
    let set =
        |response: &mut HyperResponse<BoxBody<Bytes, hyper::Error>>, name, value: &'static str| {
            response
                .headers_mut()
                .insert(name, HeaderValue::from_static(value));
        };
    match request.uri().path() {
        "/big" => {
            set(response, header::CONTENT_TYPE, "application/zip");
            set(
                response,
                header::CONTENT_DISPOSITION,
                "attachment; filename*=UTF-8''%D0%BE%D1%82%D1%87%D1%91%D1%82.zip",
            );
            *response.body_mut() = make_body(big_body());
        },
        "/text" => {
            set(response, header::CONTENT_TYPE, "text/plain");
            set(
                response,
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"a.txt\"",
            );
            *response.body_mut() = make_body(b"hello".to_vec());
        },
        "/bin" => {
            set(response, header::CONTENT_TYPE, "application/octet-stream");
            *response.body_mut() = make_body(b"\x00\x01\x02".to_vec());
        },
        "/inline" => {
            set(response, header::CONTENT_TYPE, "text/plain");
            *response.body_mut() = make_body(b"inline".to_vec());
        },
        _ => {
            set(response, header::CONTENT_TYPE, "text/html");
            *response.body_mut() =
                make_body(b"<!DOCTYPE html><title>start</title><p>start</p>".to_vec());
        },
    }
}

struct Fixture {
    servo_test: ServoTest,
    delegate: Rc<DownloadDelegate>,
    webview: WebView,
    base: String,
    server: net::test_util::Server,
}

impl Fixture {
    /// Servo with a webview already on the start page of a local server (`Servo` first: both init the runtime).
    fn new(policy: Policy) -> Self {
        let servo_test = ServoTest::new();
        let (server, url) = make_server(handler);
        let base = format!("http://{}:{}", url.host_str().unwrap(), url.port().unwrap());
        let delegate = DownloadDelegate::new(policy);
        let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
            .delegate(delegate.clone())
            .url(format!("{base}/").parse().unwrap())
            .build();
        let started = Instant::now();
        servo_test.spin(|| {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "start page did not load"
            );
            webview.page_title().as_deref() != Some("start")
        });
        Self {
            servo_test,
            delegate,
            webview,
            base,
            server,
        }
    }

    fn load(&self, path: &str) {
        self.webview
            .load(format!("{}{path}", self.base).parse().unwrap());
    }

    /// Spins until `done` holds (bounded).
    fn wait(&self, what: &str, done: impl Fn() -> bool) {
        let started = Instant::now();
        self.servo_test.spin(|| {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "timed out: {what}"
            );
            !done()
        });
    }

    /// Spins for a fixed time, to let a navigation that should NOT happen show up.
    fn settle(&self) {
        let until = Instant::now() + Duration::from_millis(1500);
        self.servo_test.spin(|| Instant::now() < until);
    }
}

#[test]
fn test_attachment_is_delivered_and_the_page_stays() {
    let f = Fixture::new(Policy::Accept);
    f.load("/big");
    f.wait("offer", || f.delegate.stream.borrow().is_some());

    let mut stream = f.delegate.stream.borrow_mut().take().unwrap();
    let mut data = Vec::new();
    loop {
        match stream.blocking_recv().expect("stream ended without Done") {
            DownloadEvent::Chunk(bytes) => data.extend_from_slice(&bytes),
            DownloadEvent::Done => break,
            DownloadEvent::Failed(reason) => panic!("download failed: {reason}"),
        }
    }
    assert_eq!(data.len(), BIG);
    assert_eq!(data, big_body());

    {
        let offers = f.delegate.offers.borrow();
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].filename.as_deref(), Some("отчёт.zip"));
        assert_eq!(offers[0].mime.as_deref(), Some("application/zip"));
        assert_eq!(offers[0].length, Some(BIG as u64));
        assert!(!offers[0].post);
    }
    // The navigation was aborted like a 204: still the start page.
    f.settle();
    assert_eq!(f.webview.page_title().as_deref(), Some("start"));
    assert!(
        !f.delegate.url.borrow().ends_with("/big"),
        "url {}",
        f.delegate.url.borrow()
    );
    let _ = f.server.close();
}

#[test]
fn test_declined_download_is_handled_by_the_engine() {
    let f = Fixture::new(Policy::Decline);
    f.load("/text");
    f.wait("navigation to the declined attachment", || {
        f.delegate.url.borrow().ends_with("/text")
    });
    assert_eq!(f.delegate.offers.borrow().len(), 1);
    assert!(f.delegate.stream.borrow().is_none());
    let _ = f.server.close();
}

#[test]
fn test_undisplayable_type_is_offered_without_content_disposition() {
    let f = Fixture::new(Policy::Accept);
    f.load("/bin");
    f.wait("offer", || f.delegate.stream.borrow().is_some());
    let mut stream = f.delegate.stream.borrow_mut().take().unwrap();
    let mut data = Vec::new();
    while let Some(event) = stream.blocking_recv() {
        match event {
            DownloadEvent::Chunk(bytes) => data.extend_from_slice(&bytes),
            DownloadEvent::Done => break,
            DownloadEvent::Failed(reason) => panic!("download failed: {reason}"),
        }
    }
    assert_eq!(data, [0, 1, 2]);
    let offers = f.delegate.offers.borrow();
    assert_eq!(offers[0].filename, None);
    assert_eq!(offers[0].mime.as_deref(), Some("application/octet-stream"));
    let _ = f.server.close();
}

#[test]
fn test_inline_response_is_not_offered() {
    let f = Fixture::new(Policy::Accept);
    f.load("/inline");
    f.wait("navigation", || {
        f.delegate.url.borrow().ends_with("/inline")
    });
    assert!(f.delegate.offers.borrow().is_empty());
    let _ = f.server.close();
}

#[test]
fn test_dropping_the_stream_cancels_and_the_next_navigation_works() {
    let f = Fixture::new(Policy::Accept);
    f.load("/big");
    f.wait("offer", || f.delegate.stream.borrow().is_some());
    let mut stream = f.delegate.stream.borrow_mut().take().unwrap();
    // Read one chunk, then walk away.
    assert!(matches!(
        stream.blocking_recv(),
        Some(DownloadEvent::Chunk(_))
    ));
    drop(stream);
    f.settle();

    f.delegate.policy.set(Policy::Decline);
    f.load("/inline");
    f.wait("navigation after the cancelled download", || {
        f.delegate.url.borrow().ends_with("/inline")
    });
    let _ = f.server.close();
}
