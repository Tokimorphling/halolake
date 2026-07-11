//! Native monoio-http/1.1 core.
//!
//! Accepts a connected stream, decodes requests with `RequestDecoder`,
//! dispatches to an inner `Service<(Request<HttpBody>, CX)>`, and encodes
//! responses with `GenericEncoder`. Keep-alive is controlled by the inner
//! service returning `(Response, continue)`.

use super::super::*;
use monoio::io::{
    AsyncReadRent, AsyncWriteRent, Split, Splitable, sink::SinkExt, stream::Stream as MonoioStream,
};
use monoio_http::h1::codec::{
    decoder::{FillPayload, RequestDecoder},
    encoder::GenericEncoder,
};
use service_async::layer::{FactoryLayer, layer_fn};

const DEFAULT_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(75);

/// HTTP/1.1 connection core. `H` handles a single request and returns
/// `(response, keep_connection_open)`.
#[derive(Clone)]
pub(crate) struct HttpH1CoreService<H> {
    inner:             H,
    keepalive_timeout: Option<Duration>,
}

impl<H> HttpH1CoreService<H> {
    pub(crate) fn new(inner: H) -> Self {
        Self {
            inner,
            keepalive_timeout: Some(DEFAULT_KEEPALIVE_TIMEOUT),
        }
    }

    pub(crate) fn layer<C>() -> impl FactoryLayer<C, H, Factory = Self> {
        layer_fn(|_c: &C, inner| Self::new(inner))
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
        let mut encoder = GenericEncoder::new(writer);
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

            // Populate body channels before handing the request to business
            // layers. Gateway currently buffers request bodies; concurrent
            // fill (monolake AccompanyPair) can be reintroduced later.
            if let Err(err) = decoder.fill_payload().await {
                warn!(?err, "failed to fill downstream request payload");
                break;
            }

            let req = HttpBody::request(decoded);
            let (resp, keep_alive) = match self.inner.call((req, cx.clone())).await {
                Ok(out) => out,
                Err(err) => {
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

            if let Err(err) = encoder.send_and_flush(resp).await {
                warn!(?err, "failed to encode/write downstream response");
                break;
            }
            if !keep_alive {
                break;
            }
        }
        Ok(())
    }
}

impl<F> service_async::MakeService for HttpH1CoreService<F>
where
    F: service_async::MakeService,
{
    type Service = HttpH1CoreService<F::Service>;
    type Error = F::Error;

    fn make_via_ref(&self, old: Option<&Self::Service>) -> Result<Self::Service, Self::Error> {
        Ok(HttpH1CoreService {
            inner:             self.inner.make_via_ref(old.map(|o| &o.inner))?,
            keepalive_timeout: self.keepalive_timeout,
        })
    }
}
