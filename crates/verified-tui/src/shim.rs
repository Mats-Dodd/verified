//! The one place the `sim` feature swaps IO: tokio's network in production,
//! turmoil's simulated network under simulation.

/// The client's TCP stream. An alias, not a re-export, so naming it outside
/// the shim is not naming tokio's type.
#[cfg(not(feature = "sim"))]
#[expect(
    clippy::disallowed_types,
    reason = "the shim is where the client's real stream is chosen"
)]
pub type TcpStream = tokio::net::TcpStream;
#[cfg(feature = "sim")]
pub type TcpStream = turmoil::net::TcpStream;
