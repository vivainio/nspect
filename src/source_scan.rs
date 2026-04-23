//! Tree-sitter powered scan of `.cs` source files.
//!
//! For v1 we extract only the namespaces from `using` directives. That's enough
//! to cross-check declared NuGet packages against what source actually imports.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use tree_sitter::{Parser, TreeCursor};

use crate::model::TypeKind;

/// Result of scanning one project's sources.
#[derive(Debug, Clone, Default)]
pub struct SourceScan {
    pub source_files: Vec<PathBuf>,
    /// Namespaces used via `using X.Y.Z;`. Deduped, sorted.
    pub usings: Vec<String>,
    /// Namespaces declared via `namespace X.Y { ... }` or file-scoped form.
    /// Deduped, sorted.
    pub declared_namespaces: Vec<String>,
    /// Fully-qualified type names declared in sources, bucketed by kind.
    /// Nested types are joined with `.`. Per-bucket lists are deduped and
    /// sorted; empty buckets are omitted.
    pub declared_types: BTreeMap<TypeKind, Vec<String>>,
}

/// Per-file extraction output.
#[derive(Debug, Default)]
pub struct FileDecls {
    pub usings: Vec<String>,
    pub namespaces: Vec<String>,
    pub types: Vec<(TypeKind, String)>,
}

/// Scan `.cs` files under each project. Files inside a nested project's
/// directory are attributed to that nested project only.
pub fn scan_projects(projects: &[crate::model::Project]) -> Result<Vec<SourceScan>> {
    let dirs: Vec<PathBuf> = projects
        .iter()
        .map(|p| p.path.parent().map(Path::to_path_buf).unwrap_or_default())
        .collect();

    let mut out = Vec::with_capacity(projects.len());
    for (i, project) in projects.iter().enumerate() {
        let project_dir = &dirs[i];
        let nested: Vec<&Path> = dirs
            .iter()
            .enumerate()
            .filter(|(j, d)| *j != i && d.starts_with(project_dir) && d != &project_dir)
            .map(|(_, d)| d.as_path())
            .collect();

        let mut scan = SourceScan::default();
        let mut usings = BTreeSet::new();
        let mut namespaces = BTreeSet::new();
        let mut types: BTreeMap<TypeKind, BTreeSet<String>> = BTreeMap::new();

        let walker = WalkBuilder::new(project_dir)
            .follow_links(false)
            .git_ignore(true)
            .hidden(false)
            .build();

        for entry in walker.flatten() {
            let path = entry.path();
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            if path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| !e.eq_ignore_ascii_case("cs"))
                .unwrap_or(true)
            {
                continue;
            }
            if nested.iter().any(|n| path.starts_with(n)) {
                continue;
            }
            if is_build_output(path) {
                continue;
            }

            match extract_decls_file(path) {
                Ok(found) => {
                    for u in found.usings {
                        usings.insert(u);
                    }
                    for n in found.namespaces {
                        namespaces.insert(n);
                    }
                    for (kind, name) in found.types {
                        types.entry(kind).or_default().insert(name);
                    }
                    scan.source_files.push(path.to_path_buf());
                }
                Err(e) => {
                    tracing::warn!("failed to parse {}: {e}", path.display());
                }
            }
        }

        scan.source_files.sort();
        scan.usings = usings.into_iter().collect();
        scan.declared_namespaces = namespaces.into_iter().collect();
        scan.declared_types = types
            .into_iter()
            .map(|(k, set)| (k, set.into_iter().collect()))
            .collect();
        let total_types: usize = scan.declared_types.values().map(Vec::len).sum();
        tracing::debug!(
            "{}: {} sources, {} usings, {} ns, {} types",
            project.name,
            scan.source_files.len(),
            scan.usings.len(),
            scan.declared_namespaces.len(),
            total_types,
        );
        out.push(scan);
    }
    Ok(out)
}

fn is_build_output(path: &Path) -> bool {
    for comp in path.components() {
        if let Some(s) = comp.as_os_str().to_str() {
            if s.eq_ignore_ascii_case("obj") || s.eq_ignore_ascii_case("bin") {
                return true;
            }
        }
    }
    false
}

fn extract_decls_file(path: &Path) -> Result<FileDecls> {
    let src = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    extract_decls(&src)
}

/// Back-compat: extract only `using` targets. Used by the CLI's ast-dump.
pub fn extract_usings(src: &str) -> Result<Vec<String>> {
    Ok(extract_decls(src)?.usings)
}

pub fn extract_decls(src: &str) -> Result<FileDecls> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_c_sharp::language())
        .map_err(|e| anyhow::anyhow!("set_language: {e}"))?;
    let Some(tree) = parser.parse(src, None) else {
        return Ok(FileDecls::default());
    };
    let mut out = FileDecls::default();
    let mut cursor = tree.walk();
    let mut ns_stack: Vec<String> = Vec::new();
    let mut ty_stack: Vec<String> = Vec::new();
    visit(&mut cursor, src.as_bytes(), &mut ns_stack, &mut ty_stack, &mut out);
    Ok(out)
}

fn type_kind_for(node_kind: &str) -> Option<TypeKind> {
    match node_kind {
        "class_declaration" => Some(TypeKind::Class),
        "interface_declaration" => Some(TypeKind::Interface),
        "struct_declaration" => Some(TypeKind::Struct),
        "record_declaration" => Some(TypeKind::Record),
        "record_struct_declaration" => Some(TypeKind::RecordStruct),
        "enum_declaration" => Some(TypeKind::Enum),
        "delegate_declaration" => Some(TypeKind::Delegate),
        _ => None,
    }
}

fn visit(
    cursor: &mut TreeCursor<'_>,
    src: &[u8],
    ns_stack: &mut Vec<String>,
    ty_stack: &mut Vec<String>,
    out: &mut FileDecls,
) {
    // Track how many namespaces this sibling-chain pushed via file-scoped
    // declarations. Those don't nest syntactically — every following sibling
    // belongs to the file-scoped namespace — so we pop them all once the
    // enclosing scope ends.
    let mut file_scoped_pushes: usize = 0;
    loop {
        let node = cursor.node();
        let kind = node.kind();
        let mut pushed_ns = false;
        let mut pushed_ty = false;

        match kind {
            "using_directive" => {
                // For `using Json = Newtonsoft.Json;` there's a `name` field
                // for the alias; skip it and take the target path child.
                let alias_id = node.child_by_field_name("name").map(|n| n.id());
                let mut tc = node.walk();
                let mut target: Option<String> = None;
                for child in node.children(&mut tc) {
                    if Some(child.id()) == alias_id {
                        continue;
                    }
                    let ck = child.kind();
                    if ck == "identifier" || ck == "qualified_name" {
                        if let Ok(text) = child.utf8_text(src) {
                            target = Some(text.to_string());
                        }
                    }
                }
                if let Some(t) = target {
                    out.usings.push(t);
                }
            }
            "namespace_declaration" | "file_scoped_namespace_declaration" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(src).ok())
                {
                    let full = if ns_stack.is_empty() {
                        name.to_string()
                    } else {
                        format!("{}.{}", ns_stack.join("."), name)
                    };
                    out.namespaces.push(full.clone());
                    ns_stack.push(full);
                    if kind == "file_scoped_namespace_declaration" {
                        // Persist for the rest of this sibling chain.
                        file_scoped_pushes += 1;
                    } else {
                        pushed_ns = true;
                    }
                }
            }
            other => {
                if let Some(type_kind) = type_kind_for(other) {
                    if let Some(name) = node
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(src).ok())
                    {
                        let prefix = if !ty_stack.is_empty() {
                            ty_stack.join(".")
                        } else {
                            ns_stack.last().cloned().unwrap_or_default()
                        };
                        let full = if prefix.is_empty() {
                            name.to_string()
                        } else {
                            format!("{prefix}.{name}")
                        };
                        out.types.push((type_kind, full.clone()));
                        ty_stack.push(full);
                        pushed_ty = true;
                    }
                }
            }
        }

        if cursor.goto_first_child() {
            visit(cursor, src, ns_stack, ty_stack, out);
            cursor.goto_parent();
        }

        if pushed_ns {
            ns_stack.pop();
        }
        if pushed_ty {
            ty_stack.pop();
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }
    for _ in 0..file_scoped_pushes {
        ns_stack.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_using_directives() {
        let src = r#"
using System;
using System.Collections.Generic;
using static System.Math;
using Json = Newtonsoft.Json;

namespace Foo {
    class Bar {}
}
"#;
        let u = extract_usings(src).unwrap();
        assert!(u.contains(&"System".to_string()));
        assert!(u.contains(&"System.Collections.Generic".to_string()));
        // `using Json = Newtonsoft.Json;` also pulls in the target namespace.
        assert!(u.contains(&"Newtonsoft.Json".to_string()));
    }

    fn has(d: &FileDecls, kind: TypeKind, name: &str) -> bool {
        d.types
            .iter()
            .any(|(k, n)| *k == kind && n == name)
    }

    #[test]
    fn extracts_namespaces_and_types() {
        let src = r#"
namespace Foo.Bar {
    public class Outer {
        private class Inner {}
        public enum State { A, B }
    }
    public interface IThing {}
    public struct Point {}
    public record R(int X);
    public delegate void Handler();
}

namespace Sibling {
    public class S {}
}
"#;
        let d = extract_decls(src).unwrap();
        assert!(d.namespaces.contains(&"Foo.Bar".to_string()));
        assert!(d.namespaces.contains(&"Sibling".to_string()));
        assert!(has(&d, TypeKind::Class, "Foo.Bar.Outer"));
        assert!(has(&d, TypeKind::Class, "Foo.Bar.Outer.Inner"));
        assert!(has(&d, TypeKind::Enum, "Foo.Bar.Outer.State"));
        assert!(has(&d, TypeKind::Interface, "Foo.Bar.IThing"));
        assert!(has(&d, TypeKind::Struct, "Foo.Bar.Point"));
        assert!(has(&d, TypeKind::Record, "Foo.Bar.R"));
        assert!(has(&d, TypeKind::Delegate, "Foo.Bar.Handler"));
        assert!(has(&d, TypeKind::Class, "Sibling.S"));
    }

    #[test]
    fn extracts_file_scoped_namespace() {
        let src = r#"
namespace Acme.Widgets;

public class Widget {}
public class Gadget {}
"#;
        let d = extract_decls(src).unwrap();
        assert_eq!(d.namespaces, vec!["Acme.Widgets"]);
        assert!(has(&d, TypeKind::Class, "Acme.Widgets.Widget"));
        assert!(has(&d, TypeKind::Class, "Acme.Widgets.Gadget"));
    }

    #[test]
    fn nested_namespaces() {
        let src = r#"
namespace A {
    namespace B {
        class C {}
    }
}
"#;
        let d = extract_decls(src).unwrap();
        assert!(d.namespaces.contains(&"A".to_string()));
        assert!(d.namespaces.contains(&"A.B".to_string()));
        assert!(has(&d, TypeKind::Class, "A.B.C"));
    }
}
