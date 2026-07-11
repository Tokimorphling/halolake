//! Connection keep-alive policy layer.

use super::super::*;
use http::Version;
use service_async::layer::{layer_fn, FactoryLayer};

const CLOSE: &str = "close";
const KEEPALIVE: &str = "keep-alive";

/// Adjusts request/response Connection headers and reports whether the
/// connection should stay open after the response is written.
#[derive(Clone)]
pub(crate) struct ConnectionReuseService<H> {
    inner: H,
}

impl<H> ConnectionReuseService<H> {
    pub(crate) fn new(inner: H) -> Self {
        Self { inner }
    }

    pub(crate) fn layer<C>() -> impl FactoryLayer<C, H, Factory = Self> {
        layer_fn(|_c: &C, inner| Self::new(inner))
    }
}

impl<H, CX, B> Service<(Request<B>, CX)> for ConnectionReuseService<H>
where
    H: Service<(Request<B>, CX), Response = Response<HttpBody>>,
    H::Error: Into<BoxError>,
{
    type Response = (Response<HttpBody>, bool);
    type Error = BoxError;

    async fn call(
        &self,
        (mut request, cx): (Request<B>, CX),
    ) -> Result<Self::Response, Self::Error> {
        let version = request.version();
        let mut keepalive = is_conn_keepalive(request.headers(), version);

        // Normalize HTTP/1.0 to 1.1-like keep-alive handling (nginx proxy_http_version 1.1).
        if version == Version::HTTP_10 {
            *request.version_mut() = Version::HTTP_11;
            request.headers_mut().remove(header::CONNECTION);
        }

        let mut response = self
            .inner
            .call((request, cx))
            .await
            .map_err(Into::into)?;

        // HTTP/2 is always multiplexed; for H1 honor client + server close.
        if response.version() == Version::HTTP_11 || response.version() == Version::HTTP_10 {
            let server_close = response
                .headers()
                .get(header::CONNECTION)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.eq_ignore_ascii_case(CLOSE));
            keepalive &= !server_close;
            let value = if keepalive { KEEPALIVE } else { CLOSE };
            response
                .headers_mut()
                .insert(header::CONNECTION, HeaderValue::from_static(value));
        }

        Ok((response, keepalive))
    }
}

fn is_conn_keepalive(headers: &HeaderMap, version: Version) -> bool {
    match version {
        Version::HTTP_11 => headers
            .get(header::CONNECTION)
            .and_then(|v| v.to_str().ok())
            .map(|v| !v.eq_ignore_ascii_case(CLOSE))
            .unwrap_or(true),
        Version::HTTP_10 => headers
            .get(header::CONNECTION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains(KEEPALIVE)),
        Version::HTTP_2 => true,
        _ => false,
    }
}

impl<F> service_async::MakeService for ConnectionReuseService<F>
where
    F: service_async::MakeService,
{
    type Service = ConnectionReuseService<F::Service>;
    type Error = F::Error;

    fn make_via_ref(&self, old: Option<&Self::Service>) -> Result<Self::Service, Self::Error> {
        Ok(ConnectionReuseService {
            inner: self.inner.make_via_ref(old.map(|o| &o.inner))?,
        })
    }
}
