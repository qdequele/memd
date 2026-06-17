//! memd's Claude Code skills: invokable playbooks shipped inside the binary and
//! written into `~/.claude/skills/`. `memd-doctor` diagnoses/repairs a broken
//! setup; `memd-memory` teaches an agent to recall and save well. Idempotent;
//! memd owns these two directories by name and never touches others.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// (skill directory name, `SKILL.md` contents) for every memd-managed skill.
/// Bundled at compile time so the prebuilt binary needs no extra files.
const SKILLS: &[(&str, &str)] = &[
    (
        "memd-doctor",
        include_str!("../../assets/skills/memd-doctor/SKILL.md"),
    ),
    (
        "memd-memory",
        include_str!("../../assets/skills/memd-memory/SKILL.md"),
    ),
];

/// `~/.claude/skills`.
fn skills_root() -> Result<PathBuf> {
    Ok(directories::BaseDirs::new()
        .context("home directory")?
        .home_dir()
        .join(".claude")
        .join("skills"))
}

/// Write memd's skills into `~/.claude/skills/`. Idempotent; returns whether
/// anything changed.
pub fn install_claude_skills() -> Result<bool> {
    write_skills_to(&skills_root()?)
}

/// Remove memd's skills from `~/.claude/skills/`. Returns whether anything changed.
pub fn remove_claude_skills() -> Result<bool> {
    remove_skills_from(&skills_root()?)
}

/// Core of [`install_claude_skills`], parameterized on the skills root for tests.
fn write_skills_to(root: &Path) -> Result<bool> {
    let mut changed = false;
    for (name, body) in SKILLS {
        let dir = root.join(name);
        let file = dir.join("SKILL.md");
        // Skip when already current so re-running setup is a clean no-op.
        if std::fs::read_to_string(&file).ok().as_deref() == Some(*body) {
            continue;
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(&file, body).with_context(|| format!("writing {}", file.display()))?;
        changed = true;
    }
    Ok(changed)
}

/// Core of [`remove_claude_skills`], parameterized on the skills root for tests.
fn remove_skills_from(root: &Path) -> Result<bool> {
    let mut changed = false;
    for (name, _) in SKILLS {
        let dir = root.join(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
            changed = true;
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_then_remove_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("skills");

        assert!(
            write_skills_to(&root).unwrap(),
            "first write changes things"
        );
        for (name, _) in SKILLS {
            assert!(
                root.join(name).join("SKILL.md").is_file(),
                "{name} should be written"
            );
        }

        // Idempotent: a second write with identical content reports no change.
        assert!(!write_skills_to(&root).unwrap());

        assert!(remove_skills_from(&root).unwrap());
        for (name, _) in SKILLS {
            assert!(!root.join(name).exists(), "{name} should be gone");
        }
        // Removing again is a no-op.
        assert!(!remove_skills_from(&root).unwrap());
    }

    #[test]
    fn bundled_skills_have_frontmatter() {
        for (name, body) in SKILLS {
            assert!(
                body.starts_with("---\n"),
                "{name} SKILL.md must start with YAML frontmatter"
            );
            assert!(
                body.contains(&format!("name: {name}")),
                "{name} frontmatter name must match its directory"
            );
        }
    }
}
