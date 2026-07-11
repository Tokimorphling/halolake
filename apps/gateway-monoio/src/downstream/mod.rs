//! Downstream HTTP stack (monolake-style Service layers).
//!
//! ```text
//! accept(TcpStream, peer)
//!   -> HttpH1CoreService            # monoio-http decode/encode
//!     -> ConnectionReuseService     # keep-alive policy
//!       -> GatewayAppService        # route + business services
//! ```

pub(crate) mod app;
pub(crate) mod connection;
pub(crate) mod core;

pub(crate) use app::GatewayAppService;
pub(crate) use connection::ConnectionReuseService;
pub(crate) use core::HttpH1CoreService;
