//! Hand-authored structural ground truth loaded from `.nspect/spec/`.
//!
//! Today this covers area assignment overrides; more dimensions (layers, tags,
//! rules) can land as sibling files without changing the loader shape.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
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
        // (project_path, matched_entry_len, area_name)
        let mut best: BTreeMap<PathBuf, (usize, String)> = BTreeMap::new();

        for (area, entries) in &self.areas {
            for entry in entries {
                let abs = repo_root.join(entry);
                let entry_len = entry.len();
                let mut matched = 0usize;
                for pp in project_paths {
                    if paths_match(&abs, pp) {
                        matched += 1;
                        let slot = best.entry(pp.clone()).or_insert((0, area.clone()));
                        if entry_len > slot.0 {
                            *slot = (entry_len, area.clone());
                        } else if entry_len == slot.0 && slot.1 != *area {
                            warnings.push(format!(
                                "spec/areas.yaml: csproj {} is claimed by both `{}` and `{}` at equal specificity; picking `{}`",
                                pp.display(), slot.1, area, slot.1
                            ));
                        }
                    }
                }
                if matched == 0 {
                    warnings.push(format!(
                        "spec/areas.yaml: `{}` under area `{}` matched no csproj",
                        entry, area
                    ));
                }
            }
        }

        let out: BTreeMap<PathBuf, String> = best.into_iter().map(|(k, (_, v))| (k, v)).collect();
        (out, warnings)
    }
}

/// A spec entry matches a csproj when the entry points at the csproj file
/// directly, or at an ancestor directory of it.
fn paths_match(entry_abs: &Path, project_abs: &Path) -> bool {
    if entry_abs == project_abs {
        return true;
    }
    // Directory claim: project_abs lives under entry_abs.
    project_abs.starts_with(entry_abs)
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
