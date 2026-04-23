//! Resolve each project's referenced simple-name type tokens against a
//! catalog of declared types across all projects. Emitted as `references.yaml`
//! alongside `atlas.yaml` / `classes.yaml` / `metrics.yaml`.
//!
//! For every project `P`, each simple name `N` collected during the source
//! scan is classified as:
//!
//! * `resolved_cross_project` — `N` is declared by exactly one other project;
//!   grouped by declaring project: `{other_project: [N, ...]}`.
//! * `ambiguous` — `N` is declared by two or more other projects; emitted
//!   as `{N: [proj_a, proj_b, ...]}`.
//! * `external` — `N` is not declared by any project in the load.
//!
//! Names that resolve to `P` itself are dropped — they carry no cross-project
//! signal. Common predefined C# names (`string`, `int`, …) are filtered by
//! the scan pass and never reach this layer.
//!
//! The catalog is built from `Project.declared_types`, and the declaring
//! namespace is taken from the type's FQN. A reference is only attributed to
//! a project if the referring project's `usings` or own declared namespaces
//! include that namespace; otherwise it falls through to `external`. This
//! filters simple-name collisions between unrelated projects.
//!
//! Per-class detail is not emitted — the scan captures references at the
//! project level only.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::model::Project;

#[derive(Debug, Serialize)]
pub struct ReferencesSnapshot {
    pub projects: Vec<ProjectReferences>,
}

#[derive(Debug, Serialize)]
pub struct ProjectReferences {
    pub name: String,
    pub path: PathBuf,
    /// Declaring project → simple names this project uses from it.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub resolved_cross_project: BTreeMap<String, Vec<String>>,
    /// Simple-name → list of projects that declare it.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub ambiguous: BTreeMap<String, Vec<String>>,
    /// Simple names that no project in the load declares.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub external: Vec<String>,
}

/// Catalog entry: which project declares a given simple type name, and under
/// which declaring namespace.
#[derive(Debug, Clone)]
struct CatalogEntry {
    project: String,
    namespace: String,
}

pub fn build(projects: &[Project], scan_root: &Path) -> ReferencesSnapshot {
    let root = scan_root
        .canonicalize()
        .unwrap_or_else(|_| scan_root.to_path_buf());

    let catalog = build_catalog(projects);

    let mut out: Vec<ProjectReferences> =
        projects.iter().map(|p| resolve(p, &catalog, &root)).collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    ReferencesSnapshot { projects: out }
}

fn build_catalog(projects: &[Project]) -> BTreeMap<String, Vec<CatalogEntry>> {
    let mut out: BTreeMap<String, Vec<CatalogEntry>> = BTreeMap::new();
    for p in projects {
        for names in p.declared_types.values() {
            for fqn in names {
                let (ns, simple) = split_fqn(fqn);
                out.entry(simple.to_string()).or_default().push(CatalogEntry {
                    project: p.name.clone(),
                    namespace: ns.to_string(),
                });
            }
        }
    }
    out
}

fn split_fqn(fqn: &str) -> (&str, &str) {
    match fqn.rsplit_once('.') {
        Some((ns, name)) => (ns, name),
        None => ("", fqn),
    }
}

fn resolve(
    p: &Project,
    catalog: &BTreeMap<String, Vec<CatalogEntry>>,
    root: &Path,
) -> ProjectReferences {
    let own_namespaces: BTreeSet<&str> =
        p.declared_namespaces.iter().map(String::as_str).collect();
    let usings: BTreeSet<&str> = p.usings.iter().map(String::as_str).collect();

    // A referenced namespace is "visible" if the project imports it via
    // `using` or declares types in it itself.
    let visible = |ns: &str| -> bool {
        if ns.is_empty() {
            return true;
        }
        own_namespaces.contains(ns) || usings.contains(ns) || {
            // Also treat a parent namespace `X` as visible if the project
            // imports a nested namespace `X.Y` — cheap heuristic; avoids
            // dropping references in large solutions.
            usings.iter().any(|u| u.starts_with(&format!("{ns}.")))
        }
    };

    let mut resolved_cross_project: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut ambiguous: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut external: Vec<String> = Vec::new();

    for name in &p.referenced_types {
        match catalog.get(name) {
            None => external.push(name.clone()),
            Some(entries) => {
                // Keep only entries whose declaring namespace is visible to `p`.
                let visible_entries: Vec<&CatalogEntry> = entries
                    .iter()
                    .filter(|e| visible(&e.namespace))
                    .collect();
                // Drop internal hits (same project). If any remaining hit is
                // internal, the reference is "internal" — skip per caller
                // request (we don't emit internal references).
                let self_hit = visible_entries.iter().any(|e| e.project == p.name);
                if self_hit {
                    continue;
                }
                let distinct_projects: BTreeSet<&str> =
                    visible_entries.iter().map(|e| e.project.as_str()).collect();
                match distinct_projects.len() {
                    0 => external.push(name.clone()),
                    1 => {
                        let proj = distinct_projects.into_iter().next().unwrap().to_string();
                        resolved_cross_project
                            .entry(proj)
                            .or_default()
                            .push(name.clone());
                    }
                    _ => {
                        let mut list: Vec<String> =
                            distinct_projects.into_iter().map(String::from).collect();
                        list.sort();
                        ambiguous.insert(name.clone(), list);
                    }
                }
            }
        }
    }

    external.sort();
    external.dedup();
    for names in resolved_cross_project.values_mut() {
        names.sort();
        names.dedup();
    }

    ProjectReferences {
        name: p.name.clone(),
        path: relativize(&p.path, root),
        resolved_cross_project,
        ambiguous,
        external,
    }
}

fn relativize(path: &Path, root: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Project, ProjectId, TypeKind};
    use std::collections::BTreeMap;

    fn mkproj(
        name: &str,
        nss: &[&str],
        types: &[(TypeKind, &str)],
        usings: &[&str],
        refs: &[&str],
    ) -> Project {
        let mut declared_types: BTreeMap<TypeKind, Vec<String>> = BTreeMap::new();
        for (k, n) in types {
            declared_types.entry(*k).or_default().push((*n).to_string());
        }
        Project {
            id: ProjectId::from_path(Path::new(name)),
            path: PathBuf::from(format!("/r/{name}.csproj")),
            name: name.to_string(),
            sdk_style: true,
            target_frameworks: vec![],
            package_refs: vec![],
            project_refs: vec![],
            assembly_refs: vec![],
            usings: usings.iter().map(|s| (*s).to_string()).collect(),
            declared_namespaces: nss.iter().map(|s| (*s).to_string()).collect(),
            declared_types,
            type_metrics: BTreeMap::new(),
            referenced_types: refs.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn classifies_cross_ambiguous_and_external() {
        let a = mkproj(
            "Domain",
            &["Acme.Domain"],
            &[(TypeKind::Class, "Acme.Domain.Customer")],
            &[],
            &[],
        );
        let b = mkproj(
            "Shared",
            &["Acme.Shared"],
            &[(TypeKind::Class, "Acme.Shared.Customer")],
            &[],
            &[],
        );
        let c = mkproj(
            "Utils",
            &["Acme.Utils"],
            &[(TypeKind::Class, "Acme.Utils.Helper")],
            &[],
            &[],
        );
        let web = mkproj(
            "Web",
            &["Acme.Web"],
            &[],
            &["Acme.Domain", "Acme.Shared", "Acme.Utils"],
            &["Customer", "Helper", "Logger"],
        );
        let snap = build(&[a, b, c, web], Path::new("/r"));
        let web_r = snap.projects.iter().find(|p| p.name == "Web").unwrap();
        assert_eq!(
            web_r.resolved_cross_project.get("Utils").cloned(),
            Some(vec!["Helper".to_string()])
        );
        // Customer is declared by both Domain and Shared → ambiguous.
        let amb = web_r.ambiguous.get("Customer").cloned().unwrap();
        assert_eq!(amb, vec!["Domain".to_string(), "Shared".to_string()]);
        assert!(web_r.external.contains(&"Logger".to_string()));
    }

    #[test]
    fn drops_internal_references() {
        let lib = mkproj(
            "Lib",
            &["Acme.Lib"],
            &[(TypeKind::Class, "Acme.Lib.Thing")],
            &[],
            &["Thing", "Logger"],
        );
        let snap = build(&[lib], Path::new("/r"));
        let r = &snap.projects[0];
        assert!(r.resolved_cross_project.is_empty());
        assert!(r.ambiguous.is_empty());
        // Internal self-hit dropped; Logger still external.
        assert_eq!(r.external, vec!["Logger".to_string()]);
    }

    #[test]
    fn gates_cross_project_by_using_visibility() {
        let dom = mkproj(
            "Domain",
            &["Acme.Domain"],
            &[(TypeKind::Class, "Acme.Domain.Customer")],
            &[],
            &[],
        );
        // No using for Acme.Domain — reference should fall through to external.
        let web = mkproj("Web", &["Acme.Web"], &[], &[], &["Customer"]);
        let snap = build(&[dom, web], Path::new("/r"));
        let web_r = snap.projects.iter().find(|p| p.name == "Web").unwrap();
        assert!(web_r.resolved_cross_project.is_empty());
        assert_eq!(web_r.external, vec!["Customer".to_string()]);
    }
}
