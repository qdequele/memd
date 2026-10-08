//! Deterministic, heuristics-first classification (PRD §8).
//!
//! No model in the loop: type is inferred from source, path, and filename.
//! The rules are agent-agnostic — every supported agent's instruction and
//! memory conventions are listed here so the crawler indexes the knowledge
//! each tool keeps on disk and makes it visible to all the others.

use super::model::{MemoryType, Source};
use std::path::Path;

/// Infer a memory type from its origin.
///
/// - Crawled files map by path (see [`classify_path`]).
/// - MCP/CLI writes default to `fact` unless the caller supplied a type.
pub fn classify(source: Source, source_path: Option<&str>) -> MemoryType {
    if let Some(path) = source_path
        && let Some(t) = classify_path(path)
    {
        return t;
    }
    match source {
        Source::Crawler => MemoryType::Reference,
        Source::Mcp | Source::Cli => MemoryType::Fact,
    }
}

/// Instruction files that agents read verbatim, matched by file name.
const INSTRUCTION_FILES: &[&str] = &[
    "claude.md",       // Claude Code
    "agents.md",       // Codex, Cursor, Zed, Copilot, Jules, …
    "agent.md",        //
    "gemini.md",       // Gemini CLI
    ".cursorrules",    // Cursor (legacy)
    ".windsurfrules",  // Windsurf (legacy)
    ".clinerules",     // Cline (single-file form)
    ".rules",          // Zed
    "global_rules.md", // Windsurf global rules (~/.codeium/windsurf/memories/)
];

/// Directories whose Markdown children are instruction files, matched as the
/// parent directory's trailing path (e.g. `.cursor/rules/foo.mdc`).
const INSTRUCTION_DIRS: &[&str] = &[
    ".cursor/rules",        // Cursor project rules (*.mdc)
    ".windsurf/rules",      // Windsurf project rules
    ".clinerules",          // Cline directory form
    ".github/instructions", // Copilot `*.instructions.md`
    "Cline/Rules",          // Cline global rules (~/Documents/Cline/Rules)
];

/// Directories whose Markdown children are an agent's own memory.
const MEMORY_DIRS: &[&str] = &[
    "/.claude/projects/",           // Claude Code auto-memory (…/<slug>/memory/*.md)
    "/.codex/memories/",            // Codex built-in memories
    "/.codeium/windsurf/memories/", // Windsurf memories
    "/memory-bank/",                // Cline memory bank convention
];

/// Classify purely from a file path (used by the crawler).
pub fn classify_path(path: &str) -> Option<MemoryType> {
    let p = Path::new(path);
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let lower = name.to_lowercase();
    let ext = Path::new(&lower)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let is_text = matches!(ext, "md" | "markdown" | "mdx" | "mdc" | "txt");

    // Agent instruction files, by name.
    if INSTRUCTION_FILES.contains(&lower.as_str())
        || path.ends_with(".github/copilot-instructions.md")
    {
        return Some(MemoryType::AgentInstruction);
    }
    // …or by parent directory.
    if is_text && let Some(parent) = p.parent() {
        let parent = parent.to_string_lossy();
        if INSTRUCTION_DIRS.iter().any(|d| parent.ends_with(d)) {
            return Some(MemoryType::AgentInstruction);
        }
    }

    // Project overviews.
    if lower.starts_with("readme") {
        return Some(MemoryType::ProjectOverview);
    }

    // Agent memory files: `MEMORY.md` anywhere, or Markdown inside a known
    // memory directory. Claude Code's memory lives under
    // `~/.claude/projects/<slug>/memory/`; the `/memory/` component is required
    // so the slug directory's other files are left alone.
    if lower == "memory.md" {
        return Some(MemoryType::Fact);
    }
    if is_text {
        let in_claude_memory = path.contains("/.claude/projects/") && path.contains("/memory/");
        let in_other_memory = MEMORY_DIRS[1..].iter().any(|d| path.contains(d));
        if in_claude_memory || in_other_memory {
            return Some(MemoryType::Fact);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_agent_files_for_every_agent() {
        for p in [
            "/x/CLAUDE.md",
            "/x/AGENTS.md",
            "/x/GEMINI.md",
            "/x/.cursorrules",
            "/x/.windsurfrules",
            "/x/.clinerules",
            "/x/.rules",
            "/x/.github/copilot-instructions.md",
            "/x/.github/instructions/rust.instructions.md",
            "/x/.cursor/rules/style.mdc",
            "/x/.windsurf/rules/style.md",
            "/x/.clinerules/01-core.md",
            "/Users/q/Documents/Cline/Rules/global.md",
            "/Users/q/.codeium/windsurf/memories/global_rules.md",
        ] {
            assert_eq!(classify_path(p), Some(MemoryType::AgentInstruction), "{p}");
        }
    }

    #[test]
    fn classifies_readme() {
        assert_eq!(
            classify_path("/x/README.md"),
            Some(MemoryType::ProjectOverview)
        );
        assert_eq!(
            classify_path("/x/readme.txt"),
            Some(MemoryType::ProjectOverview)
        );
    }

    #[test]
    fn classifies_memory_files_for_every_agent() {
        for p in [
            "/x/MEMORY.md",
            "/Users/q/.claude/projects/-Users-q-Projects-foo/memory/foo.md",
            "/Users/q/.codex/memories/MEMORY.md",
            "/Users/q/.codex/memories/rollout_summaries/2026-10-01.md",
            "/Users/q/.codeium/windsurf/memories/project.md",
            "/x/memory-bank/activeContext.md",
        ] {
            assert_eq!(classify_path(p), Some(MemoryType::Fact), "{p}");
        }
    }

    #[test]
    fn ignores_ordinary_files_under_claude_dirs() {
        // The old `/.claude/` catch-all indexed every doc inside a Claude Code
        // worktree as a "fact". These must all be ignored now.
        for p in [
            "/x/.claude/worktrees/wt/docs/api/overview.mdx",
            "/x/.claude/worktrees/wt/PRD.md",
            "/x/.claude/settings.json",
            "/x/.claude/commands/review.md",
            "/Users/q/.claude/projects/-slug/notes.md",
            "/x/docs/guide.md",
            "/x/src/memory/mod.rs",
        ] {
            assert_eq!(classify_path(p), None, "{p}");
        }
    }

    #[test]
    fn mcp_writes_default_to_fact() {
        assert_eq!(classify(Source::Mcp, None), MemoryType::Fact);
        assert_eq!(classify(Source::Cli, None), MemoryType::Fact);
    }
}
