/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The websocket handler has three main responsibilities:
//! 1) initiate the initial HTTP connection and process the response
//! 2) ensure any DOM requests for sending/closing are propagated to the network
//! 3) transmit any incoming messages/closing to the DOM
//!
//! In order to accomplish this, the handler uses a long-running loop that selects
//! over events from the network and events from the DOM, using async/await to avoid
//! the need for a dedicated thread per websocket.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_tungstenite::WebSocketStream;
use async_tungstenite::tokio::{ClientStream, client_async_tls_with_connector_and_config};
use futures::stream::StreamExt;
use headers::{
    Authorization, Connection, HeaderMapExt, SecWebsocketKey, SecWebsocketVersion, Upgrade,
};
use http::HeaderMap;
use http::header::{self, HeaderName, HeaderValue};
use log::{debug, trace, warn};
use net_traits::request::{RequestBuilder, RequestMode};
use net_traits::{CookieSource, MessageData, WebSocketDomAction, WebSocketNetworkEvent};
use profile_traits::generic_callback::GenericCallback as ProfileGenericCallback;
use servo_base::generic_channel::CallbackSetter;
use servo_url::ServoUrl;
use tokio::net::TcpStream;
use tokio::select;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio_rustls::TlsConnector;
use tungstenite::error::{Error, ProtocolError, UrlError};
use tungstenite::handshake::client::Response;
use tungstenite::protocol::CloseFrame;
use tungstenite::{ClientRequestBuilder, Message};

use crate::async_runtime::spawn_task;
use crate::connector::{NetworkStream, TlsConfig, network_connector};
use crate::cookie::ServoCookie;
use crate::hosts::replace_host;
use crate::http_loader::HttpState;

/// Create a Request object for the initial HTTP request.
/// This request contains `Origin`, `Sec-WebSocket-Protocol`, `Authorization`,
/// and `Cookie` headers as appropriate.
/// Returns an error if any header values are invalid or tungstenite cannot create
/// the desired request.
pub fn create_handshake_request(
    request: RequestBuilder,
    http_state: Arc<HttpState>,
) -> Result<net_traits::request::Request, Error> {
    let origin = request.url.origin();

    let mut headers = HeaderMap::new();
    headers.insert(
        "Origin",
        HeaderValue::from_str(request.url.origin().ascii_serialization().as_ref())?,
    );

    let host = format!(
        "{}",
        origin
            .host()
            .ok_or_else(|| Error::Url(UrlError::NoHostName))?
    );
    headers.insert("Host", HeaderValue::from_str(&host)?);

    // https://websockets.spec.whatwg.org/#concept-websocket-establish
    // 3. Append (`Upgrade`, `websocket`) to request’s header list.
    headers.typed_insert(Upgrade::websocket());

    // 4. Append (`Connection`, `Upgrade`) to request’s header list.
    headers.typed_insert(Connection::upgrade());

    // 5. Let keyValue be a nonce consisting of a randomly selected 16-byte value that has been
    // forgiving-base64-encoded and isomorphic encoded.
    let mut nonce: [u8; 16] = [0; 16];
    rand::fill(&mut nonce);
    let sec_websocket_key_header: SecWebsocketKey = nonce.into();

    // 6. Append (`Sec-WebSocket-Key`, keyValue) to request’s header list.
    headers.typed_insert(sec_websocket_key_header);

    // 7. Append (`Sec-WebSocket-Version`, `13`) to request’s header list.
    headers.typed_insert(SecWebsocketVersion::V13);

    // 8. For each protocol in protocols, combine (`Sec-WebSocket-Protocol`, protocol) in request’s
    // header list.
    let protocols = match request.mode {
        RequestMode::WebSocket {
            ref protocols,
            original_url: _,
        } => protocols,
        _ => unreachable!("How did we get here?"),
    };
    if !protocols.is_empty() {
        let protocols = protocols.join(",");
        headers.insert("Sec-WebSocket-Protocol", HeaderValue::from_str(&protocols)?);
    }

    let mut cookie_jar = http_state.cookie_jar.write();
    cookie_jar.remove_expired_cookies_for_url(&request.url);
    if let Some(cookie_list) = cookie_jar.cookies_for_url(&request.url, CookieSource::HTTP) {
        headers.insert("Cookie", HeaderValue::from_str(&cookie_list)?);
    }

    if request.url.password().is_some() || request.url.username() != "" {
        headers.typed_insert(Authorization::basic(
            request.url.username(),
            request.url.password().unwrap_or(""),
        ));
    }
    Ok(request.headers(headers).build())
}

/// Process an HTTP response resulting from a WS handshake.
/// This ensures that any `Cookie` or HSTS headers are recognized.
/// Returns an error if the protocol selected by the handshake doesn't
/// match the list of provided protocols in the original request.
fn process_ws_response(
    http_state: &HttpState,
    response: &Response,
    resource_url: &ServoUrl,
    protocols: &[String],
) -> Result<Option<String>, Error> {
    trace!("processing websocket http response for {}", resource_url);
    let mut protocol_in_use = None;
    if let Some(protocol_name) = response.headers().get("Sec-WebSocket-Protocol") {
        let protocol_name = protocol_name.to_str().unwrap_or("");
        if !protocols.is_empty() && !protocols.iter().any(|p| protocol_name == (*p)) {
            return Err(Error::Protocol(ProtocolError::InvalidHeader(Box::new(
                HeaderName::from_static("sec-websocket-protocol"),
            ))));
        }
        protocol_in_use = Some(protocol_name.to_string());
    }

    let mut jar = http_state.cookie_jar.write();
    // TODO(eijebong): Replace thise once typed headers settled on a cookie impl
    for cookie in response.headers().get_all(header::SET_COOKIE) {
        let cookie_bytes = cookie.as_bytes();
        if !ServoCookie::is_valid_name_or_value(cookie_bytes) {
            continue;
        }
        if let Ok(s) = std::str::from_utf8(cookie_bytes)
            && let Some(cookie) =
                ServoCookie::from_cookie_string(s, resource_url, CookieSource::HTTP)
        {
            jar.push(cookie, resource_url, CookieSource::HTTP);
        }
    }

    http_state
        .hsts_list
        .write()
        .update_hsts_list_from_response(resource_url, response.headers());

    Ok(protocol_in_use)
}

#[derive(Debug)]
enum DomMsg {
    Send(Message),
    Close(Option<(u16, String)>),
}

/// Initialize a listener for DOM actions. These are routed from the IPC channel
/// to a tokio channel that the main WS client task uses to receive them.
fn setup_dom_listener(
    dom_action_receiver: CallbackSetter<WebSocketDomAction>,
    initiated_close: Arc<AtomicBool>,
) -> UnboundedReceiver<DomMsg> {
    let (sender, receiver) = unbounded_channel();

    dom_action_receiver.set_callback(move |message| {
        let dom_action = message.expect("Ws dom_action message to deserialize");
        trace!("handling WS DOM action: {:?}", dom_action);
        match dom_action {
            WebSocketDomAction::SendMessage(MessageData::Text(data)) => {
                if let Err(e) = sender.send(DomMsg::Send(Message::Text(data.into()))) {
                    warn!("Error sending websocket message: {:?}", e);
                }
            },
            WebSocketDomAction::SendMessage(MessageData::Binary(data)) => {
                if let Err(e) = sender.send(DomMsg::Send(Message::Binary(data.into()))) {
                    warn!("Error sending websocket message: {:?}", e);
                }
            },
            WebSocketDomAction::Close(code, reason) => {
                if initiated_close.fetch_or(true, Ordering::SeqCst) {
                    return;
                }
                let frame = code.map(move |c| (c, reason.unwrap_or_default()));
                if let Err(e) = sender.send(DomMsg::Close(frame)) {
                    warn!("Error closing websocket: {:?}", e);
                }
            },
        }
    });

    receiver
}

/// Listen for WS events from the DOM and the network until one side
/// closes the connection or an error occurs. Since this is an async
/// function that uses the select operation, it will run as a task
/// on the WS tokio runtime.
async fn run_ws_loop(
    mut dom_receiver: UnboundedReceiver<DomMsg>,
    resource_event_sender: ProfileGenericCallback<WebSocketNetworkEvent>,
    mut stream: WebSocketStream<ClientStream<Box<dyn NetworkStream>>>,
) {
    loop {
        select! {
            dom_msg = dom_receiver.recv() => {
                trace!("processing dom msg: {:?}", dom_msg);
                let dom_msg = match dom_msg {
                    Some(msg) => msg,
                    None => break,
                };
                match dom_msg {
                    DomMsg::Send(m) => {
                        if let Err(e) = stream.send(m).await {
                            warn!("error sending websocket message: {:?}", e);
                        }
                    },
                    DomMsg::Close(frame) => {
                        if let Err(e) = stream.close(frame.map(|(code, reason)| {
                            CloseFrame {
                                code: code.into(),
                                reason: reason.into(),
                            }
                        })).await {
                            warn!("error closing websocket: {:?}", e);
                        }
                    },
                }
            }
            ws_msg = stream.next() => {
                trace!("processing WS stream: {:?}", ws_msg);
                let msg = match ws_msg {
                    Some(Ok(msg)) => msg,
                    Some(Err(e)) => {
                        warn!("Error in WebSocket communication: {:?}", e);
                        let _ = resource_event_sender.send(WebSocketNetworkEvent::Fail);
                        break;
                    },
                    None => {
                        warn!("Error in WebSocket communication");
                        let _ = resource_event_sender.send(WebSocketNetworkEvent::Fail);
                        break;
                    }
                };
                match msg {
                    Message::Text(s) => {
                        let message = MessageData::Text(s.as_str().to_owned());
                        if let Err(e) = resource_event_sender
                            .send(WebSocketNetworkEvent::MessageReceived(message))
                        {
                            warn!("Error sending websocket notification: {:?}", e);
                            break;
                        }
                    }

                    Message::Binary(v) => {
                        let message = MessageData::Binary(v.to_vec());
                        if let Err(e) = resource_event_sender
                            .send(WebSocketNetworkEvent::MessageReceived(message))
                        {
                            warn!("Error sending websocket notification: {:?}", e);
                            break;
                        }
                    }

                    Message::Ping(_) | Message::Pong(_) => {}

                    Message::Close(frame) => {
                        let (reason, code) = match frame {
                            Some(frame) => (frame.reason, Some(frame.code.into())),
                            None => ("".into(), None),
                        };
                        debug!("Websocket connection closing due to ({:?}) {}", code, reason);
                        let _ = resource_event_sender.send(WebSocketNetworkEvent::Close(
                            code,
                            reason.to_string(),
                        ));
                        break;
                    }

                    Message::Frame(_) => {
                        warn!("Unexpected websocket frame message");
                    }
                }
            }
        }
    }
}

/// TCP-соединение до `host:port`: напрямую либо через HTTP-CONNECT прокси из prefs
/// (`network_https_proxy_uri`, иначе `network_http_proxy_uri`). Имя хоста уходит прокси как есть.
async fn connect_tcp(host: &str, port: u16) -> std::io::Result<Box<dyn NetworkStream>> {
    // An embedder-provided connector (see `connector::NetworkConnector`) takes precedence over everything below.
    if let Some(connector) = network_connector() {
        return connector.connect(host, port).await;
    }
    Ok(Box::new(connect_tcp_direct(host, port).await?))
}

async fn connect_tcp_direct(host: &str, port: u16) -> std::io::Result<TcpStream> {
    use std::io::{Error as IoError, ErrorKind};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let proxy_uri = {
        let https = servo_config::pref!(network_https_proxy_uri);
        if https.is_empty() {
            servo_config::pref!(network_http_proxy_uri)
        } else {
            https
        }
    };
    if proxy_uri.is_empty() {
        return TcpStream::connect((host, port)).await;
    }

    let proxy = url::Url::parse(&proxy_uri).map_err(|e| {
        IoError::new(
            ErrorKind::InvalidInput,
            format!("некорректный proxy uri: {e}"),
        )
    })?;
    let proxy_host = proxy
        .host_str()
        .ok_or_else(|| IoError::new(ErrorKind::InvalidInput, "в proxy uri нет хоста"))?;
    let proxy_port = proxy.port_or_known_default().unwrap_or(80);
    let mut stream = TcpStream::connect((proxy_host, proxy_port)).await?;

    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if !proxy.username().is_empty() {
        use base64::Engine as _;
        let credentials = format!(
            "{}:{}",
            percent_decode(proxy.username()),
            percent_decode(proxy.password().unwrap_or(""))
        );
        let encoded = base64::engine::general_purpose::STANDARD.encode(credentials);
        request.push_str(&format!("Proxy-Authorization: Basic {encoded}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await?;

    // Читаем ответ побайтно до пустой строки, чтобы не съесть данные туннеля после заголовков.
    let mut head = Vec::with_capacity(128);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 16 * 1024 {
            return Err(IoError::new(
                ErrorKind::InvalidData,
                "слишком длинный ответ прокси",
            ));
        }
        if stream.read(&mut byte).await? == 0 {
            return Err(IoError::new(
                ErrorKind::UnexpectedEof,
                "прокси закрыл соединение",
            ));
        }
        head.push(byte[0]);
    }
    let status_line = String::from_utf8_lossy(&head);
    let status_line = status_line.lines().next().unwrap_or("");
    if status_line.split_whitespace().nth(1) != Some("200") {
        return Err(IoError::new(
            ErrorKind::ConnectionRefused,
            format!("прокси отказал в CONNECT: {status_line}"),
        ));
    }
    Ok(stream)
}

fn percent_decode(input: &str) -> String {
    url::form_urlencoded::parse(format!("k={input}").as_bytes())
        .next()
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default()
}

/// Initiate a new async WS connection. Returns an error if the connection fails
/// for any reason, or if the response isn't valid. Otherwise, the endless WS
/// listening loop will be started.
pub(crate) async fn start_websocket(
    http_state: Arc<HttpState>,
    resource_event_sender: ProfileGenericCallback<WebSocketNetworkEvent>,
    protocols: &[String],
    client: &net_traits::request::Request,
    tls_config: TlsConfig,
    dom_action_receiver: CallbackSetter<WebSocketDomAction>,
) -> Result<Response, Error> {
    trace!("starting WS connection to {}", client.url());

    let initiated_close = Arc::new(AtomicBool::new(false));
    let dom_receiver = setup_dom_listener(dom_action_receiver, initiated_close.clone());

    let url = client.url();
    let host = replace_host(url.host_str().expect("URL has no host"));
    let mut net_url = client.url().into_url();
    net_url
        .set_host(Some(&host))
        .map_err(|e| Error::Url(UrlError::UnableToConnect(e.to_string())))?;

    let domain = net_url
        .host()
        .ok_or_else(|| Error::Url(UrlError::NoHostName))?;
    let port = net_url
        .port_or_known_default()
        .ok_or_else(|| Error::Url(UrlError::UnableToConnect("Unknown port".into())))?;

    // nrb patch: WebSocket ходит через настроенный HTTP-CONNECT прокси (как остальной сетевой стек Servo), а не напрямую.
    // Без этого WebSocket обходил прокси и локально резолвил DNS — утечка реального IP при работе через туннель.
    let socket = connect_tcp(&domain.to_string(), port)
        .await
        .map_err(Error::Io)?;
    let connector = TlsConnector::from(Arc::new(tls_config));

    // TODO(pylbrecht): move request conversion to a separate function
    let mut original_url = client.original_url();
    if original_url.scheme() == "ws" && url.scheme() == "https" {
        original_url.as_mut_url().set_scheme("wss").unwrap();
    }
    let mut builder =
        ClientRequestBuilder::new(original_url.as_str().parse().expect("unable to parse URI"));
    for (key, value) in client.headers.iter() {
        builder = builder.with_header(
            key.as_str(),
            value
                .to_str()
                .expect("unable to convert header value to string"),
        );
    }

    let (stream, response) =
        client_async_tls_with_connector_and_config(builder, socket, Some(connector), None).await?;

    let protocol_in_use = process_ws_response(&http_state, &response, &url, protocols)?;

    if !initiated_close.load(Ordering::SeqCst) {
        if resource_event_sender
            .send(WebSocketNetworkEvent::ConnectionEstablished { protocol_in_use })
            .is_err()
        {
            return Ok(response);
        }

        trace!("about to start ws loop for {}", url);
        spawn_task(run_ws_loop(dom_receiver, resource_event_sender, stream));
    } else {
        trace!("client closed connection for {}, not running loop", url);
    }
    Ok(response)
}
