//! pgtui — terminal UI browser for PostgreSQL, inspired by pgweb.
//!
//! Exposed as a library so integration tests can drive the real
//! [`db::spawn`] worker thread over its mpsc channel.

pub mod db;
