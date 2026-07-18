//! Bounded, cancellable, nonblocking WebSocket transport primitives.

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::io;
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::thread;
use std::time::{Duration, Instant};

use tungstenite::client::ClientRequestBuilder;
use tungstenite::handshake::{HandshakeError, MidHandshake, client::ClientHandshake};
use tungstenite::http::Uri;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket, client_tls_with_config};
use voxtype_core::{ErrorCategory, VoxError};
use voxtype_provider_common::CancellationToken;

use crate::MAX_MESSAGE_BYTES;

const MAX_ENDPOINT_BYTES: usize = 16 * 1024;
const MAX_HEADERS: usize = 32;
const MAX_HEADER_NAME_BYTES: usize = 64;
const MAX_HEADER_VALUE_BYTES: usize = 1024;
const TCP_CONNECT_SLICE: Duration = Duration::from_millis(100);
const MAX_WRITE_BUFFER_BYTES: usize = 512 * 1024;

/// Fully constructed handshake supplied by the isolated client-identity layer.
///
/// The endpoint can contain persistent device query values, so all fields are
/// redacted from `Debug` and transport errors.
#[derive(Clone, Eq, PartialEq)]
pub struct WebSocketSpec {
    pub endpoint: String,
    pub headers: Vec<(String, String)>,
    pub connect_timeout: Duration,
    pub poll_interval: Duration,
}

impl fmt::Debug for WebSocketSpec {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebSocketSpec")
            .field("endpoint", &"[redacted]")
            .field(
                "headers",
                &format_args!("[{} redacted headers]", self.headers.len()),
            )
            .field("connect_timeout", &self.connect_timeout)
            .field("poll_interval", &self.poll_interval)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueueBinaryResult {
    Queued,
    Full(Vec<u8>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlushResult {
    Flushed,
    Pending,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SocketEvent {
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong,
    Close,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PollResult {
    Idle,
    Event(SocketEvent),
    Closed,
}

/// One nonblocking socket owned by a single provider I/O worker.
pub struct BinaryWebSocket {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
}

impl fmt::Debug for BinaryWebSocket {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str("BinaryWebSocket([redacted connection])")
    }
}

impl BinaryWebSocket {
    /// Queues one binary message without cloning its payload.
    ///
    /// # Errors
    ///
    /// Returns a stable transport error. `Full` is returned separately because
    /// tungstenite guarantees that message has not entered its write buffer.
    pub fn queue_binary(
        &mut self,
        payload: Vec<u8>,
    ) -> Result<QueueBinaryResult, WebSocketTransportError> {
        match self.socket.write(Message::Binary(payload.into())) {
            Ok(()) => Ok(QueueBinaryResult::Queued),
            Err(tungstenite::Error::WriteBufferFull(message)) => match *message {
                Message::Binary(bytes) => Ok(QueueBinaryResult::Full(bytes.to_vec())),
                _ => Err(transport_error(
                    ErrorCategory::Internal,
                    false,
                    "doubao.websocket_buffer_state",
                    "WebSocket returned an unexpected buffered message",
                )),
            },
            Err(error) => Err(map_socket_error(&error, "doubao.websocket_write_failed")),
        }
    }

    /// Flushes queued application and automatic control frames.
    ///
    /// `Pending` means the nonblocking socket would block; the caller must poll
    /// again without re-queueing the message.
    ///
    /// # Errors
    ///
    /// Returns a stable connection or protocol error.
    pub fn flush(&mut self) -> Result<FlushResult, WebSocketTransportError> {
        match self.socket.flush() {
            Ok(()) => Ok(FlushResult::Flushed),
            Err(tungstenite::Error::Io(error)) if would_block(&error) => Ok(FlushResult::Pending),
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                Ok(FlushResult::Closed)
            }
            Err(error) => Err(map_socket_error(&error, "doubao.websocket_flush_failed")),
        }
    }

    /// Polls at most one inbound message.
    ///
    /// Ping responses are queued by tungstenite and immediately flushed once;
    /// a would-block flush remains queued for the next worker iteration.
    ///
    /// # Errors
    ///
    /// Text, raw frame, oversized, malformed, TLS, and socket failures become
    /// stable errors without including the URI or provider payload.
    pub fn poll(&mut self) -> Result<PollResult, WebSocketTransportError> {
        match self.socket.read() {
            Ok(Message::Binary(bytes)) => {
                Ok(PollResult::Event(SocketEvent::Binary(bytes.to_vec())))
            }
            Ok(Message::Ping(bytes)) => {
                let _ = self.flush()?;
                Ok(PollResult::Event(SocketEvent::Ping(bytes.to_vec())))
            }
            Ok(Message::Pong(_)) => Ok(PollResult::Event(SocketEvent::Pong)),
            Ok(Message::Close(_)) => {
                let _ = self.flush()?;
                Ok(PollResult::Event(SocketEvent::Close))
            }
            Ok(Message::Text(_) | Message::Frame(_)) => Err(transport_error(
                ErrorCategory::Protocol,
                false,
                "doubao.websocket_nonbinary_message",
                "Doubao WebSocket returned an unsupported non-binary message",
            )),
            Err(tungstenite::Error::Io(error)) if would_block(&error) => Ok(PollResult::Idle),
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                Ok(PollResult::Closed)
            }
            Err(error) => Err(map_socket_error(&error, "doubao.websocket_read_failed")),
        }
    }

    /// Queues a close frame, flushes once, and shuts down the TCP stream.
    /// This is best-effort and is appropriate for cancellation/error cleanup.
    pub fn close_now(&mut self) {
        let _ = self.socket.close(None);
        let _ = self.socket.flush();
        shutdown_stream(self.socket.get_mut());
    }
}

impl Drop for BinaryWebSocket {
    fn drop(&mut self) {
        shutdown_stream(self.socket.get_mut());
    }
}

/// Connects with bounded TCP attempts and a cancellable nonblocking TLS/HTTP
/// handshake. DNS resolution still uses the standard resolver and is the only
/// operation here that cannot be interrupted by `CancellationToken`.
///
/// # Errors
///
/// Returns configuration, cancellation, timeout, connection, TLS, or handshake
/// errors without copying the sensitive URI or headers into diagnostics.
pub fn connect_websocket(
    spec: &WebSocketSpec,
    cancellation: &CancellationToken,
) -> Result<BinaryWebSocket, WebSocketTransportError> {
    let uri = validate_spec(spec)?;
    if cancellation.is_cancelled() {
        return Err(cancelled());
    }
    let deadline = Instant::now() + spec.connect_timeout;
    let host = uri.host().ok_or_else(|| {
        transport_error(
            ErrorCategory::Configuration,
            false,
            "doubao.websocket_missing_host",
            "Doubao WebSocket endpoint has no host",
        )
    })?;
    let port = uri.port_u16().unwrap_or_else(|| {
        if uri.scheme_str() == Some("wss") {
            443
        } else {
            80
        }
    });
    let addresses = (host, port).to_socket_addrs().map_err(|_| {
        transport_error(
            ErrorCategory::Unavailable,
            true,
            "doubao.websocket_dns_failed",
            "Could not resolve the Doubao WebSocket host",
        )
    })?;
    let addresses: Vec<_> = addresses.collect();
    if addresses.is_empty() {
        return Err(transport_error(
            ErrorCategory::Unavailable,
            true,
            "doubao.websocket_dns_empty",
            "Doubao WebSocket host resolved to no addresses",
        ));
    }
    let stream = connect_tcp(&addresses, deadline, cancellation)?;
    stream.set_nodelay(true).map_err(|_| {
        connection_error(
            "doubao.websocket_nodelay_failed",
            "Could not configure the Doubao TCP connection",
        )
    })?;
    stream.set_nonblocking(true).map_err(|_| {
        connection_error(
            "doubao.websocket_nonblocking_failed",
            "Could not configure nonblocking Doubao WebSocket I/O",
        )
    })?;

    let mut request = ClientRequestBuilder::new(uri);
    for (name, value) in &spec.headers {
        request = request.with_header(name, value);
    }
    let websocket_config = WebSocketConfig::default()
        .read_buffer_size(16 * 1024)
        .write_buffer_size(0)
        .max_write_buffer_size(MAX_WRITE_BUFFER_BYTES)
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));
    match client_tls_with_config(request, stream, Some(websocket_config), None) {
        Ok((socket, _response)) => Ok(BinaryWebSocket { socket }),
        Err(HandshakeError::Interrupted(handshake)) => {
            finish_handshake(handshake, deadline, spec.poll_interval, cancellation)
        }
        Err(HandshakeError::Failure(error)) => {
            if cancellation.is_cancelled() {
                Err(cancelled())
            } else {
                Err(map_handshake_error(error))
            }
        }
    }
}

fn finish_handshake(
    mut handshake: MidHandshake<ClientHandshake<MaybeTlsStream<TcpStream>>>,
    deadline: Instant,
    poll_interval: Duration,
    cancellation: &CancellationToken,
) -> Result<BinaryWebSocket, WebSocketTransportError> {
    loop {
        if cancellation.is_cancelled() {
            shutdown_handshake(&mut handshake);
            return Err(cancelled());
        }
        if Instant::now() >= deadline {
            shutdown_handshake(&mut handshake);
            return Err(timeout(
                "doubao.websocket_handshake_timeout",
                "Doubao WebSocket handshake timed out",
            ));
        }
        match handshake.handshake() {
            Ok((socket, _response)) => return Ok(BinaryWebSocket { socket }),
            Err(HandshakeError::Interrupted(next)) => handshake = next,
            Err(HandshakeError::Failure(error)) => {
                return if cancellation.is_cancelled() {
                    Err(cancelled())
                } else {
                    Err(map_handshake_error(error))
                };
            }
        }
        thread::sleep(poll_interval);
    }
}

fn connect_tcp(
    addresses: &[std::net::SocketAddr],
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<TcpStream, WebSocketTransportError> {
    loop {
        let mut saw_timeout = false;
        for address in addresses {
            if cancellation.is_cancelled() {
                return Err(cancelled());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(timeout(
                    "doubao.websocket_connect_timeout",
                    "Doubao WebSocket TCP connection timed out",
                ));
            }
            let remaining = deadline.saturating_duration_since(now);
            let slice = remaining.min(TCP_CONNECT_SLICE);
            match TcpStream::connect_timeout(address, slice) {
                Ok(stream) => return Ok(stream),
                Err(error) if matches!(error.kind(), io::ErrorKind::TimedOut) => {
                    saw_timeout = true;
                }
                Err(_) => {}
            }
        }
        if !saw_timeout {
            return Err(connection_error(
                "doubao.websocket_connect_failed",
                "Could not connect to the Doubao WebSocket service",
            ));
        }
    }
}

fn validate_spec(spec: &WebSocketSpec) -> Result<Uri, WebSocketTransportError> {
    if spec.endpoint.is_empty()
        || spec.endpoint.len() > MAX_ENDPOINT_BYTES
        || spec.endpoint.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(configuration_error(
            "doubao.websocket_invalid_endpoint",
            "Doubao WebSocket endpoint is invalid",
        ));
    }
    if !(Duration::from_millis(100)..=Duration::from_secs(60)).contains(&spec.connect_timeout)
        || !(Duration::from_millis(1)..=Duration::from_millis(100)).contains(&spec.poll_interval)
    {
        return Err(configuration_error(
            "doubao.websocket_invalid_timeout",
            "Doubao WebSocket timeouts are outside the safe range",
        ));
    }
    if !spec.endpoint.starts_with("wss://") && !is_loopback_ws(&spec.endpoint) {
        return Err(configuration_error(
            "doubao.websocket_insecure_endpoint",
            "Doubao WebSocket endpoint must use WSS or loopback WS",
        ));
    }
    if spec.headers.len() > MAX_HEADERS {
        return Err(configuration_error(
            "doubao.websocket_too_many_headers",
            "Doubao WebSocket handshake contains too many headers",
        ));
    }
    for (name, value) in &spec.headers {
        if !valid_header_name(name)
            || value.is_empty()
            || value.len() > MAX_HEADER_VALUE_BYTES
            || value.bytes().any(|byte| byte.is_ascii_control())
            || is_reserved_header(name)
        {
            return Err(configuration_error(
                "doubao.websocket_invalid_header",
                "Doubao WebSocket handshake header is invalid or reserved",
            ));
        }
    }
    spec.endpoint.parse::<Uri>().map_err(|_| {
        configuration_error(
            "doubao.websocket_invalid_uri",
            "Doubao WebSocket endpoint is not a valid URI",
        )
    })
}

fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_HEADER_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn is_reserved_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "host"
            | "upgrade"
            | "sec-websocket-accept"
            | "sec-websocket-extensions"
            | "sec-websocket-key"
            | "sec-websocket-protocol"
            | "sec-websocket-version"
    )
}

fn is_loopback_ws(endpoint: &str) -> bool {
    let Some(rest) = endpoint.strip_prefix("ws://") else {
        return false;
    };
    let authority = rest
        .split(['/', '?'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    authority == "localhost"
        || authority.starts_with("localhost:")
        || authority == "127.0.0.1"
        || authority.starts_with("127.0.0.1:")
        || authority == "[::1]"
        || authority.starts_with("[::1]:")
}

fn shutdown_handshake(handshake: &mut MidHandshake<ClientHandshake<MaybeTlsStream<TcpStream>>>) {
    shutdown_stream(handshake.get_mut().get_mut());
}

fn shutdown_stream(stream: &mut MaybeTlsStream<TcpStream>) {
    match stream {
        MaybeTlsStream::Plain(tcp) => {
            let _ = tcp.shutdown(Shutdown::Both);
        }
        MaybeTlsStream::Rustls(tls) => {
            let _ = tls.sock.shutdown(Shutdown::Both);
        }
        _ => {}
    }
}

fn would_block(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

fn map_handshake_error(error: tungstenite::Error) -> WebSocketTransportError {
    match error {
        tungstenite::Error::Http(response) => {
            let status = response.status().as_u16();
            if status == 401 || status == 403 {
                transport_error(
                    ErrorCategory::Authentication,
                    false,
                    "doubao.websocket_auth_rejected",
                    "Doubao rejected the WebSocket handshake credentials",
                )
            } else if status == 429 {
                transport_error(
                    ErrorCategory::RateLimited,
                    true,
                    "doubao.websocket_rate_limited",
                    "Doubao rate limited the WebSocket handshake",
                )
            } else {
                transport_error(
                    ErrorCategory::Protocol,
                    false,
                    "doubao.websocket_handshake_rejected",
                    "Doubao rejected the WebSocket handshake",
                )
            }
        }
        tungstenite::Error::Tls(_) => transport_error(
            ErrorCategory::Connection,
            false,
            "doubao.websocket_tls_failed",
            "Doubao WebSocket TLS validation failed",
        ),
        tungstenite::Error::Io(_) => connection_error(
            "doubao.websocket_handshake_io_failed",
            "Doubao WebSocket handshake I/O failed",
        ),
        _ => transport_error(
            ErrorCategory::Protocol,
            false,
            "doubao.websocket_handshake_failed",
            "Doubao WebSocket handshake failed",
        ),
    }
}

fn map_socket_error(
    error: &tungstenite::Error,
    default_code: &'static str,
) -> WebSocketTransportError {
    match error {
        tungstenite::Error::Tls(_) => transport_error(
            ErrorCategory::Connection,
            false,
            "doubao.websocket_tls_failed",
            "Doubao WebSocket TLS connection failed",
        ),
        tungstenite::Error::Capacity(_)
        | tungstenite::Error::Protocol(_)
        | tungstenite::Error::Utf8(_)
        | tungstenite::Error::AttackAttempt => transport_error(
            ErrorCategory::Protocol,
            false,
            "doubao.websocket_protocol_failed",
            "Doubao WebSocket protocol validation failed",
        ),
        _ => connection_error(default_code, "Doubao WebSocket connection failed"),
    }
}

fn cancelled() -> WebSocketTransportError {
    transport_error(
        ErrorCategory::Cancelled,
        false,
        "doubao.websocket_cancelled",
        "Doubao WebSocket operation was cancelled",
    )
}

fn timeout(code: &'static str, message: &'static str) -> WebSocketTransportError {
    transport_error(ErrorCategory::Timeout, true, code, message)
}

fn connection_error(code: &'static str, message: &'static str) -> WebSocketTransportError {
    transport_error(ErrorCategory::Connection, true, code, message)
}

fn configuration_error(code: &'static str, message: &'static str) -> WebSocketTransportError {
    transport_error(ErrorCategory::Configuration, false, code, message)
}

fn transport_error(
    category: ErrorCategory,
    retryable: bool,
    code: &'static str,
    message: &'static str,
) -> WebSocketTransportError {
    WebSocketTransportError {
        category,
        retryable,
        code,
        message,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WebSocketTransportError {
    category: ErrorCategory,
    retryable: bool,
    code: &'static str,
    message: &'static str,
}

impl WebSocketTransportError {
    #[must_use]
    pub const fn category(self) -> ErrorCategory {
        self.category
    }

    #[must_use]
    pub const fn is_retryable(self) -> bool {
        self.retryable
    }

    #[must_use]
    pub const fn code(self) -> &'static str {
        self.code
    }

    #[must_use]
    pub fn into_vox_error(self) -> VoxError {
        VoxError::new(self.category, self.code, self.message).with_retryable(self.retryable)
    }
}

impl Display for WebSocketTransportError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl Error for WebSocketTransportError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc::{self, sync_channel};

    fn fixture_spec(endpoint: String) -> WebSocketSpec {
        WebSocketSpec {
            endpoint,
            headers: vec![
                ("User-Agent".to_owned(), "VoxType fixture".to_owned()),
                ("proto-version".to_owned(), "1".to_owned()),
                ("x-custom-keepalive".to_owned(), "1".to_owned()),
            ],
            connect_timeout: Duration::from_secs(2),
            poll_interval: Duration::from_millis(2),
        }
    }

    fn poll_until_event(socket: &mut BinaryWebSocket) -> SocketEvent {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match socket.poll().expect("poll loopback WebSocket") {
                PollResult::Event(event) => return event,
                PollResult::Idle => {
                    assert!(Instant::now() < deadline, "loopback event timed out");
                    thread::yield_now();
                }
                PollResult::Closed => panic!("loopback WebSocket closed early"),
            }
        }
    }

    #[test]
    fn validates_endpoint_and_redacts_handshake() {
        let spec = fixture_spec("ws://example.com/private?device_id=secret".to_owned());
        let error = validate_spec(&spec).expect_err("remote plain WS must fail");
        assert_eq!(error.code(), "doubao.websocket_insecure_endpoint");
        assert!(!format!("{spec:?}").contains("secret"));

        let mut reserved = fixture_spec("ws://127.0.0.1:1/socket".to_owned());
        reserved
            .headers
            .push(("Host".to_owned(), "override".to_owned()));
        assert_eq!(
            validate_spec(&reserved)
                .expect_err("reserved header")
                .code(),
            "doubao.websocket_invalid_header"
        );
    }

    #[test]
    fn loopback_exchanges_binary_and_automatic_pong() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback WebSocket");
        let address = listener.local_addr().expect("loopback address");
        let (observation_sender, observation_receiver) = sync_channel(1);
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept WebSocket client");
            let mut socket = tungstenite::accept_hdr(
                stream,
                |request: &tungstenite::handshake::server::Request,
                 response: tungstenite::handshake::server::Response| {
                    let uri = request.uri().to_string();
                    let user_agent = request
                        .headers()
                        .get("user-agent")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    observation_sender
                        .send((uri, user_agent))
                        .expect("handshake observation");
                    Ok(response)
                },
            )
            .expect("server WebSocket handshake");
            assert_eq!(
                socket.read().expect("client binary"),
                Message::Binary(vec![1, 2, 3].into())
            );
            socket
                .send(Message::Binary(vec![4, 5, 6].into()))
                .expect("server binary");
            socket
                .send(Message::Ping(vec![9, 8].into()))
                .expect("server ping");
            loop {
                match socket.read().expect("client pong") {
                    Message::Pong(bytes) => {
                        assert_eq!(bytes.as_ref(), &[9, 8]);
                        break;
                    }
                    Message::Close(_) => panic!("client closed before pong"),
                    _ => {}
                }
            }
        });

        let spec = fixture_spec(format!("ws://{address}/socket?device_id=fixture"));
        let mut socket = connect_websocket(&spec, &CancellationToken::new())
            .expect("connect loopback WebSocket");
        assert_eq!(
            socket.queue_binary(vec![1, 2, 3]).expect("queue binary"),
            QueueBinaryResult::Queued
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while socket.flush().expect("flush client binary") == FlushResult::Pending {
            assert!(Instant::now() < deadline, "client flush timed out");
            thread::yield_now();
        }
        assert_eq!(
            poll_until_event(&mut socket),
            SocketEvent::Binary(vec![4, 5, 6])
        );
        assert_eq!(poll_until_event(&mut socket), SocketEvent::Ping(vec![9, 8]));
        let (uri, user_agent) = observation_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("handshake observation");
        assert_eq!(uri, "/socket?device_id=fixture");
        assert_eq!(user_agent, "VoxType fixture");
        server.join().expect("loopback server");
    }

    #[test]
    fn cancellation_interrupts_a_stalled_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalled server");
        let address = listener.local_addr().expect("stalled address");
        let (accepted_sender, accepted_receiver) = mpsc::channel();
        let (stop_sender, stop_receiver) = mpsc::channel();
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().expect("accept stalled client");
            accepted_sender.send(()).expect("accepted signal");
            stop_receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("stalled server stop");
        });
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let spec = fixture_spec(format!("ws://{address}/stalled"));
        let worker = thread::spawn(move || connect_websocket(&spec, &worker_cancellation));
        accepted_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("server accepted connection");
        let cancelled_at = Instant::now();
        cancellation.cancel();
        let error = worker
            .join()
            .expect("connect worker")
            .expect_err("cancelled handshake");
        assert_eq!(error.category(), ErrorCategory::Cancelled);
        assert!(cancelled_at.elapsed() < Duration::from_millis(500));
        stop_sender.send(()).expect("stop stalled server");
        server.join().expect("stalled server");
    }
}
