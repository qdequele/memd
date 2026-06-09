//! Deterministic, heuristics-first classification (PRD §8).
//!
//! No model in the loop: type is inferred from source, path, and filename.

use super::model::{MemoryType, Source};
use std::path::Path;

/// Infer a memory type from its origin.
///
/// - Crawled files map by filename (`CLAUDE.md`/`AGENTS.md` → agent_instruction,
///   `README*` → project_overview, known memory files → fact).
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

/// Classify purely from a file path's name (used by the crawler).
pub fn classify_path(path: &str) -> Option<MemoryType> {
    let name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let lower = name.to_lowercase();

    // Agent instruction files.
    if matches!(
        lower.as_str(),
        "claude.md" | "agents.md" | "agent.md" | ".cursorrules" | "gemini.md"
    ) || path.ends_with(".github/copilot-instructions.md")
    {
        return Some(MemoryType::AgentInstruction);
    }

    // Project overviews.
    if lower.starts_with("readme") {
        return Some(MemoryType::ProjectOverview);
    }

    // Known memory files: MEMORY.md anywhere, or text/markdown files living in a
    // dedicated `memory/` or `.claude/` directory. Restrict to text extensions
    // so we don't slurp every binary/asset in those dirs.
    let is_text = matches!(
        Path::new(&lower).extension().and_then(|e| e.to_str()),
        Some("md") | Some("markdown") | Some("mdx") | Some("txt")
    );
    if lower == "memory.md"
        || (is_text && (path.contains("/memory/") || path.contains("/.claude/")))
    {
        return Some(MemoryType::Fact);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_agent_files() {
        assert_eq!(
            classify_path("/x/CLAUDE.md"),
            Some(MemoryType::AgentInstruction)
        );
        assert_eq!(
            classify_path("/x/AGENTS.md"),
            Some(MemoryType::AgentInstruction)
        );
        assert_eq!(
            classify_path("/x/.github/copilot-instructions.md"),
            Some(MemoryType::AgentInstruction)
        );
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
    fn classifies_memory_files() {
        assert_eq!(classify_path("/x/MEMORY.md"), Some(MemoryType::Fact));
        assert_eq!(
            classify_path("/Users/q/.claude/projects/p/memory/foo.md"),
            Some(MemoryType::Fact)
        );
    }

    #[test]
    fn mcp_writes_default_to_fact() {
        assert_eq!(classify(Source::Mcp, None), MemoryType::Fact);
        assert_eq!(classify(Source::Cli, None), MemoryType::Fact);
    }
}
