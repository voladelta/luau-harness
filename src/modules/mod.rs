//! Module resolution, package manifests, the lockfile (approvals) and staging.
//!
//! `require("pkg")` loads `init.luau`; `require("pkg/mod")` loads `mod.luau`. Lookup order:
//!   1. `<ws>/.harness/override/std/<file>`   user override of a built-in file (e.g. the printer), no rebuild needed
//!   2. `<ws>/.harness/pkgs/<pkg>/<mod>.luau` project package (needs a lockfile entry for its exact content)
//!   3. the built-in `std` package embedded in the binary
//!
//! An override replaces a file of the built-in `std` package and nothing else, and no package may be named `std`, so
//! the three never compete for one name.
//!
//! Agent skills (the `.agents/skills` convention): `<ws>/.agents/skills/<name>/SKILL.md`, then `~/.agents/skills/<name>/SKILL.md`.
//! Their manifest comes from SKILL.md frontmatter; they carry no Luau and no capabilities (an `hx.toml` or `.luau` file
//! in one is an ordinary file), and are locked by `hx approve NAME` like any package. `.harness/pkgs/<name>` shadows them.
//!
//! Every package and skill is read by one function, `snapshot`: approval, `hx skills`, the run's registry and `hx
//! accept` all see the same files and the same digest.
//!
//! Everything under (1), (2), `project/init` and the agent skills is used only when the workspace config is trusted
//! (see trust.rs); otherwise only the built-in package resolves.
//!
//! `require("base:std/printer")` skips (1), so an override can wrap the module it replaces.
//! `require("project/init")` is `<ws>/.harness/init.luau`, run before the loop starts (hook registration etc.).
mod approval;
mod registry;
mod staging;
#[cfg(test)]
mod tests;

pub use registry::{test_files, Registry, Source, TestTarget};

use crate::supervisor::Capabilities;
use serde::Deserialize;
use std::path::PathBuf;

/// Packages built into the binary: (package, file, text). Trusted like the binary itself.
pub const EMBEDDED: &[(&str, &str, &str)] = &[
    ("std", "hx.toml", include_str!("../../stdlib/std/hx.toml")),
    ("std", "init.luau", include_str!("../../stdlib/std/init.luau")),
    ("std", "loop.luau", include_str!("../../stdlib/std/loop.luau")),
    ("std", "rpi.luau", include_str!("../../stdlib/std/rpi.luau")),
    ("std", "rpi/common.md", include_str!("../../stdlib/std/rpi/common.md")),
    ("std", "rpi/research_questions.md", include_str!("../../stdlib/std/rpi/research_questions.md")),
    ("std", "rpi/research.md", include_str!("../../stdlib/std/rpi/research.md")),
    ("std", "rpi/system_design.md", include_str!("../../stdlib/std/rpi/system_design.md")),
    ("std", "rpi/program_design.md", include_str!("../../stdlib/std/rpi/program_design.md")),
    ("std", "rpi/implementation.md", include_str!("../../stdlib/std/rpi/implementation.md")),
    ("std", "rpi/verification.md", include_str!("../../stdlib/std/rpi/verification.md")),
    ("std", "rpi/repair.md", include_str!("../../stdlib/std/rpi/repair.md")),
    ("std", "rpi/PROVENANCE.md", include_str!("../../stdlib/std/rpi/PROVENANCE.md")),
    ("std", "prompt.luau", include_str!("../../stdlib/std/prompt.luau")),
    ("std", "printer.luau", include_str!("../../stdlib/std/printer.luau")),
    ("std", "feedback.luau", include_str!("../../stdlib/std/feedback.luau")),
    ("std", "hooks.luau", include_str!("../../stdlib/std/hooks.luau")),
    ("std", "testlib.luau", include_str!("../../stdlib/std/testlib.luau")),
    ("std", "packages.luau", include_str!("../../stdlib/std/packages.luau")),
    ("std", "mcp.luau", include_str!("../../stdlib/std/mcp.luau")),
    ("std", "authoring.luau", include_str!("../../stdlib/std/authoring.luau")),
    ("std", "vision.luau", include_str!("../../stdlib/std/vision.luau")),
    ("std", "compact.luau", include_str!("../../stdlib/std/compact.luau")),
    ("std", "recheck.luau", include_str!("../../stdlib/std/recheck.luau")),
    ("std", "system.md", include_str!("../../prompts/system.md")),
];

/// A package's `hx.toml`. Unknown tables and unknown capabilities are load errors, so a typo cannot silently grant
/// nothing (or less than intended).
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub package: PackageInfo,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub tool: ToolInfo,
    /// `[exports]` and `[tests]`: written by older `promote()` manifests and read by nothing (the module is `init.luau`,
    /// tests are `tests/*.luau`). Accepted so approved packages keep loading.
    #[serde(default, rename = "exports")]
    _exports: Option<toml::Table>,
    #[serde(default, rename = "tests")]
    _tests: Option<toml::Table>,
}

impl Manifest {
    /// The manifest of an agent skill: kind "skill", no capabilities, the description from SKILL.md frontmatter.
    pub(super) fn agent_skill(skill_md: &str) -> Manifest {
        let mut m = Manifest::default();
        m.package.version = "agent-skill".into();
        m.package.kind = Some("skill".into());
        m.package.description = Some(frontmatter_description(skill_md));
        m
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct PackageInfo {
    /// The package's name; `hx test` uses it for a directory outside `.harness/pkgs` (else the directory name).
    pub name: Option<String>,
    pub version: String,
    /// "tool" (functions the model calls as tools.NAME), "skill" (SKILL.md + optional Luau), or empty (plain module)
    pub kind: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ToolInfo {
    pub signature: Option<String>,
    pub doc: Option<String>,
}

/// An installed, locked and granted package, as shown to the model.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PkgInfo {
    pub name: String,
    pub version: String,
    pub kind: String,
    pub description: String,
    pub signature: String,
    pub doc: String,
    /// skills: absolute directory and its files (relative), so SKILL.md's references can be read
    pub dir: String,
    pub files: Vec<String>,
}

/// A name the model can use as a Luau identifier (`deftool` names, and so promoted packages): lowercase letters,
/// digits and `_`, starting with a letter, at most 40 bytes.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 40 && n.chars().next().is_some_and(|c| c.is_ascii_lowercase()) && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A package or skill name: lowercase letters, digits, `_` and `-` (agent skills are conventionally hyphenated),
/// starting with a letter, at most 64 bytes, and not `std` or `project`, which name built-in modules. No `/`, `:` or
/// `.`, so a name can never reach another package's modules, the `base:`/`tool:` namespaces or a path outside its
/// directory.
pub fn valid_package_name(n: &str) -> bool {
    n.len() <= 64 && !matches!(n, "std" | "project") && n.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

pub struct Resolver {
    pub root: PathBuf,
}

/// The value of `description:` in SKILL.md frontmatter (plain, quoted, or a `>`/`|` block).
pub(super) fn frontmatter_description(text: &str) -> String {
    let Some(fm) = text.strip_prefix("---\n").and_then(|t| t.split_once("\n---").map(|(f, _)| f)) else { return String::new() };
    let mut lines = fm.lines();
    while let Some(l) = lines.next() {
        let Some(v) = l.strip_prefix("description:") else { continue };
        let v = v.trim();
        if v.is_empty() || v.starts_with('>') || v.starts_with('|') {
            let block: Vec<&str> = lines.take_while(|l| l.starts_with(' ') || l.starts_with('\t') || l.is_empty()).map(str::trim).collect();
            return block.join(" ").trim().to_string();
        }
        return v.trim_matches(|c| c == '"' || c == '\'').to_string();
    }
    String::new()
}

pub(super) fn file_for(module: &str) -> String {
    if module.is_empty() { "init.luau".into() } else { format!("{module}.luau") }
}

pub(super) fn split_name(name: &str) -> (&str, &str) {
    name.split_once('/').unwrap_or((name, ""))
}

impl Resolver {
    pub fn new(root: PathBuf) -> Resolver {
        Resolver { root }
    }

    pub(super) fn harness(&self) -> PathBuf {
        self.root.join(".harness")
    }

    pub fn embedded(pkg: &str, file: &str) -> Option<&'static str> {
        EMBEDDED.iter().find(|(p, n, _)| *p == pkg && *n == file).map(|(_, _, t)| *t)
    }
}
