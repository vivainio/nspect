//! Emit a per-project snapshot of declared namespaces and types collected by
//! the tree-sitter source scan. Intended to live next to `atlas.yaml` as a
//! sibling artifact (`classes.yaml`).
//!
//! Types are grouped under their declaring namespace so the namespace prefix
//! doesn't repeat on every entry. Nested types keep their dotted local path
//! (e.g. `Outer.Inner`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::model::{Project, TypeKind};

/// Key used for types that are not declared inside any `namespace` block.
const GLOBAL_NS: &str = "<global>";

#[derive(Debug, Serialize)]
pub struct ClassesSnapshot {
    pub projects: Vec<ProjectClasses>,
}

#[derive(Debug, Serialize)]
pub struct ProjectClasses {
    pub name: String,
    pub path: PathBuf,
    /// Namespace -> kind -> local type names (namespace prefix stripped).
    /// Empty namespaces (no types declared) are omitted.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub namespaces: BTreeMap<String, BTreeMap<TypeKind, Vec<String>>>,
}

pub fn build(projects: &[Project], scan_root: &Path) -> ClassesSnapshot {
    let root = scan_root
        .canonicalize()
        .unwrap_or_else(|_| scan_root.to_path_buf());
    let mut out: Vec<ProjectClasses> = projects.iter().map(|p| regroup(p, &root)).collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    ClassesSnapshot { projects: out }
}

fn relativize(path: &Path, root: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}

fn regroup(p: &Project, root: &Path) -> ProjectClasses {
    // Longest-prefix-first so `Foo.Bar.Baz` wins over `Foo.Bar` for a type
    // declared in the deeper namespace.
    let mut namespaces_by_len: Vec<&String> = p.declared_namespaces.iter().collect();
    namespaces_by_len.sort_by_key(|n| std::cmp::Reverse(n.len()));

    let mut grouped: BTreeMap<String, BTreeMap<TypeKind, Vec<String>>> = BTreeMap::new();

    // Seed every declared namespace so empty-but-declared namespaces still
    // appear in the output. They'll be pruned below if truly empty.
    for ns in &p.declared_namespaces {
        grouped.entry(ns.clone()).or_default();
    }

    for (kind, names) in &p.declared_types {
        for full in names {
            let (ns, local) = split_namespace(full, &namespaces_by_len);
            grouped
                .entry(ns)
                .or_default()
                .entry(*kind)
                .or_default()
                .push(local.to_string());
        }
    }

    // Drop namespaces that held no types, and ensure per-kind lists are sorted.
    grouped.retain(|_, kinds| !kinds.is_empty());
    for kinds in grouped.values_mut() {
        for names in kinds.values_mut() {
            names.sort();
            names.dedup();
        }
    }

    ProjectClasses {
        name: p.name.clone(),
        path: relativize(&p.path, root),
        namespaces: grouped,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Project, ProjectId};
    use std::path::PathBuf;

    fn mkproj(name: &str, nss: &[&str], types: &[(TypeKind, &str)]) -> Project {
        let mut declared_types: BTreeMap<TypeKind, Vec<String>> = BTreeMap::new();
        for (k, n) in types {
            declared_types.entry(*k).or_default().push((*n).to_string());
        }
        Project {
            id: ProjectId::from_path(std::path::Path::new(name)),
            path: PathBuf::from(format!("/r/{name}.csproj")),
            name: name.to_string(),
            sdk_style: true,
            target_frameworks: vec![],
            package_refs: vec![],
            project_refs: vec![],
            assembly_refs: vec![],
            usings: vec![],
            declared_namespaces: nss.iter().map(|s| s.to_string()).collect(),
            declared_types,
            type_metrics: BTreeMap::new(),
            referenced_types: Vec::new(),
            source_files: Vec::new(),
        }
    }

    #[test]
    fn groups_by_namespace_and_strips_prefix() {
        let p = mkproj(
            "Demo",
            &["Foo", "Foo.Bar"],
            &[
                (TypeKind::Class, "Foo.A"),
                (TypeKind::Class, "Foo.Bar.B"),
                (TypeKind::Class, "Foo.Bar.Outer.Inner"),
                (TypeKind::Interface, "Foo.Bar.IX"),
            ],
        );
        let out = regroup(&p, std::path::Path::new("/r"));
        let foo = out.namespaces.get("Foo").unwrap();
        assert_eq!(foo.get(&TypeKind::Class).unwrap(), &vec!["A".to_string()]);
        let foo_bar = out.namespaces.get("Foo.Bar").unwrap();
        assert_eq!(
            foo_bar.get(&TypeKind::Class).unwrap(),
            &vec!["B".to_string(), "Outer.Inner".to_string()]
        );
        assert_eq!(
            foo_bar.get(&TypeKind::Interface).unwrap(),
            &vec!["IX".to_string()]
        );
    }

    #[test]
    fn global_namespace_fallback() {
        let p = mkproj("Demo", &[], &[(TypeKind::Class, "NoNamespaceType")]);
        let out = regroup(&p, std::path::Path::new("/r"));
        let g = out.namespaces.get(GLOBAL_NS).unwrap();
        assert_eq!(
            g.get(&TypeKind::Class).unwrap(),
            &vec!["NoNamespaceType".to_string()]
        );
    }

    #[test]
    fn prefers_longest_matching_namespace() {
        // Both Foo and Foo.Bar are declared; Foo.Bar.X must land under Foo.Bar.
        let p = mkproj(
            "Demo",
            &["Foo", "Foo.Bar"],
            &[(TypeKind::Class, "Foo.Bar.X")],
        );
        let out = regroup(&p, std::path::Path::new("/r"));
        assert!(out.namespaces.get("Foo.Bar").is_some());
        assert!(out.namespaces.get("Foo").is_none()); // pruned, had no types
    }
}
