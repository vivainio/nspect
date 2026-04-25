//! Hand-authored structural ground truth loaded from `.nspect/spec/`.
//!
//! Today this covers area assignment overrides; more dimensions (layers, tags,
//! rules) can land as sibling files without changing the loader shape.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use globset::{Glob, GlobMatcher};
use serde::Deserialize;

/// Parsed `.nspect/spec/areas.yaml`.
///
/// Shape:
/// ```yaml
/// areas:
///   Billing:
///     - src/Legacy/OddOne
///     - src/Shared/Pipeline.csproj
///   Platform:
///     - src/Common
/// ```
///
/// Keys are area names (new or overriding an inferred one). Values are paths
/// relative to the repo root — either a directory (claims every csproj under
/// it) or a single `.csproj` file.
#[derive(Debug, Default, Deserialize)]
pub struct AreasSpec {
    #[serde(default)]
    pub areas: BTreeMap<String, Vec<String>>,
}

impl AreasSpec {
    /// Load `<repo_root>/.nspect/spec/areas.yaml`. Returns an empty spec if the
    /// file is absent — overrides are opt-in.
    pub fn load(repo_root: &Path) -> Result<Self> {
        let path = repo_root.join(".nspect").join("spec").join("areas.yaml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let parsed: AreasSpec =
            serde_yaml::from_str(&body).with_context(|| format!("parsing {}", path.display()))?;
        Ok(parsed)
    }

    /// Resolve the spec into a function that maps a csproj path to an area
    /// override (or None to keep the inferred area). `project_paths` is the
    /// full set of csprojs in the load, used to validate entries.
    ///
    /// Conflict rule: if a csproj matches entries under two areas, the
    /// longest-path entry wins. Ties are reported as warnings and the first
    /// (area-alphabetical) wins.
    ///
    /// Zero-match entries are emitted as warnings.
    pub fn resolve(
        &self,
        repo_root: &Path,
        project_paths: &[PathBuf],
    ) -> (BTreeMap<PathBuf, String>, Vec<String>) {
        let mut warnings: Vec<String> = Vec::new();
        // (project_path, matched_entry_specificity, area_name)
        let mut best: BTreeMap<PathBuf, (usize, String)> = BTreeMap::new();

        let repo_root = repo_root
            .canonicalize()
            .unwrap_or_else(|_| repo_root.to_path_buf());

        for (area, entries) in &self.areas {
            for entry in entries {
                let (matcher, specificity) = match compile_entry(entry) {
                    Ok(m) => m,
                    Err(e) => {
                        warnings.push(format!(
                            "spec/areas.yaml: area `{area}` entry `{entry}`: {e}"
                        ));
                        continue;
                    }
                };
                let mut matched = 0usize;
                for pp in project_paths {
                    if matcher.matches(&repo_root, entry, pp) {
                        matched += 1;
                        let slot = best.entry(pp.clone()).or_insert((0, area.clone()));
                        if specificity > slot.0 {
                            *slot = (specificity, area.clone());
                        } else if specificity == slot.0 && slot.1 != *area {
                            warnings.push(format!(
                                "spec/areas.yaml: csproj {} is claimed by both `{}` and `{}` at equal specificity; picking `{}`",
                                pp.display(), slot.1, area, slot.1
                            ));
                        }
                    }
                }
                if matched == 0 {
                    warnings.push(format!(
                        "spec/areas.yaml: `{entry}` under area `{area}` matched no csproj"
                    ));
                }
            }
        }

        let out: BTreeMap<PathBuf, String> = best.into_iter().map(|(k, (_, v))| (k, v)).collect();
        (out, warnings)
    }
}

enum EntryMatcher {
    /// Entry is a plain path — matches if it equals the csproj or is an
    /// ancestor directory of it.
    Plain,
    /// Entry is a glob — matches against the csproj path relative to the
    /// repo root. A glob that matches the project's directory (rather than
    /// the .csproj filename) also matches everything under that directory.
    Glob(GlobMatcher),
}

impl EntryMatcher {
    fn matches(&self, repo_root: &Path, entry: &str, project_abs: &Path) -> bool {
        match self {
            EntryMatcher::Plain => {
                let abs = repo_root.join(entry);
                project_abs == abs || project_abs.starts_with(&abs)
            }
            EntryMatcher::Glob(m) => {
                let Ok(rel) = project_abs.strip_prefix(repo_root) else {
                    return false;
                };
                // Direct match against the full csproj relative path.
                if m.is_match(rel) {
                    return true;
                }
                // Directory-style glob: match any ancestor of the csproj so
                // `test/Dh*` claims `test/DhApi/DhApi.Tests.csproj`.
                let mut anc = rel.parent();
                while let Some(p) = anc {
                    if p.as_os_str().is_empty() {
                        break;
                    }
                    if m.is_match(p) {
                        return true;
                    }
                    anc = p.parent();
                }
                false
            }
        }
    }
}

/// Compile a spec entry into a matcher. Plain paths stay literal; anything
/// containing glob metacharacters (`*`, `?`, `[`, `{`) is parsed as a glob.
///
/// Returns (matcher, specificity). Specificity = the length of the entry's
/// literal prefix up to the first glob metacharacter; used to break ties
/// when multiple entries claim the same csproj.
fn compile_entry(entry: &str) -> Result<(EntryMatcher, usize), String> {
    let meta_pos = entry.find(|c: char| matches!(c, '*' | '?' | '[' | '{'));
    match meta_pos {
        None => Ok((EntryMatcher::Plain, entry.len())),
        Some(pos) => {
            let glob = Glob::new(entry).map_err(|e| format!("invalid glob: {e}"))?;
            Ok((EntryMatcher::Glob(glob.compile_matcher()), pos))
        }
    }
}

/// Parsed `.nspect/spec/rules.yaml`. Declares allowed/forbidden area-to-area
/// dependencies, enforced by `nspect atlas --check`.
///
/// Shape:
/// ```yaml
/// rules:
///   - area: Shared
///     allow: []              # nothing internal allowed (fully isolated)
///   - area: Billing
///     allow: [Shared, Ingest] # Billing may only reach these areas (plus self)
///   - area: Inventory
///     deny: [Billing]         # targeted prohibition
/// ```
///
/// Semantics when evaluated per edge `A → B`:
/// - Same-area edges (`A == B`) are always allowed.
/// - If `allow` is set for A's area, B's area must be in `allow` (or equal A).
///   Anything else is a violation.
/// - If `deny` is set for A's area, B's area must not appear in `deny`.
/// - Both keys may be set; both apply independently.
/// - Areas with no rule entry have no constraint.
#[derive(Debug, Default, Deserialize)]
pub struct RulesSpec {
    #[serde(default)]
    pub rules: Vec<AreaRule>,
}

#[derive(Debug, Deserialize)]
pub struct AreaRule {
    pub area: String,
    #[serde(default)]
    pub allow: Option<Vec<String>>,
    #[serde(default)]
    pub deny: Option<Vec<String>>,
}

impl RulesSpec {
    /// Load `<repo_root>/.nspect/spec/rules.yaml`. Returns an empty spec if the
    /// file is absent.
    pub fn load(repo_root: &Path) -> Result<Self> {
        let path = repo_root.join(".nspect").join("spec").join("rules.yaml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let parsed: RulesSpec =
            serde_yaml::from_str(&body).with_context(|| format!("parsing {}", path.display()))?;
        Ok(parsed)
    }

    /// Look up the rule governing outgoing edges from `area`, if any.
    pub fn for_area(&self, area: &str) -> Option<&AreaRule> {
        self.rules.iter().find(|r| r.area == area)
    }

    /// Emit warnings for `allow`/`deny` names that don't match any known area.
    /// Stale rules otherwise sit silent and produce no findings, which is the
    /// opposite of what the user wants.
    pub fn validate(&self, known_areas: &std::collections::BTreeSet<String>) -> Vec<String> {
        let mut out = Vec::new();
        for r in &self.rules {
            if !known_areas.contains(&r.area) {
                out.push(format!(
                    "spec/rules.yaml: rule references unknown area `{}`",
                    r.area
                ));
            }
            for list_name in ["allow", "deny"] {
                let list = match list_name {
                    "allow" => r.allow.as_deref(),
                    _ => r.deny.as_deref(),
                };
                let Some(list) = list else { continue };
                for a in list {
                    if !known_areas.contains(a) {
                        out.push(format!(
                            "spec/rules.yaml: rule for `{}` lists unknown area `{}` under `{}`",
                            r.area, a, list_name
                        ));
                    }
                }
            }
        }
        out
    }
}

/// Write a commented `.nspect/spec/areas.yaml` stub if none exists. Called by
/// `nspect init`.
pub fn seed_areas_stub(repo_root: &Path) -> Result<()> {
    let dir = repo_root.join(".nspect").join("spec");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("areas.yaml");
    if path.exists() {
        return Ok(());
    }
    let stub = "\
# areas.yaml — hand-authored area assignments.
#
# Keys are area names; values are lists of paths (relative to repo root). A
# path may point at a single .csproj or at a directory (claims every csproj
# under it). Matching csprojs are reassigned to this area, overriding the
# inferred area (first path segment after src/).
#
# Unlisted csprojs keep their inferred area. `nspect init` / `nspect atlas`
# warn on entries that match no csproj so stale paths surface quickly.
#
# Example:
# areas:
#   Billing:
#     - src/Legacy/OddOne
#     - src/Shared/Pipeline.csproj
#   Platform:
#     - src/Common

areas: {}
";
    std::fs::write(&path, stub).with_context(|| format!("writing {}", path.display()))?;
    eprintln!("seeded {}", path.display());
    Ok(())
}

/// Write a commented `.nspect/spec/rules.yaml` stub if none exists. Called by
/// `nspect init`.
pub fn seed_rules_stub(repo_root: &Path) -> Result<()> {
    let dir = repo_root.join(".nspect").join("spec");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("rules.yaml");
    if path.exists() {
        return Ok(());
    }
    let stub = "\
# rules.yaml — area-to-area dependency guardrails, checked by
# `nspect atlas --check` (emits `forbidden_area_edge` findings).
#
# Per area you may set:
#   allow: [Area1, Area2, ...]   outgoing edges may only land in these areas
#                                (same-area edges are always allowed).
#   deny:  [AreaX, ...]          additional targeted prohibitions.
#
# Areas with no rule entry are unconstrained. Stale names (referring to
# areas that don't exist in areas.yaml) produce warnings.
#
# Example:
# rules:
#   - area: Shared
#     allow: []               # Shared must not depend on anything internal
#   - area: Billing
#     allow: [Shared, Ingest]
#   - area: Inventory
#     deny: [Billing]

rules: []
";
    std::fs::write(&path, stub).with_context(|| format!("writing {}", path.display()))?;
    eprintln!("seeded {}", path.display());
    Ok(())
}
