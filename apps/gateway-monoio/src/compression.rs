//! Downstream request body and upstream response body decompression.
//!
//! Mirrors sub2api's `httputil/body.go` request decoder and
//! `repository/http_upstream.go` response decoder. Clients (Codex TUI) send
//! zstd-compressed Responses API payloads; Anthropic/OpenAI upstreams do not
//! accept `Content-Encoding: zstd` on the request, so the gateway must decode
//! the body and strip the framing headers before forwarding. Upstream
//! responses may be gzip/br/deflate/zstd encoded when the client pinned an
//! `Accept-Encoding`; that path is covered too so SSE translators see raw
//! bytes.
//!
//! Decompression is a synchronous, CPU-bound operation. The monoio runtime is
//! thread-per-core, so blocking calls are dispatched to the blocking pool via
//! `monoio::spawn_blocking` to avoid stalling the event loop. Bodies arriving
//! at this layer are already fully buffered (see `Gateway::collect_request`),
//! which keeps the API synchronous `Vec<u8> -> Vec<u8>`.

use bytes::Bytes;
use http::{
    HeaderMap, Response,
    header::{CONTENT_ENCODING, CONTENT_LENGTH},
};
use monoio_http::common::body::HttpBody;
use std::io::Read;
use tracing::warn;

/// Maximum decompressed body size (64 MiB), matching sub2api's
/// `maxDecompressedBodySize`. Guards against decompression bombs where a tiny
/// compressed payload expands into a multi-GB buffer.
const MAX_DECOMPRESSED_BODY_SIZE: usize = 64 << 20;

/// Errors returned by [`decompress_request_body`]. They carry an
/// `invalid_request_error` style so the gateway can surface them directly to
/// the downstream client without leaking upstream transport details.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DecompressError {
    #[error("unsupported Content-Encoding: {0}")]
    Unsupported(String),
    #[error("decode Content-Encoding {encoding:?}: {message}")]
    Failed {
        encoding: &'static str,
        message:  String,
    },
    #[error("decompressed body exceeds {max} bytes", max = MAX_DECOMPRESSED_BODY_SIZE)]
    TooLarge,
}

/// Inspects `Content-Encoding`, decodes the body if present, and removes the
/// `Content-Encoding` / `Content-Length` headers so they are not forwarded
/// upstream (the upstream body is re-serialized with fresh framing by the
/// relay layer). `identity` and missing encodings are passed through unchanged.
pub(crate) fn decompress_request_body(
    headers: &mut HeaderMap,
    body: Bytes,
) -> Result<Bytes, DecompressError> {
    let Some(encoding_raw) = headers.get(CONTENT_ENCODING) else {
        return Ok(body);
    };
    let encoding = match encoding_raw.to_str() {
        Ok(value) => value.trim().to_ascii_lowercase(),
        Err(_) => {
            headers.remove(CONTENT_ENCODING);
            headers.remove(CONTENT_LENGTH);
            return Err(DecompressError::Unsupported(
                "non-ascii Content-Encoding".to_string(),
            ));
        }
    };
    if encoding.is_empty() || encoding == "identity" {
        return Ok(body);
    }
    // The framing headers describe the wire bytes we just consumed, not the
    // decoded bytes. The relay layer always re-serializes the body and
    // `prepare_upstream_request_headers` recomputes `Content-Length`, so strip
    // both to avoid an upstream framing mismatch. Remove them before decoding
    // so an unsupported/corrupt payload still cannot leak the stale framing
    // into an error response.
    headers.remove(CONTENT_ENCODING);
    headers.remove(CONTENT_LENGTH);
    let decoded = decode(&encoding, body.as_ref())?;
    Ok(Bytes::from(decoded))
}

fn decode(encoding: &str, raw: &[u8]) -> Result<Vec<u8>, DecompressError> {
    match encoding {
        "zstd" => decode_zstd(raw),
        "gzip" | "x-gzip" => decode_gzip(raw),
        "deflate" => decode_deflate(raw),
        "br" => decode_brotli(raw),
        other => Err(DecompressError::Unsupported(other.to_string())),
    }
}

/// Decodes a fully buffered `Content-Encoding` payload. Shared by the
/// downstream request decoder and the upstream response decoder.
pub(crate) fn decode_encoded_body(encoding: &str, raw: &[u8]) -> Result<Bytes, DecompressError> {
    decode(encoding, raw).map(Bytes::from)
}

/// Decodes an upstream response body when `Content-Encoding` is present.
/// Mirrors sub2api's `decompressResponseBody` semantics: on success the
/// `Content-Encoding` / `Content-Length` headers are removed (the length no
/// longer describes the body); on unsupported/corrupt encodings the response
/// is left untouched so the downstream client sees the original bytes.
///
/// Responses without an encoding are returned as-is without buffering, so SSE
/// streaming is unaffected. Encoded bodies must be buffered because the
/// synchronous decompressors below require the complete frame.
pub(crate) async fn decompress_upstream_response(
    mut upstream: Response<HttpBody>,
) -> Response<HttpBody> {
    let Some(encoding_raw) = upstream.headers().get(CONTENT_ENCODING) else {
        return upstream;
    };
    let encoding = match encoding_raw.to_str() {
        Ok(value) => value.trim().to_ascii_lowercase(),
        Err(_) => return upstream,
    };
    if encoding.is_empty()
        || encoding == "identity"
        || !matches!(
            encoding.as_str(),
            "zstd" | "gzip" | "x-gzip" | "deflate" | "br"
        )
    {
        return upstream;
    }
    let payload = match upstream.body_mut().to_ready().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Bytes::new(),
        Err(_) => return upstream,
    };
    // Decompression is CPU-bound and runs on the blocking pool so the monoio
    // event loop is not stalled by adversarial compressed frames.
    let encoding_for_task = encoding.clone();
    let decoded =
        match monoio::spawn_blocking(move || decode_encoded_body(&encoding_for_task, &payload))
            .await
        {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(err)) => {
                warn!(
                    ?err,
                    encoding = encoding.as_str(),
                    "failed to decode upstream response; forwarding original response"
                );
                return upstream;
            }
            Err(err) => {
                warn!(
                    ?err,
                    encoding = encoding.as_str(),
                    "blocking decompression task failed"
                );
                return upstream;
            }
        };
    upstream.headers_mut().remove(CONTENT_ENCODING);
    upstream.headers_mut().remove(CONTENT_LENGTH);
    *upstream.body_mut() = HttpBody::Ready(Some(decoded));
    upstream
}

fn decode_zstd(raw: &[u8]) -> Result<Vec<u8>, DecompressError> {
    let mut decoder =
        ruzstd::decoding::StreamingDecoder::new(raw).map_err(|err| DecompressError::Failed {
            encoding: "zstd",
            message:  err.to_string(),
        })?;
    read_limited(&mut decoder, "zstd")
}

fn decode_gzip(raw: &[u8]) -> Result<Vec<u8>, DecompressError> {
    let mut decoder = flate2::read::GzDecoder::new(raw);
    read_limited(&mut decoder, "gzip")
}

fn decode_deflate(raw: &[u8]) -> Result<Vec<u8>, DecompressError> {
    let mut decoder = flate2::read::ZlibDecoder::new(raw);
    read_limited(&mut decoder, "deflate")
}

fn decode_brotli(raw: &[u8]) -> Result<Vec<u8>, DecompressError> {
    let mut decoder = brotli::Decompressor::new(raw, 4096);
    read_limited(&mut decoder, "br")
}

fn read_limited<R: Read>(
    reader: &mut R,
    encoding: &'static str,
) -> Result<Vec<u8>, DecompressError> {
    // Conservative initial capacity; `Vec` grows as needed and we avoid
    // over-allocating for adversarial inputs that advertise a huge frame size.
    let mut out = Vec::with_capacity(4 * 1024);
    let mut buf = [0u8; 32 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return Ok(out),
            Ok(n) => {
                if out.len().saturating_add(n) > MAX_DECOMPRESSED_BODY_SIZE {
                    return Err(DecompressError::TooLarge);
                }
                out.extend_from_slice(&buf[..n]);
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                return Err(DecompressError::Failed {
                    encoding,
                    message: err.to_string(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{
        Compression,
        write::{GzEncoder, ZlibEncoder},
    };
    use std::io::Write;

    fn gz(input: &[u8]) -> Vec<u8> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(input).unwrap();
        enc.finish().unwrap()
    }

    fn zlib(input: &[u8]) -> Vec<u8> {
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(input).unwrap();
        enc.finish().unwrap()
    }

    fn zstd_frame(input: &[u8]) -> Vec<u8> {
        zstd::encode_all(input, 3).unwrap()
    }

    #[test]
    fn identity_passes_through() {
        let mut headers = HeaderMap::new();
        let body = Bytes::from_static(b"{\"hello\":1}");
        let out = decompress_request_body(&mut headers, body.clone()).unwrap();
        assert_eq!(out, body);
        assert!(!headers.contains_key(CONTENT_ENCODING));
    }

    #[test]
    fn missing_encoding_passes_through() {
        let mut headers = HeaderMap::new();
        let body = Bytes::from_static(b"{}");
        let out = decompress_request_body(&mut headers, body.clone()).unwrap();
        assert_eq!(out, body);
    }

    #[test]
    fn gzip_decodes_and_strips_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_ENCODING, "gzip".parse().unwrap());
        headers.insert(CONTENT_LENGTH, "0".parse().unwrap());
        let payload = br#"{"model":"claude-opus-5","input":[]}"#;
        let body = Bytes::from(gz(payload));
        let out = decompress_request_body(&mut headers, body).unwrap();
        assert_eq!(out.as_ref(), payload);
        assert!(!headers.contains_key(CONTENT_ENCODING));
        assert!(!headers.contains_key(CONTENT_LENGTH));
    }

    #[test]
    fn deflate_decodes() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_ENCODING, "deflate".parse().unwrap());
        let payload = br#"{"ok":true}"#;
        let body = Bytes::from(zlib(payload));
        let out = decompress_request_body(&mut headers, body).unwrap();
        assert_eq!(out.as_ref(), payload);
    }

    #[test]
    fn unsupported_encoding_errors() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_ENCODING, "snappy".parse().unwrap());
        let body = Bytes::from_static(b"whatever");
        let err = decompress_request_body(&mut headers, body).unwrap_err();
        assert!(matches!(err, DecompressError::Unsupported(_)));
        assert!(!headers.contains_key(CONTENT_ENCODING));
    }

    #[test]
    fn corrupt_zstd_errors() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_ENCODING, "zstd".parse().unwrap());
        // Truncate a real zstd frame so the decoder hits an unexpected EOF /
        // block error mid-stream.
        let frame = zstd_frame(b"{\"model\":\"claude-opus-5\"}");
        let truncated = &frame[..frame.len() / 2];
        let body = Bytes::from(truncated.to_vec());
        let err = decompress_request_body(&mut headers, body).unwrap_err();
        assert!(matches!(err, DecompressError::Failed {
            encoding: "zstd",
            ..
        }));
    }

    #[test]
    fn zstd_decodes_round_trip() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_ENCODING, "zstd".parse().unwrap());
        // Mirror the Codex TUI payload: a JSON Responses-API request body
        // compressed with zstd.
        let payload = br#"{"model":"claude-opus-5","stream":true,"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
        let body = Bytes::from(zstd_frame(payload));
        let out = decompress_request_body(&mut headers, body).unwrap();
        assert_eq!(out.as_ref(), payload);
        assert!(!headers.contains_key(CONTENT_ENCODING));
        assert!(!headers.contains_key(CONTENT_LENGTH));
    }

    #[test]
    fn empty_body_with_encoding_decodes_to_empty() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_ENCODING, "gzip".parse().unwrap());
        let body = Bytes::from(gz(b""));
        let out = decompress_request_body(&mut headers, body).unwrap();
        assert!(out.is_empty());
    }

    fn encoded_upstream(encoding: &str, payload: &[u8]) -> Response<HttpBody> {
        Response::builder()
            .header(CONTENT_ENCODING, encoding)
            .header(CONTENT_LENGTH, payload.len().to_string())
            .body(HttpBody::Ready(Some(Bytes::from(payload.to_vec()))))
            .expect("build encoded upstream response")
    }

    /// Runs the future on a monoio runtime with a blocking pool attached, like
    /// the gateway workers. `spawn_blocking` panics without one.
    fn run_with_blocking_pool<F, T>(future: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        let mut rt = monoio::RuntimeBuilder::<monoio::FusionDriver>::new()
            .enable_timer()
            .attach_thread_pool(Box::new(monoio::blocking::DefaultThreadPool::new(2)))
            .build()
            .expect("build monoio runtime");
        rt.block_on(future)
    }

    #[test]
    fn upstream_gzip_response_is_decoded_and_headers_stripped() {
        run_with_blocking_pool(async {
            let payload = b"{\"ok\":true}";
            let response = encoded_upstream("gzip", &gz(payload));
            let mut decoded = decompress_upstream_response(response).await;
            assert!(!decoded.headers().contains_key(CONTENT_ENCODING));
            assert!(!decoded.headers().contains_key(CONTENT_LENGTH));
            let body = decoded.body_mut().to_ready().await.unwrap().unwrap();
            assert_eq!(body.as_ref(), payload);
        })
    }

    #[test]
    fn upstream_deflate_response_is_decoded() {
        run_with_blocking_pool(async {
            let payload = b"{\"ok\":true}";
            let response = encoded_upstream("deflate", &zlib(payload));
            let mut decoded = decompress_upstream_response(response).await;
            assert!(!decoded.headers().contains_key(CONTENT_ENCODING));
            let body = decoded.body_mut().to_ready().await.unwrap().unwrap();
            assert_eq!(body.as_ref(), payload);
        })
    }

    #[test]
    fn upstream_zstd_response_is_decoded() {
        run_with_blocking_pool(async {
            let payload = b"{\"ok\":true}";
            let response = encoded_upstream("zstd", &zstd_frame(payload));
            let mut decoded = decompress_upstream_response(response).await;
            assert!(!decoded.headers().contains_key(CONTENT_ENCODING));
            let body = decoded.body_mut().to_ready().await.unwrap().unwrap();
            assert_eq!(body.as_ref(), payload);
        })
    }

    #[test]
    fn upstream_brotli_response_is_decoded() {
        run_with_blocking_pool(async {
            let payload = b"{\"ok\":true}";
            let mut encoder = brotli::CompressorWriter::new(Vec::new(), 4096, 5, 22);
            encoder.write_all(payload).unwrap();
            let compressed = encoder.into_inner();
            let response = encoded_upstream("br", &compressed);
            let mut decoded = decompress_upstream_response(response).await;
            assert!(!decoded.headers().contains_key(CONTENT_ENCODING));
            let body = decoded.body_mut().to_ready().await.unwrap().unwrap();
            assert_eq!(body.as_ref(), payload);
        })
    }

    #[test]
    fn upstream_without_encoding_passes_through_unchanged() {
        run_with_blocking_pool(async {
            let payload = b"raw response";
            let response = Response::builder()
                .header(CONTENT_LENGTH, payload.len().to_string())
                .body(HttpBody::Ready(Some(Bytes::from_static(payload))))
                .expect("build plain upstream response");
            let mut decoded = decompress_upstream_response(response).await;
            assert!(!decoded.headers().contains_key(CONTENT_ENCODING));
            assert!(decoded.headers().contains_key(CONTENT_LENGTH));
            let body = decoded.body_mut().to_ready().await.unwrap().unwrap();
            assert_eq!(body.as_ref(), payload);
        })
    }

    #[test]
    fn upstream_unsupported_encoding_is_preserved() {
        run_with_blocking_pool(async {
            let payload = b"raw response";
            let response = encoded_upstream("snappy", payload);
            let mut decoded = decompress_upstream_response(response).await;
            assert_eq!(decoded.headers().get(CONTENT_ENCODING).unwrap(), "snappy");
            let body = decoded.body_mut().to_ready().await.unwrap().unwrap();
            assert_eq!(body.as_ref(), payload);
        })
    }

    #[test]
    fn upstream_corrupt_zstd_is_preserved() {
        run_with_blocking_pool(async {
            let payload = b"not a real zstd frame";
            let response = encoded_upstream("zstd", payload);
            let mut decoded = decompress_upstream_response(response).await;
            assert_eq!(decoded.headers().get(CONTENT_ENCODING).unwrap(), "zstd");
            let body = decoded.body_mut().to_ready().await.unwrap().unwrap();
            assert_eq!(body.as_ref(), payload);
        })
    }
}
