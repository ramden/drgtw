//! Pure auth primitives for the admin UI.
//!
//! No axum, no database, no I/O — every function is a pure computation
//! over its arguments, which makes them trivially unit-testable.

pub mod cookie;
pub mod csrf;
pub mod error;
pub mod password;
pub mod session;
