//! Native monoio-http/1.1 core.
//!
//! Accepts a connected stream, decodes requests with `RequestDecoder`,
//! dispatches to an inner `Service<(Request<HttpBody>, CX)>`, and encodes
//! responses with an SSE-aware HTTP/1 encoder. Keep-alive is controlled by the
//! inner service returning `(Response, continue)`.

use super::super::*;
use bytes::BytesMut;
use monoio::io::{
    AsyncReadRent, AsyncWriteRent, AsyncWriteRentExt, Split, Splitable,
    stream::Stream as MonoioStream,
};
use monoio_http::h1::codec::decoder::{FillPayload, RequestDecoder};
use service_async::layer::{FactoryLayer, layer_fn};
use std::{
    fmt::Write,
    future::{Future, poll_fn},
    pin::Pin,
    task::Poll,
};

const DEFAULT_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(75);
const RESPONSE_WRITE_BUFFER_SIZE: usize = 8 * 1024;

/// HTTP/1.1 connection core. `H` handles a single request and returns
/// `(response, keep_connection_open)`.
#[derive(Clone)]
pub(crate) struct HttpH1CoreService<H> {
    inner:                    H,
    keepalive_timeout:        Option<Duration>,
    request_body_limit_bytes: usize,
    request_body_timeout:     Duration,
}

impl<H> HttpH1CoreService<H> {
    pub(crate) fn layer<C>(
        request_body_limit_bytes: usize,
        request_body_timeout: Duration,
    ) -> impl FactoryLayer<C, H, Factory = Self> {
        layer_fn(move |_c: &C, inner| Self {
            inner,
            keepalive_timeout: Some(DEFAULT_KEEPALIVE_TIMEOUT),
            request_body_limit_bytes,
            request_body_timeout,
        })
    }
}

impl<H, S, CX> Service<(S, CX)> for HttpH1CoreService<H>
where
    S: Split + AsyncReadRent + AsyncWriteRent + 'static,
    CX: Clone + 'static,
    H: Service<(Request<HttpBody>, CX), Response = (Response<HttpBody>, bool)> + 'static,
    H::Error: std::fmt::Debug + Into<BoxError>,
{
    type Response = ();
    type Error = BoxError;

    async fn call(&self, (stream, cx): (S, CX)) -> Result<Self::Response, Self::Error> {
        let (reader, writer) = stream.into_split();
        let mut decoder = RequestDecoder::new(reader);
        let mut encoder = DownstreamResponseEncoder::new(writer);
        decoder.set_timeout(self.keepalive_timeout);

        loop {
            let decoded = match decoder.next().await {
                Some(Ok(req)) => req,
                Some(Err(err)) => {
                    warn!(?err, "failed to decode downstream HTTP request");
                    break;
                }
                None => break,
            };

            if has_ambiguous_request_framing(&decoded) {
                let resp = connection_close(json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "content-length and transfer-encoding cannot be combined",
                ));
                let _ = encoder.send_and_flush(resp).await;
                break;
            }
            if declared_content_length(&decoded)
                .is_some_and(|length| length > self.request_body_limit_bytes)
            {
                let resp = connection_close(json_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "invalid_request_error",
                    "request body exceeds configured limit",
                ));
                let _ = encoder.send_and_flush(resp).await;
                break;
            }

            let req = HttpBody::request(decoded);
            // Decode the request body only as the handler consumes it. This
            // avoids filling monoio-http's stream queue before the configured
            // size limit can run, and keeps chunked uploads backpressured by
            // business-layer body reads.
            let mut fill = std::pin::pin!(decoder.fill_payload());
            let mut fill_result = None;
            let mut body_deadline = std::pin::pin!(monoio::time::sleep(self.request_body_timeout));
            let handled = poll_with_accompany_until(
                self.inner.call((req, cx.clone())),
                fill.as_mut(),
                &mut fill_result,
                body_deadline.as_mut(),
            )
            .await;
            let (mut resp, keep_alive) = match handled {
                Err(RequestBodyTimedOut) => {
                    let resp = connection_close(json_error(
                        StatusCode::REQUEST_TIMEOUT,
                        "request_timeout",
                        "timed out while reading request body",
                    ));
                    let _ = encoder.send_and_flush(resp).await;
                    break;
                }
                Ok(Ok(out)) => out,
                Ok(Err(err)) => {
                    let err: BoxError = err.into();
                    error!(?err, "downstream handler failed");
                    let resp = json_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "internal_error",
                        "internal server error",
                    );
                    let _ = encoder.send_and_flush(resp).await;
                    break;
                }
            };

            // A chunked body can exceed the limit only after decoding starts.
            // Stop polling the request reader and close after the 413 rather
            // than draining an attacker-controlled remainder.
            if resp.status() == StatusCode::PAYLOAD_TOO_LARGE {
                resp = connection_close(resp);
                if let Err(err) = encoder.send_and_flush(resp).await {
                    warn!(?err, "failed to encode/write downstream response");
                }
                break;
            }

            let sent = poll_with_accompany_until(
                encoder.send_and_flush(resp),
                fill.as_mut(),
                &mut fill_result,
                body_deadline.as_mut(),
            )
            .await;
            match sent {
                Err(RequestBodyTimedOut) => {
                    warn!("downstream request body timed out while writing response");
                    break;
                }
                Ok(Err(err)) => {
                    warn!(?err, "failed to encode/write downstream response");
                    break;
                }
                Ok(Ok(())) => {}
            }
            if !keep_alive {
                break;
            }

            let filled = match fill_result.take() {
                Some(result) => Ok(result),
                None => poll_accompany_until(fill.as_mut(), body_deadline.as_mut()).await,
            };
            match filled {
                Err(RequestBodyTimedOut) => {
                    warn!("downstream request body timed out after response");
                    break;
                }
                Ok(Err(err)) => {
                    warn!(?err, "failed to fill downstream request payload");
                    break;
                }
                Ok(Ok(())) => {}
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct RequestBodyTimedOut;

async fn poll_with_accompany_until<M, A, D>(
    main: M,
    mut accompany: Pin<&mut A>,
    accompany_result: &mut Option<A::Output>,
    mut deadline: Pin<&mut D>,
) -> Result<M::Output, RequestBodyTimedOut>
where
    M: Future,
    A: Future,
    D: Future<Output = ()>,
{
    let mut main = std::pin::pin!(main);
    poll_fn(|cx| {
        if accompany_result.is_none() {
            match accompany.as_mut().poll(cx) {
                Poll::Ready(result) => *accompany_result = Some(result),
                Poll::Pending => {
                    if deadline.as_mut().poll(cx).is_ready() {
                        return Poll::Ready(Err(RequestBodyTimedOut));
                    }
                }
            }
        }
        main.as_mut().poll(cx).map(Ok)
    })
    .await
}

async fn poll_accompany_until<A, D>(
    mut accompany: Pin<&mut A>,
    mut deadline: Pin<&mut D>,
) -> Result<A::Output, RequestBodyTimedOut>
where
    A: Future,
    D: Future<Output = ()>,
{
    poll_fn(|cx| match accompany.as_mut().poll(cx) {
        Poll::Ready(result) => Poll::Ready(Ok(result)),
        Poll::Pending if deadline.as_mut().poll(cx).is_ready() => {
            Poll::Ready(Err(RequestBodyTimedOut))
        }
        Poll::Pending => Poll::Pending,
    })
    .await
}

fn declared_content_length<B>(request: &Request<B>) -> Option<usize> {
    request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

fn has_ambiguous_request_framing<B>(request: &Request<B>) -> bool {
    request.headers().contains_key(header::CONTENT_LENGTH)
        && request.headers().contains_key(header::TRANSFER_ENCODING)
}

fn connection_close(mut response: Response<HttpBody>) -> Response<HttpBody> {
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    response
}

/// HTTP/1 response encoder with low-latency SSE flushing.
///
/// `monoio-http` deliberately coalesces small streaming chunks into an 8 KiB
/// buffer. That is a good default for bulk bodies, but LLM SSE chunks are often
/// only tens or hundreds of bytes, so the default can delay the first visible
/// token until many events have accumulated. This encoder preserves the same
/// coalescing policy for ordinary chunked responses and flushes after each
/// non-empty chunk only when the response is `text/event-stream`.
struct DownstreamResponseEncoder<W> {
    writer: W,
    buffer: BytesMut,
}

impl<W> DownstreamResponseEncoder<W>
where
    W: AsyncWriteRent,
{
    fn new(writer: W) -> Self {
        Self {
            writer,
            buffer: BytesMut::with_capacity(RESPONSE_WRITE_BUFFER_SIZE),
        }
    }

    async fn send_and_flush(&mut self, response: Response<HttpBody>) -> Result<(), HttpError> {
        let event_stream = is_event_stream(response.headers());
        let (mut parts, mut body) = response.into_parts();
        if event_stream {
            parts.headers.insert(
                HeaderName::from_static("x-accel-buffering"),
                HeaderValue::from_static("no"),
            );
        }

        match body.stream_hint() {
            monoio_http::common::body::StreamHint::None => {
                encode_response_head(&mut self.buffer, &parts, ResponseLength::Empty);
            }
            monoio_http::common::body::StreamHint::Fixed => {
                let data = body.next_data().await.transpose()?.unwrap_or_default();
                encode_response_head(&mut self.buffer, &parts, ResponseLength::Fixed(data.len()));
                if self.buffer.len().saturating_add(data.len()) > RESPONSE_WRITE_BUFFER_SIZE {
                    self.flush_buffer().await?;
                    let (result, _) = self.writer.write_all(data).await;
                    result?;
                    self.writer.flush().await?;
                } else {
                    self.buffer.extend_from_slice(&data);
                }
            }
            monoio_http::common::body::StreamHint::Stream => {
                encode_response_head(&mut self.buffer, &parts, ResponseLength::Chunked);
                if event_stream {
                    self.flush_buffer().await?;
                }

                while let Some(data) = body.next_data().await {
                    let data = data?;
                    if data.is_empty() {
                        continue;
                    }
                    if data.len() >= RESPONSE_WRITE_BUFFER_SIZE {
                        encode_chunk_head(&mut self.buffer, data.len());
                        self.flush_buffer().await?;
                        let (result, _) = self.writer.write_all(data).await;
                        result?;
                        self.buffer.extend_from_slice(b"\r\n");
                        if event_stream {
                            self.flush_buffer().await?;
                        }
                    } else {
                        encode_chunk(&mut self.buffer, &data);
                        if event_stream || self.buffer.len() >= RESPONSE_WRITE_BUFFER_SIZE {
                            self.flush_buffer().await?;
                        }
                    }
                }
                self.buffer.extend_from_slice(b"0\r\n\r\n");
            }
        }

        self.flush_buffer().await
    }

    async fn flush_buffer(&mut self) -> Result<(), HttpError> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        // `write_all` owns the buffer while the operation is in flight. Leave
        // an allocation-free placeholder and restore the returned allocation;
        // SSE flushes once per token, so allocating a fresh 8 KiB buffer here
        // would dominate the hot path.
        let buffer = std::mem::take(&mut self.buffer);
        let (result, mut buffer) = self.writer.write_all(buffer).await;
        result?;
        buffer.clear();
        self.buffer = buffer;
        self.writer.flush().await?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum ResponseLength {
    Empty,
    Fixed(usize),
    Chunked,
}

fn encode_response_head(dst: &mut BytesMut, parts: &http::response::Parts, length: ResponseLength) {
    let version = if parts.version == http::Version::HTTP_10 {
        "HTTP/1.0"
    } else {
        "HTTP/1.1"
    };
    let reason = parts.status.canonical_reason().unwrap_or("<none>");
    write!(dst, "{version} {} {reason}\r\n", parts.status.as_u16())
        .expect("writing response head to BytesMut cannot fail");
    match length {
        ResponseLength::Empty => dst.extend_from_slice(b"content-length: 0\r\n"),
        ResponseLength::Fixed(length) => {
            write!(dst, "content-length: {length}\r\n")
                .expect("writing content length to BytesMut cannot fail");
        }
        ResponseLength::Chunked => dst.extend_from_slice(b"transfer-encoding: chunked\r\n"),
    }
    for (name, value) in &parts.headers {
        if name == header::CONTENT_LENGTH || name == header::TRANSFER_ENCODING {
            continue;
        }
        dst.extend_from_slice(name.as_str().as_bytes());
        dst.extend_from_slice(b": ");
        dst.extend_from_slice(value.as_bytes());
        dst.extend_from_slice(b"\r\n");
    }
    dst.extend_from_slice(b"\r\n");
}

fn encode_chunk(dst: &mut BytesMut, data: &[u8]) {
    encode_chunk_head(dst, data.len());
    dst.extend_from_slice(data);
    dst.extend_from_slice(b"\r\n");
}

fn encode_chunk_head(dst: &mut BytesMut, length: usize) {
    write!(dst, "{length:X}\r\n").expect("writing chunk length to BytesMut cannot fail");
}

fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
}

impl<F> service_async::MakeService for HttpH1CoreService<F>
where
    F: service_async::MakeService,
{
    type Service = HttpH1CoreService<F::Service>;
    type Error = F::Error;

    fn make_via_ref(&self, old: Option<&Self::Service>) -> Result<Self::Service, Self::Error> {
        Ok(HttpH1CoreService {
            inner:                    self.inner.make_via_ref(old.map(|o| &o.inner))?,
            keepalive_timeout:        self.keepalive_timeout,
            request_body_limit_bytes: self.request_body_limit_bytes,
            request_body_timeout:     self.request_body_timeout,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use monoio::{
        BufResult,
        buf::{IoBuf, IoVecBuf},
        io::{AsyncReadRent, AsyncWriteRent, AsyncWriteRentExt},
        net::{TcpListener, TcpStream},
    };
    use monoio_http::h1::payload::{Payload, stream_payload_pair};
    use std::{cell::RefCell, convert::Infallible, io, rc::Rc};

    #[derive(Default)]
    struct WriteState {
        bytes:       Vec<u8>,
        write_sizes: Vec<usize>,
        flushes:     usize,
    }

    struct RecordingWriter {
        state: Rc<RefCell<WriteState>>,
    }

    impl AsyncWriteRent for RecordingWriter {
        fn write<T: IoBuf>(&mut self, buf: T) -> impl Future<Output = BufResult<usize, T>> {
            let len = buf.bytes_init();
            let bytes = unsafe { std::slice::from_raw_parts(buf.read_ptr(), len) };
            let mut state = self.state.borrow_mut();
            state.bytes.extend_from_slice(bytes);
            state.write_sizes.push(len);
            async move { (Ok(len), buf) }
        }

        async fn writev<T: IoVecBuf>(&mut self, buf_vec: T) -> BufResult<usize, T> {
            (
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "recording writer does not use vectored writes",
                )),
                buf_vec,
            )
        }

        fn flush(&mut self) -> impl Future<Output = io::Result<()>> {
            self.state.borrow_mut().flushes += 1;
            async { Ok(()) }
        }

        async fn shutdown(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn streaming_response(content_type: &'static str) -> (Response<HttpBody>, StreamPayloadSender) {
        let (payload, sender) = stream_payload_pair::<Bytes, HttpError>();
        let response = Response::builder()
            .header(header::CONTENT_TYPE, content_type)
            .body(HttpBody::H1(Payload::Stream(payload)))
            .expect("stream response");
        (response, sender)
    }

    #[monoio::test(enable_timer = true)]
    async fn event_stream_flushes_a_small_chunk_before_eof() {
        let state = Rc::new(RefCell::new(WriteState::default()));
        let writer = RecordingWriter {
            state: Rc::clone(&state),
        };
        let mut encoder = DownstreamResponseEncoder::new(writer);
        let (response, mut sender) = streaming_response("text/event-stream; charset=utf-8");
        let task = monoio::spawn(async move { encoder.send_and_flush(response).await });

        sender.feed_data(Some(Bytes::from_static(b"data: first\n\n")));
        monoio::time::sleep(Duration::from_millis(1)).await;

        {
            let state_ref = state.borrow();
            assert!(
                state_ref.flushes >= 2,
                "response head and first SSE chunk must flush"
            );
            assert!(
                state_ref
                    .bytes
                    .windows(b"data: first\n\n".len())
                    .any(|window| window == b"data: first\n\n"),
                "first SSE event must be visible before EOF"
            );
            assert!(
                state_ref
                    .bytes
                    .windows(b"x-accel-buffering: no".len())
                    .any(|window| window == b"x-accel-buffering: no"),
                "reverse proxies must be told not to buffer SSE"
            );
        }

        sender.feed_data(None);
        task.await.expect("encode SSE response");
    }

    #[monoio::test(enable_timer = true)]
    async fn ordinary_chunked_response_keeps_small_chunk_coalescing() {
        let state = Rc::new(RefCell::new(WriteState::default()));
        let writer = RecordingWriter {
            state: Rc::clone(&state),
        };
        let mut encoder = DownstreamResponseEncoder::new(writer);
        let (response, mut sender) = streaming_response("application/octet-stream");
        let task = monoio::spawn(async move { encoder.send_and_flush(response).await });

        sender.feed_data(Some(Bytes::from_static(b"small body chunk")));
        monoio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(
            state.borrow().flushes,
            0,
            "ordinary small chunks stay coalesced"
        );

        sender.feed_data(None);
        task.await.expect("encode ordinary response");
        assert_eq!(state.borrow().flushes, 1);
    }

    #[monoio::test(enable_timer = true)]
    async fn large_chunked_response_writes_body_without_copying_into_encoder_buffer() {
        let state = Rc::new(RefCell::new(WriteState::default()));
        let writer = RecordingWriter {
            state: Rc::clone(&state),
        };
        let mut encoder = DownstreamResponseEncoder::new(writer);
        let (response, mut sender) = streaming_response("application/octet-stream");
        let task = monoio::spawn(async move { encoder.send_and_flush(response).await });
        let large = Bytes::from(vec![b'x'; RESPONSE_WRITE_BUFFER_SIZE * 2]);
        let large_len = large.len();
        sender.feed_data(Some(large));
        sender.feed_data(None);
        task.await.expect("encode large response");

        assert!(
            state.borrow().write_sizes.contains(&large_len),
            "large body must be passed directly to the writer"
        );
    }

    #[derive(Clone)]
    struct BodyLimitHandler {
        limit:         usize,
        panic_on_call: bool,
    }

    impl Service<(Request<HttpBody>, ())> for BodyLimitHandler {
        type Response = (Response<HttpBody>, bool);
        type Error = Infallible;

        async fn call(
            &self,
            (request, ()): (Request<HttpBody>, ()),
        ) -> Result<Self::Response, Self::Error> {
            assert!(
                !self.panic_on_call,
                "oversized content-length reached handler"
            );
            let mut body = request.into_body();
            let mut received = 0usize;
            while let Some(chunk) = body.next_data().await {
                let chunk = chunk.expect("decode test request body");
                if chunk.len() > self.limit.saturating_sub(received) {
                    return Ok((
                        json_error(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "invalid_request_error",
                            "request body exceeds configured limit",
                        ),
                        false,
                    ));
                }
                received += chunk.len();
            }
            Ok((
                json_response(StatusCode::OK, serde_json::json!({"ok": true})),
                false,
            ))
        }
    }

    async fn start_test_core(
        handler: BodyLimitHandler,
        limit: usize,
        body_timeout: Duration,
    ) -> (SocketAddr, monoio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind downstream test server");
        let address = listener.local_addr().expect("downstream test address");
        let server = monoio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept downstream test");
            let core = HttpH1CoreService {
                inner:                    handler,
                keepalive_timeout:        Some(Duration::from_millis(500)),
                request_body_limit_bytes: limit,
                request_body_timeout:     body_timeout,
            };
            core.call((stream, ()))
                .await
                .expect("serve downstream test");
        });
        (address, server)
    }

    async fn read_response_to_eof(stream: &mut TcpStream) -> Vec<u8> {
        monoio::time::timeout(Duration::from_millis(500), async {
            let mut response = Vec::new();
            loop {
                let (result, buffer) = stream.read(vec![0u8; 4096]).await;
                let read = result.expect("read downstream response");
                response.extend_from_slice(&buffer[..read]);
                if read == 0 {
                    return response;
                }
            }
        })
        .await
        .expect("downstream response timeout")
    }

    #[monoio::test_all(enable_timer = true)]
    async fn rejects_oversized_content_length_before_reading_body() {
        let (address, server) = start_test_core(
            BodyLimitHandler {
                limit:         4,
                panic_on_call: true,
            },
            4,
            Duration::from_millis(100),
        )
        .await;
        let mut stream = TcpStream::connect(address)
            .await
            .expect("connect downstream test");
        stream
            .write_all(
                b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\r\n".to_vec(),
            )
            .await
            .0
            .expect("write oversized request head");

        let response = read_response_to_eof(&mut stream).await;
        assert!(response.starts_with(b"HTTP/1.1 413"));
        server.await;
    }

    #[monoio::test_all(enable_timer = true)]
    async fn chunked_body_limit_returns_without_draining_attacker_remainder() {
        let (address, server) = start_test_core(
            BodyLimitHandler {
                limit:         4,
                panic_on_call: false,
            },
            4,
            Duration::from_millis(100),
        )
        .await;
        let mut stream = TcpStream::connect(address)
            .await
            .expect("connect downstream test");
        // Deliberately omit the terminating zero chunk. The handler must see
        // the first decoded chunk, return 413, and close instead of draining.
        stream
            .write_all(
                b"POST / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n"
                    .to_vec(),
            )
            .await
            .0
            .expect("write oversized chunked request");

        let response = read_response_to_eof(&mut stream).await;
        assert!(response.starts_with(b"HTTP/1.1 413"));
        server.await;
    }

    #[monoio::test_all(enable_timer = true)]
    async fn stalled_chunked_request_body_times_out() {
        let (address, server) = start_test_core(
            BodyLimitHandler {
                limit:         1024,
                panic_on_call: false,
            },
            1024,
            Duration::from_millis(20),
        )
        .await;
        let mut stream = TcpStream::connect(address)
            .await
            .expect("connect downstream test");
        stream
            .write_all(
                b"POST / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n"
                    .to_vec(),
            )
            .await
            .0
            .expect("write stalled chunked request head");

        let response = read_response_to_eof(&mut stream).await;
        assert!(response.starts_with(b"HTTP/1.1 408"));
        server.await;
    }
}
