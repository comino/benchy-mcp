//! The frame between the server and its engine process.
//!
//! Four-byte big-endian length, then JSON. No pickle, no eval, nothing that can execute: the
//! channel between a supervisor and the thing it supervises is not a place to be clever.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

/// Anything larger is a bug or an attack; neither deserves an allocation.
pub const MAX_FRAME: u32 = 64 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Seal an empty bench that may attach from `workspace` alone. Sent once per engine.
    Open {
        /// A directory inside the server's session directory, made by the server, which removes
        /// it again.
        workspace: String,
        memory_limit: String,
        /// Serve even where Landlock cannot confine the engine. The operator's explicit choice.
        #[serde(default)]
        allow_unsandboxed: bool,
        /// Where result numbering continues. An id names one result for the life of the server,
        /// so a rebuilt engine must never reuse one its predecessor handed out.
        #[serde(default = "one")]
        first_result_id: u64,
        /// What every result id starts with: the session's own, so ids never repeat across
        /// sessions either.
        #[serde(default)]
        result_prefix: String,
    },
    /// Attach each table `name` from its `file`, a DuckDB file in the workspace, read-only; the
    /// engine unlinks each file as soon as it is attached. One answer per table, in `each`, in
    /// order: several at once, so the check every attach needs is paid once
    /// (`Workspace::attach_all`).
    Attach { tables: Vec<(String, String)> },
    /// Take table `name` away.
    Detach { name: String },
    /// REQ-18: put the operator's helpers on the bench, from `file`, a DuckDB file in the
    /// workspace a helpers job wrote, attached read-only in place of any helpers before it; the
    /// engine unlinks it once attached. `shown` is every helper, made or waiting, as `describe`
    /// lists it: the file must hold exactly the ones made. `reads` is every table a helper has
    /// read this session (`crate::helpers::Built::reads`), which the agent may not name an
    /// object of its own after, on the bench or not.
    Helpers {
        file: String,
        shown: Vec<crate::helpers::Shown>,
        reads: Vec<String>,
    },
    Describe {
        budget: usize,
        /// Tables that were to be on the bench and could not be loaded: listed by name only.
        #[serde(default)]
        unavailable: Vec<String>,
        /// What the texts call the agent's tools, where a host renames them (`crate::tools`).
        #[serde(default, skip_serializing_if = "crate::tools::Names::is_empty")]
        tools: crate::tools::Names,
    },
    Query {
        sql: String,
        max_rows: usize,
        max_bytes: usize,
        max_cell_bytes: usize,
        deadline_ms: u64,
        /// Tables that were to be on the bench and could not be loaded, so an error naming one
        /// says it is not available rather than that it never existed.
        #[serde(default)]
        unavailable: Vec<String>,
        #[serde(default, skip_serializing_if = "crate::tools::Names::is_empty")]
        tools: crate::tools::Names,
    },
    /// Read further into a retained result, without running its query again.
    Page {
        result_id: String,
        offset: usize,
        max_rows: usize,
        max_bytes: usize,
        max_cell_bytes: usize,
        #[serde(default, skip_serializing_if = "crate::tools::Names::is_empty")]
        tools: crate::tools::Names,
    },
}

fn one() -> u64 {
    1
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    /// Rendered text for the agent, or the error.
    pub body: String,
    /// Result ids produced, so the server can retain them without parsing prose.
    #[serde(default)]
    pub result_ids: Vec<String>,
    /// What the engine reported at load, for the startup log.
    #[serde(default)]
    pub tables: Vec<(String, u64)>,
    /// How Landlock confines the engine, reported at load for the startup log.
    #[serde(default)]
    pub posture: Option<crate::sandbox::Posture>,
    /// Why it failed, as a code a host can act on: `E_SQL`, `E_DEADLINE`, `E_NO_RESULT`,
    /// `E_DATA`, `E_NAME`, `E_REBUILT`, `E_UNAVAILABLE` (docs/HOST-PROTOCOL.md).
    #[serde(default)]
    pub code: Option<String>,
    /// An attached table's columns, name and DuckDB type.
    #[serde(default)]
    pub columns: Vec<(String, String)>,
    /// One reply per table of an `Attach`, in the order asked.
    #[serde(default)]
    pub each: Vec<Reply>,
    /// What each result of a `Query` holds and its text shows, in the order of `result_ids`.
    #[serde(default)]
    pub results: Vec<crate::result::Shown>,
    /// The statement a `Page`'s result came from, when it could be told apart in its batch.
    #[serde(default)]
    pub sql: Option<String>,
    /// Why the engine must be rebuilt now that it has answered: a batch left it holding what it
    /// cannot let go (`Workspace::spoiled`). The server answers the request as for an engine it
    /// lost, and never passes this answer on.
    #[serde(default)]
    pub rebuild: Option<String>,
}

impl Reply {
    pub fn ok(body: impl Into<String>) -> Self {
        Self {
            ok: true,
            body: body.into(),
            result_ids: vec![],
            tables: vec![],
            posture: None,
            code: None,
            columns: vec![],
            each: vec![],
            results: vec![],
            sql: None,
            rebuild: None,
        }
    }
    pub fn err(body: impl Into<String>) -> Self {
        Self {
            ok: false,
            ..Self::ok(body)
        }
    }
    /// A failure with the code a host acts on.
    pub fn fail(code: &str, body: impl Into<String>) -> Self {
        Self {
            code: Some(code.to_string()),
            ..Self::err(body)
        }
    }
}

pub fn write_frame<W: Write>(w: &mut W, value: &impl Serialize) -> std::io::Result<()> {
    let body = serde_json::to_vec(value)?;
    w.write_all(&(body.len() as u32).to_be_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// The frame [`write_frame`] writes, as bytes: for a writer that writes it elsewhere, on a thread
/// of its own (`Supervisor::send`). Serialized straight after its length's place, so the body is
/// not copied.
pub fn frame(value: &impl Serialize) -> std::io::Result<Vec<u8>> {
    let mut out = vec![0u8; 4];
    serde_json::to_writer(&mut out, value)?;
    let n = (out.len() - 4) as u32;
    out[..4].copy_from_slice(&n.to_be_bytes());
    Ok(out)
}

pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> std::io::Result<T> {
    let body = read_frame_bytes(r, MAX_FRAME)?;
    Ok(serde_json::from_slice(&body)?)
}

/// One frame's body, unparsed, of at most `cap` bytes: for a reader with its own cap, or its own
/// way of parsing.
pub fn read_frame_bytes<R: Read>(r: &mut R, cap: u32) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let n = u32::from_be_bytes(len);
    if n > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame of {n} bytes exceeds the cap"),
        ));
    }
    let mut body = vec![0u8; n as usize];
    r.read_exact(&mut body)?;
    Ok(body)
}
