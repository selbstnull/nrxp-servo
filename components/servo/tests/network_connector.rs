/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Tests for the embedder-provided [`NetworkConnector`]: with one installed, Servo must open **no** sockets of its own.

mod common;

use std::rc::Rc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

use http_body_util::combinators::BoxBody;
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::{Request as HyperRequest, Response as HyperResponse};
use net::test_util::{make_body, make_server};
use servo::{ConnectFuture, NetworkConnector, WebViewBuilder};

use crate::common::{ServoTest, WebViewDelegateImpl};

/// Records every requested `host:port` and serves all of them from one local test server.
struct RecordingConnector {
    /// Set once the test server is up (it must be created after `Servo`: both initialise the async runtime).
    target_port: AtomicU16,
    seen: Mutex<Vec<(String, u16)>>,
}

impl NetworkConnector for RecordingConnector {
    fn connect(&self, host: &str, port: u16) -> ConnectFuture {
        self.seen.lock().unwrap().push((host.to_owned(), port));
        let target = self.target_port.load(Ordering::SeqCst);
        Box::pin(async move {
            // Whatever name Servo asked for, the embedder decides where it goes.
            let stream = tokio::net::TcpStream::connect(("127.0.0.1", target)).await?;
            Ok(Box::new(stream) as Box<dyn servo::NetworkStream>)
        })
    }
}

#[test]
fn test_all_connections_go_through_the_connector() {
    let handler =
        move |request: HyperRequest<Incoming>,
              response: &mut HyperResponse<BoxBody<Bytes, hyper::Error>>| {
            let (content_type, body) = match request.uri().path() {
                "/sub.js" => ("text/javascript", "document.title = 'connector-ok';"),
                _ => (
                    "text/html",
                    "<!DOCTYPE html><title>start</title>\
                 <script src=\"http://sub.nrxp-test.invalid:8081/sub.js\"></script>",
                ),
            };
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
            *response.body_mut() = make_body(body.as_bytes().to_vec());
        };
    let connector = Arc::new(RecordingConnector {
        target_port: AtomicU16::new(0),
        seen: Mutex::new(Vec::new()),
    });
    let servo_test = ServoTest::new_with_builder({
        let connector = connector.clone();
        move |builder| builder.network_connector(connector)
    });
    let (server, url) = make_server(handler);
    connector
        .target_port
        .store(url.port().unwrap(), Ordering::SeqCst);
    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .build();

    let delegate_clone = delegate.clone();
    servo_test.spin(move || !delegate_clone.url_changed.get());

    // The main document is requested by a name that does not resolve anywhere: it can only load via the connector.
    delegate.reset();
    webview.load("http://page.nrxp-test.invalid:8080/".parse().unwrap());
    // The subresource script sets the title asynchronously, so wait for the title (bounded, with a useful message).
    let started = std::time::Instant::now();
    servo_test.spin(|| {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "timed out; title {:?}, connections {:?}",
            webview.page_title(),
            connector.seen.lock().unwrap()
        );
        webview.page_title().as_deref() != Some("connector-ok")
    });
    let _ = server.close();

    assert_eq!(webview.page_title().as_deref(), Some("connector-ok"));
    let seen = connector.seen.lock().unwrap().clone();
    // Names are passed unresolved, with the port taken from the URL.
    assert!(
        seen.contains(&("page.nrxp-test.invalid".to_owned(), 8080)),
        "document connection missing: {seen:?}"
    );
    assert!(
        seen.contains(&("sub.nrxp-test.invalid".to_owned(), 8081)),
        "subresource connection missing: {seen:?}"
    );
}
