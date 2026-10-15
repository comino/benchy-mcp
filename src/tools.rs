//! The names benchy's texts give the agent's tools: a result's footer says to read on with
//! `get_result`, a hint says to run `describe`. Every such text takes the name from here.
//!
//! A host relays the agent's calls through tools of its own, which need not be called what
//! benchy's MCP server calls them, and an agent told to call a tool that does not exist spends a
//! step finding that out. So a host may name them in `hello` (protocol 4, `tool_names`), and the
//! texts of that session use its names (`texts_name_the_tools_as_the_host_names_them`). Without
//! a map, and always in MCP mode, they are benchy's own.
//!
//! The names are a label in prose and nothing else: benchy resolves nothing by them, so they are
//! held to plain identifiers — no path, URL or anything else a name could address
//! (`tool_names_are_known_tools_plain_names_and_protocol_4`).
//!
//! The names are kept per thread, set by the engine for each request it answers. The texts are
//! made deep inside rendering — a result's footer, a hint, a table's profile — on the engine's one
//! thread, and threading a map through each would touch every one of them for a label.

use std::cell::RefCell;
use std::collections::BTreeMap;

/// benchy's own tools, as its MCP server names them.
pub const TOOLS: [&str; 4] = ["describe", "query", "get_result", "dialect"];

/// The longest name a host may give a tool: as long as MCP lets a tool's name be.
pub const MAX_NAME: usize = 64;

/// A host's names for benchy's tools: only those it renames.
pub type Names = BTreeMap<String, String>;

thread_local! {
    static NAMES: RefCell<Names> = const { RefCell::new(Names::new()) };
}

/// What the texts made on this thread from now on call the tools.
pub fn set(names: &Names) {
    NAMES.with(|n| {
        let mut n = n.borrow_mut();
        if *n != *names {
            *n = names.clone();
        }
    });
}

/// What texts call `tool`, one of [`TOOLS`]: the host's name for it, or benchy's own.
pub fn name(tool: &'static str) -> String {
    debug_assert!(TOOLS.contains(&tool), "{tool} is not one of benchy's tools");
    NAMES
        .with(|n| n.borrow().get(tool).cloned())
        .unwrap_or_else(|| tool.to_string())
}

/// How many bytes longer than benchy's own name the host's name for `tool` is: room a text
/// budgeted for benchy's name has to keep back.
pub fn longer(tool: &'static str) -> usize {
    name(tool).len().saturating_sub(tool.len())
}

/// A host's `tool_names`, checked: an object whose keys are benchy's tools and whose values are
/// plain names — an ASCII letter, then letters, digits, `_` or `-`, at most [`MAX_NAME`] bytes, as
/// tool names are on the platforms agents run on — no two tools under one name.
pub fn parse(asked: &serde_json::Value) -> Result<Names, String> {
    let serde_json::Value::Object(asked) = asked else {
        return Err("`tool_names` must be an object, like {\"get_result\": \"page\"}".into());
    };
    let mut names = Names::new();
    for (tool, given) in asked {
        if !TOOLS.contains(&tool.as_str()) {
            return Err(format!(
                "`tool_names` has no tool `{tool}`; benchy's tools are {}",
                TOOLS.join(", ")
            ));
        }
        let plain = given.as_str().filter(|g| {
            !g.is_empty()
                && g.len() <= MAX_NAME
                && g.starts_with(|c: char| c.is_ascii_alphabetic())
                && g.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        });
        let Some(given) = plain else {
            return Err(format!(
                "`tool_names.{tool}` must be a plain name: an ASCII letter, then letters, digits, \
                 `_` or `-`, at most {MAX_NAME} bytes"
            ));
        };
        // Two tools under one name would send the agent to one for the other.
        if let Some((other, _)) = names.iter().find(|(_, g)| *g == given) {
            return Err(format!(
                "`tool_names` names `{other}` and `{tool}` both `{given}`"
            ));
        }
        if given != tool && TOOLS.contains(&given) {
            return Err(format!(
                "`tool_names.{tool}` is `{given}`, which is another of benchy's tools"
            ));
        }
        names.insert(tool.clone(), given.to_string());
    }
    Ok(names)
}
