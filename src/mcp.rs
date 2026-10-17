//! The MCP surface.
//!
//! No load tool: everything is on the bench before the agent's first call, so the agent spends no
//! steps loading.
//!
//! `dialect` is the one tool that is not about the data. A dialect primer in the instructions
//! would repeat in every request; the reference lives behind a call instead, and only the short
//! deltas that must always be known ride in the instructions.

use crate::supervisor::Supervisor;
use crate::Caps;
use rmcp::handler::server::ServerHandler;
use rmcp::model::*;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};
use std::sync::{Arc, Mutex};

const CATALOGUE_BUDGET: usize = 4096;

/// The dialect reference, embedded so the served text and the repository's copy cannot drift.
const DIALECT: &str = include_str!("../docs/DIALECT.md");

/// The reference as the agent gets it: the file without its maintainers' blocks — how it was
/// measured, which tests keep it true — which cost the agent bytes and tell it nothing
/// (`the_dialect_reference_is_written_for_the_agent`).
fn for_the_agent(doc: &str) -> String {
    const OPEN: &str = "<!-- maintainers -->";
    const CLOSE: &str = "<!-- /maintainers -->";
    let mut out = String::new();
    let mut rest = doc;
    while let Some(i) = rest.find(OPEN) {
        out.push_str(&rest[..i]);
        rest = match rest[i..].find(CLOSE) {
            Some(j) => rest[i + j + CLOSE.len()..].trim_start_matches('\n'),
            None => "",
        };
    }
    out.push_str(rest);
    out.trim_end().to_string() + "\n"
}

pub struct Bench {
    /// Behind a mutex because there is one engine, and behind an `Arc` because every call to it
    /// runs on the blocking pool: a query can take its whole deadline, and holding the async
    /// runtime for that long would stop the server answering anything else meanwhile.
    engine: Arc<Mutex<Supervisor>>,
    caps: Caps,
    /// What the engine reported at startup, never a literal. REQ-11 `version_not_hardcoded`.
    engine_version: String,
}

fn schema(props: serde_json::Value, required: &[&str]) -> Arc<JsonObject> {
    let v = serde_json::json!({
        "type": "object", "properties": props,
        "required": required, "additionalProperties": false
    });
    Arc::new(v.as_object().unwrap().clone())
}

fn text(s: String) -> CallToolResponse {
    CallToolResult::success(vec![ContentBlock::text(s)]).into()
}
fn failed(s: String) -> CallToolResponse {
    CallToolResult::error(vec![ContentBlock::text(s)]).into()
}

impl Bench {
    /// Run `f` against the engine on the blocking pool, so a long query never holds the async
    /// runtime and the server keeps answering while it runs.
    async fn on_engine<F>(&self, f: F) -> crate::proto::Reply
    where
        F: FnOnce(&mut Supervisor, &Caps) -> crate::proto::Reply + Send + 'static,
    {
        let engine = self.engine.clone();
        let caps = self.caps.clone();
        tokio::task::spawn_blocking(move || {
            let mut e = engine.lock().unwrap_or_else(|p| p.into_inner());
            f(&mut e, &caps)
        })
        .await
        .unwrap_or_else(|e| crate::proto::Reply::err(format!("the engine call failed: {e}")))
    }

    pub fn new(engine: Supervisor, caps: Caps, engine_version: String) -> Self {
        Self {
            engine: Arc::new(Mutex::new(engine)),
            caps,
            engine_version,
        }
    }
}

impl ServerHandler for Bench {
    fn get_info(&self) -> ServerConfig {
        let mut me = Implementation::default();
        me.name = "benchy-mcp".into();
        me.version = env!("CARGO_PKG_VERSION").into();

        let mut cfg = ServerConfig::default();
        cfg.protocol_version = ProtocolVersion::LATEST;
        cfg.capabilities = ServerCapabilities::builder().enable_tools().build();
        cfg.server_info = me;
        cfg.instructions = Some(format!(
            "A workbench: the data is already loaded as tables. Call describe first, then \
             query. SQL is DuckDB {} — QUALIFY, GROUP BY ALL, SELECT * EXCLUDE, ASOF JOIN and \
             PIVOT all work, // is integer division, and lists are 1-indexed. File and \
             network functions (read_csv, read_parquet, ATTACH, INSTALL) are unavailable and \
             will be refused: the data is already here. Results are capped; a truncated \
             result says so. Call dialect if a query fails on syntax — errors already name \
             their fix, so read the error first.",
            self.engine_version
        ));
        cfg
    }

    async fn list_tools(
        &self,
        _r: Option<PaginatedRequestParams>,
        _c: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let read = ToolAnnotations::new().read_only(true);
        let mut describe = Tool::new(
            "describe",
            "What is on the bench: every table, its row count and its columns, and the shape \
             of its data — the time column, its range and cadence, the series key, and which \
             series are dead, start late, stop, pause, skip or repeat rows; and the views and \
             macros the operator provides, if any. Call this first.",
            schema(serde_json::json!({}), &[]),
        );
        describe.annotations = Some(read.clone());

        let mut query = Tool::new(
            "query",
            "Run SQL over the bench. Several statements separated by `;` are allowed and every \
             statement's result comes back. If one fails, nothing after it runs; the ones before \
             it keep their effects, and their results come back after the error. You may CREATE \
             your own tables, views and macros and they persist between calls, under any name \
             not already taken: a loaded table's, a helper's, a table's a helper reads or a \
             system view's like `sqlite_master`, and for a macro a built-in function's like \
             `lower`. The loaded tables are read-only. Returns bounded rows and a result_id.",
            schema(
                serde_json::json!({ "sql": { "type": "string",
                    "description": "DuckDB SQL. One or more statements." } }),
                &["sql"],
            ),
        );
        query.annotations = Some(read.clone());

        let mut get_result = Tool::new(
            "get_result",
            "Fetch a retained result by its result_id, or read further into it: `offset` is the \
             row to start from, as a truncated result tells you. The last 32 results are kept, \
             up to 2,000 rows each; paging never runs the query again.",
            schema(
                serde_json::json!({
                    "result_id": { "type": "string" },
                    "offset": { "type": "integer", "minimum": 0 }
                }),
                &["result_id"],
            ),
        );
        get_result.annotations = Some(read.clone());

        let mut dialect = Tool::new(
            "dialect",
            "DuckDB syntax reference: what differs from Postgres, MySQL, T-SQL and Oracle, and \
             what DuckDB has that they do not. Errors already name their fix — read the error \
             first and call this only if you need more.",
            schema(serde_json::json!({}), &[]),
        );
        dialect.annotations = Some(read);

        // MCP 2026-07-28 requires a list result to say how long it stays fresh and who may cache
        // it, and a client speaking it (Claude Code 2.1) refuses the tools without both
        // (`a_client_without_initialize_can_list_the_tools`). The tools are fixed for the life
        // of the binary; an hour keeps a client from listing them again each turn without
        // keeping them long past an upgrade. Private: a stdio server answers one user, and
        // nothing is gained by letting anything else cache what it said.
        Ok(
            ListToolsResult::with_all_items(vec![describe, query, get_result, dialect])
                .with_ttl_ms(3_600_000)
                .with_cache_scope(CacheScope::Private),
        )
    }

    async fn call_tool(
        &self,
        req: CallToolRequestParams,
        _c: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let args = req.arguments.unwrap_or_default();
        // Every response obeys the byte cap as the client receives it. The renderers keep inside
        // it already and say what they cut; this is the backstop for everything else — the
        // dialect reference, a catalogue, an error.
        let cap = self.caps.max_bytes;
        let text = |s: String| text(crate::result::fit(&s, cap));
        let failed = |s: String| failed(crate::result::fit(&s, cap));
        match req.name.as_ref() {
            "dialect" => Ok(text(format!(
                "The engine is DuckDB {}.\n\n{}",
                self.engine_version,
                for_the_agent(DIALECT)
            ))),
            "describe" => {
                let budget = CATALOGUE_BUDGET.min(cap);
                let r = self.on_engine(move |e, _| e.describe(budget)).await;
                Ok(if r.ok { text(r.body) } else { failed(r.body) })
            }
            "query" => {
                let Some(sql) = args.get("sql").and_then(|v| v.as_str()) else {
                    return Ok(failed("`sql` must be a string".into()));
                };
                let sql = sql.to_string();
                let r = self.on_engine(move |e, caps| e.query(sql, caps)).await;
                Ok(if r.ok { text(r.body) } else { failed(r.body) })
            }
            "get_result" => {
                let Some(id) = args.get("result_id").and_then(|v| v.as_str()) else {
                    return Ok(failed("`result_id` must be a string".into()));
                };
                let offset = match args.get("offset") {
                    None => 0,
                    Some(v) => match v.as_u64() {
                        Some(n) => n as usize,
                        None => return Ok(failed("`offset` must be a whole number".into())),
                    },
                };
                let id = id.to_string();
                let r = self
                    .on_engine(move |e, caps| e.page(id, offset, caps))
                    .await;
                Ok(if r.ok { text(r.body) } else { failed(r.body) })
            }
            other => Ok(failed(format!("unknown tool `{other}`"))),
        }
    }
}
