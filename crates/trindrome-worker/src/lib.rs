//! Local orchestration for Trindrome: configuration, durable progress in SQLite and
//! the worker loop. The mathematics lives in `trindrome-core`.

pub mod configuration;
pub mod engine;
pub mod gpu_monitor;
pub mod identity;
pub mod idle;
pub mod persistence;
pub mod policy;
pub mod power;
pub mod remote;
pub mod scheduler;
pub mod sensors;
pub mod settings_file;
pub mod startup;
