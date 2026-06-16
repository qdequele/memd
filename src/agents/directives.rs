//! The managed memd directive block injected into agent instruction files
//! (Claude Code's CLAUDE.md, Codex's AGENTS.md, Gemini's GEMINI.md). Idempotent
//! via the `<!-- memd:start --> … <!-- memd:end -->` markers.

use anyhow::{Context, Result};
use std::path::Path;

const DIRECTIVE_START: &str = "<!-- memd:start (managed by `memd directives`) -->";
const DIRECTIVE_END: &str = "<!-- memd:end -->";

/// The managed directive block injected into agent instruction files.
pub fn directive_block() -> String {
    format!(
        "{DIRECTIVE_START}\n\
## Memory (memd)\n\
`memd` is your persistent, cross-tool long-term memory — an always-on local MCP \
server shared with all of your LLM tools.\n\
- At the start of a task, call `get_memory` (set `scope` to the project path) to \
load prior decisions, preferences, and facts before acting — don't ask the user \
to repeat context you can recall.\n\
- When you learn something durable (a decision, preference, fact, or reusable \
solution), call `save_memory`.\n\
- Use `read_memory(id)` for the full text and `list_memories` to browse.\n\
{DIRECTIVE_END}"
    )
}

/// Insert or replace the managed directive block in `path` (creating the file).
pub fn upsert_directive(path: &Path) -> Result<()> {
    let block = directive_block();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let next = if let Some(region) = managed_region(&existing) {
        existing.replacen(&region, &block, 1)
    } else if existing.trim().is_empty() {
        format!("{block}\n")
    } else {
        format!("{}\n\n{block}\n", existing.trim_end())
    };
    std::fs::write(path, next).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Remove the managed directive block from `path`. Returns whether it changed.
pub fn remove_directive(path: &Path) -> Result<bool> {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return Ok(false);
    };
    let Some(region) = managed_region(&existing) else {
        return Ok(false);
    };
    let cleaned = existing.replacen(&region, "", 1);
    let cleaned = cleaned.replace("\n\n\n", "\n\n");
    std::fs::write(path, cleaned.trim_start())?;
    Ok(true)
}

/// Extract the current `<!-- memd:start --> … <!-- memd:end -->` region, if any.
fn managed_region(text: &str) -> Option<String> {
    let start = text.find(DIRECTIVE_START)?;
    let end = text.find(DIRECTIVE_END)? + DIRECTIVE_END.len();
    if end > start {
        Some(text[start..end].to_string())
    } else {
        None
    }
}

/// Write directives into every registry agent that declares an instruction file.
pub fn install_all() -> Result<()> {
    for agent in super::registry() {
        if let Some(path) = &agent.directives {
            upsert_directive(path)?;
            println!("Wrote memd directives to {}", path.display());
        }
    }
    println!("\nReload your tools (or start a new session) for the directives to take effect.");
    Ok(())
}

/// Remove directives from every registry agent that declares an instruction file.
pub fn remove_all() -> Result<()> {
    for agent in super::registry() {
        if let Some(path) = &agent.directives
            && remove_directive(path)?
        {
            println!("Removed memd directives from {}", path.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_then_remove_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("CLAUDE.md");
        std::fs::write(&p, "# My notes\n\nkeep me\n").unwrap();
        upsert_directive(&p).unwrap();
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains("keep me"));
        assert!(after.contains("## Memory (memd)"));
        // Idempotent: second upsert doesn't duplicate the block.
        upsert_directive(&p).unwrap();
        let twice = std::fs::read_to_string(&p).unwrap();
        assert_eq!(twice.matches("## Memory (memd)").count(), 1);
        // Remove leaves the user's content.
        assert!(remove_directive(&p).unwrap());
        let cleaned = std::fs::read_to_string(&p).unwrap();
        assert!(cleaned.contains("keep me"));
        assert!(!cleaned.contains("## Memory (memd)"));
    }
}
