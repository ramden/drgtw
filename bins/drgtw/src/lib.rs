//! Library target for the drgtw binary crate.
//!
//! Exposes the internal modules so that integration tests (in `tests/`) can
//! import `drgtw::server::router` and friends without spawning a subprocess.

pub mod middleware;
pub mod routes;
pub mod server;
