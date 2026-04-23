//! Per-project structural metrics (LOC, members, cyclomatic) derived from the
//! tree-sitter source scan. Emitted as `metrics.yaml` alongside `atlas.yaml`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::model::{Project, TypeKind, TypeMetrics};

const GLOBAL_NS: &str = "<global>";

#[derive(Debug, Serialize)]
pub struct MetricsSnapshot {
    pub projects: Vec<ProjectMetrics>,
}

#[derive(Debug, Serialize)]
pub struct ProjectMetrics {
    pub name: String,
    pub path: PathBuf,
    pub totals: ProjectTotals,
    /// Namespace -> kind -> local-name -> metrics.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub namespaces: BTreeMap<String, BTreeMap<TypeKind, BTreeMap<String, TypeMetrics>>>,
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct ProjectTotals {
    pub types: u32,
    pub loc: u32,
    pub members: u32,
    pub complexity: u32,
}

pub fn build(projects: &[Project], scan_root: &Path) -> MetricsSnapshot {
    let root = scan_root
        .canonicalize()
        .unwrap_or_else(|_| scan_root.to_path_buf());
    let mut out: Vec<ProjectMetrics> =
        projects.iter().map(|p| per_project(p, &root)).collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    MetricsSnapshot { projects: out }
}

/// Aggregate `TypeMetrics` across a project — useful for Atlas "weight".
pub fn project_totals(p: &Project) -> ProjectTotals {
    let mut t = ProjectTotals::default();
    for m in p.type_metrics.values() {
        t.types = t.types.saturating_add(1);
        t.loc = t.loc.saturating_add(m.loc);
        t.members = t.members.saturating_add(m.members);
        t.complexity = t.complexity.saturating_add(m.complexity);
    }
    t
}

fn per_project(p: &Project, root: &Path) -> ProjectMetrics {
    // Same longest-prefix namespace split used by `classes.rs`.
    let mut namespaces_by_len: Vec<&String> = p.declared_namespaces.iter().collect();
    namespaces_by_len.sort_by_key(|n| std::cmp::Reverse(n.len()));

    // Index FQN -> kind for quick lookup when we iterate metrics.
    let mut kind_of: BTreeMap<&str, TypeKind> = BTreeMap::new();
    for (kind, names) in &p.declared_types {
        for n in names {
            kind_of.insert(n.as_str(), *kind);
        }
    }

    let mut grouped: BTreeMap<String, BTreeMap<TypeKind, BTreeMap<String, TypeMetrics>>> =
        BTreeMap::new();

    for (fqn, metrics) in &p.type_metrics {
        // A type without a registered kind means we have metrics but no
        // declaration entry — treat as class. Shouldn't happen in practice.
        let kind = kind_of.get(fqn.as_str()).copied().unwrap_or(TypeKind::Class);
        let (ns, local) = split_namespace(fqn, &namespaces_by_len);
        grouped
            .entry(ns)
            .or_default()
            .entry(kind)
            .or_default()
            .insert(local.to_string(), metrics.clone());
    }

    ProjectMetrics {
        name: p.name.clone(),
        path: relativize(&p.path, root),
        totals: project_totals(p),
        namespaces: grouped,
    }
}

fn relativize(path: &Path, root: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}

fn split_namespace<'a>(full: &'a str, namespaces_by_len: &[&String]) -> (String, &'a str) {
    for ns in namespaces_by_len {
        if let Some(rest) = full.strip_prefix(ns.as_str()) {
            if let Some(local) = rest.strip_prefix('.') {
                return ((*ns).clone(), local);
            }
        }
    }
    (GLOBAL_NS.to_string(), full)
}
