//! benchy-mcp — an agent workbench for time-series analytics over the data you already have.
//!
//! Put data on it, work on it with SQL, take the result with you. The bench does not know what
//! you are making and has no opinion about it.
//!
//! Requirements this crate implements are named in each module. `REQUIREMENTS.md` is normative.

pub mod admission;
pub mod canonical;
pub mod config;
pub mod decompress;
pub mod engine;
pub mod exec;
pub mod guard;
pub mod helpers;
pub mod hint;
pub mod host;
pub mod ipc;
pub mod loader;
pub mod lockdown;
pub mod mcp;
pub mod profile;
pub mod proto;
pub mod result;
pub mod sandbox;
pub mod source;
pub mod sources;
pub mod supervisor;
pub mod tools;
pub mod trace;
pub mod workspace;

pub use result::{Caps, QueryResult};
pub use workspace::{Bench, LoadedTable, WorkError, Workspace};
