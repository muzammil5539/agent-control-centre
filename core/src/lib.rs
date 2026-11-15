//! Agent Control Centre core: everything except the window. No GUI dependencies, so the
//! `acc` CLI, the SDK client and the tests build without a display.
pub mod api;
pub mod auth;
pub mod cli;
pub mod config;
pub mod log;
pub mod procs;
pub mod sdk;
pub mod store;
