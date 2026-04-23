//! Tree-sitter powered scan of `.cs` source files.
//!
//! For v1 we extract only the namespaces from `using` directives. That's enough
//! to cross-check declared NuGet packages against what source actually imports.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use tree_sitter::{Parser, TreeCursor};

/// Result of scanning one project's sources.
#[derive(Debug, Clone, Default)]
pub struct SourceScan {
    pub source_files: Vec<PathBuf>,
    /// Namespaces used via `using X.Y.Z;`. Deduped, sorted.
    pub usings: Vec<String>,
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

            match extract_usings_file(path) {
                Ok(found) => {
                    for u in found {
                        usings.insert(u);
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
        tracing::debug!(
            "{}: {} sources, {} usings",
            project.name,
            scan.source_files.len(),
            scan.usings.len()
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

fn extract_usings_file(path: &Path) -> Result<Vec<String>> {
    let src = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    extract_usings(&src)
}

pub fn extract_usings(src: &str) -> Result<Vec<String>> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_c_sharp::language())
        .map_err(|e| anyhow::anyhow!("set_language: {e}"))?;
    let Some(tree) = parser.parse(src, None) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let mut cursor = tree.walk();
    visit(&mut cursor, src.as_bytes(), &mut out);
    Ok(out)
}

fn visit(cursor: &mut TreeCursor<'_>, src: &[u8], out: &mut Vec<String>) {
    loop {
        let node = cursor.node();
        if node.kind() == "using_directive" {
            // For `using Json = Newtonsoft.Json;` there's a `name` identifier (the
            // alias) AND a separate type path we actually care about. Skip the
            // alias field; keep the last identifier/qualified_name child, which
            // is the target namespace/type.
            let alias_id = node.child_by_field_name("name").map(|n| n.id());
            let mut tc = node.walk();
            let mut target: Option<String> = None;
            for child in node.children(&mut tc) {
                if Some(child.id()) == alias_id {
                    continue;
                }
                let kind = child.kind();
                if kind == "identifier" || kind == "qualified_name" {
                    if let Ok(text) = child.utf8_text(src) {
                        target = Some(text.to_string());
                    }
                }
            }
            if let Some(t) = target {
                out.push(t);
            }
        }
        if cursor.goto_first_child() {
            visit(cursor, src, out);
            cursor.goto_parent();
        }
        if !cursor.goto_next_sibling() {
            break;
        }
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
}
