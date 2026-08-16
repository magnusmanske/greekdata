#![forbid(unsafe_code)]

//! Scrapes Greek public-interest data from primary sources, normalizes it, and
//! stores it with full provenance so historical records are never lost.

pub mod cache;
pub mod config;
pub mod db;
pub mod error;
pub mod greek;
pub mod locate;
pub mod model;
pub mod pdf;
pub mod server;
pub mod sources;
pub mod update;

pub use error::{Error, Result};
