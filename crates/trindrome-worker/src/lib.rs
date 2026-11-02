//! Local orchestration for Trindrome: configuration, durable progress in SQLite and
//! the worker loop. The mathematics lives in `trindrome-core`.

pub mod configuration;
pub mod persistence;
pub mod scheduler;
