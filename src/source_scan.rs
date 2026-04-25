//! Tree-sitter powered scan of `.cs` source files.
//!
//! For v1 we extract only the namespaces from `using` directives. That's enough
//! to cross-check declared NuGet packages against what source actually imports.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use tree_sitter::{Parser, TreeCursor};

use crate::model::{TypeKind, TypeMetrics};

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
    /// Per-type metrics keyed by fully-qualified type name.
    pub type_metrics: BTreeMap<String, TypeMetrics>,
    /// Simple type names referenced in type-position syntax. Deduped, sorted.
    pub referenced_types: Vec<String>,
}

/// Per-file extraction output.
#[derive(Debug, Default, Clone, bincode::Encode, bincode::Decode)]
pub struct FileDecls {
    pub usings: Vec<String>,
    pub namespaces: Vec<String>,
    pub types: Vec<(TypeKind, String)>,
    pub metrics: Vec<(String, TypeMetrics)>,
    /// Simple type names observed in type-position contexts.
    pub references: Vec<String>,
}

/// Scan `.cs` files under each project. Files inside a nested project's
/// directory are attributed to that nested project only.
pub fn scan_projects(projects: &[crate::model::Project]) -> Result<Vec<SourceScan>> {
    scan_projects_cached(projects, None)
}

/// Same as `scan_projects`, but consults / updates an on-disk cache of
/// per-file `FileDecls` keyed by absolute path + (mtime_ns, len). Cache
/// hits skip the tree-sitter parse entirely. The cache is rewritten in
/// place on success — stale entries (paths no longer scanned) are evicted.
pub fn scan_projects_cached(
    projects: &[crate::model::Project],
    cache_path: Option<&Path>,
) -> Result<Vec<SourceScan>> {
    let dirs: Vec<PathBuf> = projects
        .iter()
        .map(|p| p.path.parent().map(Path::to_path_buf).unwrap_or_default())
        .collect();

    let mut cache = match cache_path {
        Some(p) => crate::cache::load(p),
        None => crate::cache::Cache::default(),
    };
    let mut live_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut hits: u64 = 0;
    let mut misses: u64 = 0;

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
        let mut metrics: BTreeMap<String, TypeMetrics> = BTreeMap::new();
        let mut references: BTreeSet<String> = BTreeSet::new();

        let walker = WalkBuilder::new(project_dir)
            .follow_links(false)
            .git_ignore(true)
            .hidden(false)
            .build();

        // Collect .cs paths first, then sort — ensures deterministic file_id
        // assignment across runs without retroactively breaking the mapping.
        let mut cs_paths: Vec<PathBuf> = Vec::new();
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
            cs_paths.push(path.to_path_buf());
        }
        // Sort by (parent_dir, basename) so file_ids assigned below match
        // the order produced when metrics.yaml groups source_files by parent
        // directory. A plain full-path sort would place `Foo/Bar/x.cs`
        // *between* `Foo/a.cs` and `Foo/c.cs`, which breaks alignment with
        // the grouped view.
        cs_paths.sort_by(|a, b| {
            let pa = a.parent().unwrap_or_else(|| Path::new(""));
            let pb = b.parent().unwrap_or_else(|| Path::new(""));
            pa.cmp(pb).then_with(|| a.file_name().cmp(&b.file_name()))
        });

        for path in &cs_paths {
            // Reserve the file_id *before* parsing so spans stamped during
            // this file's extraction match its eventual index in source_files.
            let file_id = scan.source_files.len() as u32;
            let key = path.to_string_lossy().into_owned();
            live_keys.insert(key.clone());
            let stamp = crate::cache::stamp(path);
            let cached: Option<FileDecls> = match stamp {
                Some((m, l)) => cache.get(&key, m, l).cloned(),
                None => None,
            };
            let parse_result: Result<FileDecls> = if let Some(d) = cached {
                hits += 1;
                Ok(d)
            } else {
                misses += 1;
                match extract_decls_unstamped(path) {
                    Ok(d) => {
                        if let Some((m, l)) = stamp {
                            cache.insert(key.clone(), m, l, d.clone());
                        }
                        Ok(d)
                    }
                    Err(e) => Err(e),
                }
            };
            match parse_result.map(|mut d| {
                stamp_file_id(&mut d, file_id);
                d
            }) {
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
                    for r in found.references {
                        references.insert(r);
                    }
                    for (name, m) in found.metrics {
                        // Partial classes declared across multiple files: sum.
                        let slot = metrics.entry(name).or_default();
                        slot.loc = slot.loc.saturating_add(m.loc);
                        slot.members = slot.members.saturating_add(m.members);
                        slot.complexity = slot.complexity.saturating_add(m.complexity);
                        slot.spans.extend(m.spans);
                        slot.methods.extend(m.methods);
                        // Partial-class base lists may appear on any of the
                        // partial fragments; merge uniquely preserving order.
                        for b in m.bases {
                            if !slot.bases.contains(&b) {
                                slot.bases.push(b);
                            }
                        }
                        // Same for attributes — a partial fragment may carry
                        // its own. Keep duplicates filtered so the rendered
                        // list is faithful to the user-visible decoration.
                        for a in m.attributes {
                            if !slot.attributes.contains(&a) {
                                slot.attributes.push(a);
                            }
                        }
                        // Per-type referenced names accumulate across
                        // partial fragments; dedup-and-sort happens once
                        // after the whole project's files have been folded.
                        slot.referenced_types.extend(m.referenced_types);
                    }
                    scan.source_files.push(path.to_path_buf());
                }
                Err(e) => {
                    tracing::warn!("failed to parse {}: {e}", path.display());
                }
            }
        }

        // Non-partial types inherit their methods' file from `spans[0]`, so
        // drop the per-method `file_id` in that case to keep metrics.yaml
        // terse. Partials keep it on every method.
        for m in metrics.values_mut() {
            if m.spans.len() == 1 {
                for meth in &mut m.methods {
                    meth.file_id = None;
                }
            }
            m.referenced_types.sort();
            m.referenced_types.dedup();
        }

        // Do NOT sort `scan.source_files` here — spans already reference
        // these by index. The input list was sorted before iteration so the
        // end state is deterministic and still index-aligned.
        scan.usings = usings.into_iter().collect();
        scan.declared_namespaces = namespaces.into_iter().collect();
        scan.declared_types = types
            .into_iter()
            .map(|(k, set)| (k, set.into_iter().collect()))
            .collect();
        scan.type_metrics = metrics;
        scan.referenced_types = references.into_iter().collect();
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

    if let Some(p) = cache_path {
        cache.retain_keys(&live_keys);
        if let Err(e) = crate::cache::save(p, &cache) {
            tracing::warn!("failed to write source-scan cache to {}: {e}", p.display());
        }
        tracing::debug!(
            "source-scan cache: {} hits, {} misses, {} live entries",
            hits,
            misses,
            cache.len()
        );
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

/// Read + parse a file and produce its `FileDecls` with `file_id` left at
/// its default (0). The caller stamps the real `file_id` once it knows the
/// scan-position index — this lets the cache store a single, scan-order-
/// independent copy of the parse output.
fn extract_decls_unstamped(path: &Path) -> Result<FileDecls> {
    let src =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    extract_decls(&src)
}

/// Overwrite every span / method's `file_id` with the supplied scan index.
fn stamp_file_id(decls: &mut FileDecls, file_id: u32) {
    for (_, m) in decls.metrics.iter_mut() {
        for sp in &mut m.spans {
            sp.file_id = file_id;
        }
        for meth in &mut m.methods {
            meth.file_id = Some(file_id);
        }
    }
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
    visit(
        &mut cursor,
        src.as_bytes(),
        &mut ns_stack,
        &mut ty_stack,
        &mut out,
    );
    Ok(out)
}

const MEMBER_KINDS: &[&str] = &[
    "method_declaration",
    "property_declaration",
    "field_declaration",
    "constructor_declaration",
    "event_declaration",
    "event_field_declaration",
    "indexer_declaration",
    "destructor_declaration",
    "operator_declaration",
    "conversion_operator_declaration",
];

const BRANCH_KINDS: &[&str] = &[
    "if_statement",
    "while_statement",
    "do_statement",
    "for_statement",
    "for_each_statement",
    "case_switch_label",
    "catch_clause",
    "conditional_expression",
    "when_clause",
];

const METHOD_LIKE_KINDS: &[&str] = &[
    "method_declaration",
    "constructor_declaration",
    "destructor_declaration",
    "operator_declaration",
    "conversion_operator_declaration",
];

/// Compute per-type metrics from its tree-sitter subtree.
fn compute_metrics(node: tree_sitter::Node<'_>, src: &[u8]) -> TypeMetrics {
    // tree-sitter rows are 0-based; emit 1-based lines for display.
    let line_start = node.start_position().row as u32 + 1;
    let line_end = node.end_position().row as u32 + 1;
    let loc = line_end.saturating_sub(line_start) + 1;
    let mut members: u32 = 0;
    let mut complexity: u32 = 0;
    let mut methods: Vec<crate::model::MethodMetric> = Vec::new();

    // Iterate direct body children to count members and collect per-method
    // metrics. Some grammars expose the body via a `body` field; others as
    // a direct `declaration_list` child — handle both.
    let body = node.child_by_field_name("body").or_else(|| {
        let mut tc = node.walk();
        let mut found = None;
        for c in node.named_children(&mut tc) {
            if c.kind() == "declaration_list" {
                found = Some(c);
                break;
            }
        }
        found
    });
    if let Some(body) = body {
        let mut bc = body.walk();
        for child in body.named_children(&mut bc) {
            let kind = child.kind();
            if MEMBER_KINDS.contains(&kind) {
                members += 1;
            }
            if METHOD_LIKE_KINDS.contains(&kind) {
                methods.push(method_metric(child, src));
            }
        }
    }

    // Cyclomatic: descend into the type's subtree and tally branch nodes.
    // `walk` starts positioned on the type node itself, so we iterate its
    // children to avoid walking the type node's siblings.
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        count_branches_siblings(&mut cursor, &mut complexity);
    }

    // Base class + implemented interfaces (simple names, in source order).
    let mut bases: Vec<String> = Vec::new();
    let mut tc = node.walk();
    for child in node.named_children(&mut tc) {
        if child.kind() == "base_list" {
            let mut bc = child.walk();
            for b in child.named_children(&mut bc) {
                collect_type_names(b, src, &mut bases);
            }
            break;
        }
    }

    let attributes = collect_attributes(node, src);

    TypeMetrics {
        loc,
        members,
        complexity,
        spans: vec![crate::model::SourceSpan {
            file_id: 0, // filled in by `extract_decls_file`
            line_start,
            line_end,
        }],
        methods,
        bases,
        attributes,
        referenced_types: Vec::new(),
    }
}

/// Collect attribute usages applied to a declaration node (type or method).
/// Each `attribute_list` child holds one or more `attribute`s; we capture
/// each as `Name` or `Name(args)`, with the trailing `Attribute` suffix
/// stripped and any C# string `"` rewritten to `'` for YAML plain-scalar
/// safety. Qualified attribute names (`ns.X.ServiceContract`) collapse to
/// the last segment.
fn collect_attributes(node: tree_sitter::Node<'_>, src: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut tc = node.walk();
    for child in node.named_children(&mut tc) {
        if child.kind() != "attribute_list" {
            continue;
        }
        let mut ac = child.walk();
        for attr in child.named_children(&mut ac) {
            if attr.kind() != "attribute" {
                continue;
            }
            let name_text = attr
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(src).ok())
                .unwrap_or("");
            let last = name_text.rsplit('.').next().unwrap_or(name_text);
            let bare = last.strip_suffix("Attribute").unwrap_or(last);
            if bare.is_empty() {
                continue;
            }

            // Locate the attribute's argument list, if any. Tree-sitter
            // c-sharp exposes it as `attribute_argument_list`.
            let mut bc = attr.walk();
            let mut args: Option<String> = None;
            for c in attr.named_children(&mut bc) {
                if c.kind() == "attribute_argument_list" {
                    if let Ok(t) = c.utf8_text(src) {
                        let trimmed = t.trim();
                        let inner = trimmed
                            .strip_prefix('(')
                            .and_then(|s| s.strip_suffix(')'))
                            .unwrap_or(trimmed);
                        let cleaned = inner.replace('"', "'");
                        // Compact internal whitespace and newlines so the
                        // method one-liner doesn't grow vertically when an
                        // attribute argument spans lines.
                        let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
                        if !cleaned.is_empty() {
                            args = Some(cleaned);
                        }
                    }
                    break;
                }
            }
            out.push(match args {
                Some(a) => format!("{bare}({a})"),
                None => bare.to_string(),
            });
        }
    }
    out
}

fn method_metric(node: tree_sitter::Node<'_>, src: &[u8]) -> crate::model::MethodMetric {
    let name = node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(src).ok())
        .unwrap_or("<anonymous>")
        .to_string();
    let line_start = node.start_position().row as u32 + 1;
    let line_end = node.end_position().row as u32 + 1;
    let loc = line_end.saturating_sub(line_start) + 1;
    let mut complexity: u32 = 0;
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        count_branches_siblings(&mut cursor, &mut complexity);
    }
    let attributes = collect_attributes(node, src);
    let signature_types = collect_signature_types(node, src);
    crate::model::MethodMetric {
        name,
        line_start,
        line_end,
        loc,
        complexity,
        file_id: None, // stamped by `extract_decls_file`
        attributes,
        signature_types,
    }
}

/// Pull every simple type name out of a method's signature: each parameter's
/// declared type plus the method's return type. Sorted, deduped. Predefined
/// types (`int`, `string`, …) are filtered by `collect_type_names` already.
fn collect_signature_types(node: tree_sitter::Node<'_>, src: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(ret) = node
        .child_by_field_name("returns")
        .or_else(|| node.child_by_field_name("return_type"))
        .or_else(|| node.child_by_field_name("type"))
    {
        collect_type_names(ret, src, &mut out);
    }
    let mut tc = node.walk();
    for child in node.named_children(&mut tc) {
        if child.kind() != "parameter_list" {
            continue;
        }
        let mut pc = child.walk();
        for param in child.named_children(&mut pc) {
            if param.kind() != "parameter" {
                continue;
            }
            if let Some(t) = param.child_by_field_name("type") {
                collect_type_names(t, src, &mut out);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn count_branches_siblings(cursor: &mut TreeCursor<'_>, out: &mut u32) {
    loop {
        if BRANCH_KINDS.contains(&cursor.node().kind()) {
            *out = out.saturating_add(1);
        }
        if cursor.goto_first_child() {
            count_branches_siblings(cursor, out);
            cursor.goto_parent();
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

/// Push every simple type name found under `node` into both the project-wide
/// references bag and the innermost enclosing type's `referenced_types`.
/// Self-references (a type referencing itself by simple name) are filtered
/// out so consumer-graphs stay clean.
fn record_refs(
    node: tree_sitter::Node<'_>,
    src: &[u8],
    ty_stack: &[String],
    out: &mut FileDecls,
) {
    let mut buf: Vec<String> = Vec::new();
    collect_type_names(node, src, &mut buf);
    if buf.is_empty() {
        return;
    }
    let enclosing_fqn = ty_stack.last().cloned();
    let enclosing_simple = enclosing_fqn
        .as_ref()
        .and_then(|f| f.rsplit('.').next())
        .map(str::to_string);

    for r in buf {
        out.references.push(r.clone());
        if let (Some(fqn), Some(simple)) = (&enclosing_fqn, &enclosing_simple) {
            if &r == simple {
                continue;
            }
            // The enclosing type's metrics entry was pushed when its
            // declaration was first visited, so it must already exist by
            // the time we see references inside its body. Search from the
            // end since recently-declared types are most likely current.
            if let Some((_, m)) = out.metrics.iter_mut().rfind(|(n, _)| n == fqn) {
                m.referenced_types.push(r);
            }
        }
    }
}

/// Recursively extract simple type names from a type-position node. Unwraps
/// nullable/array/pointer wrappers, descends into generic arguments and tuple
/// elements. `predefined_type` (int, string, …) and bare punctuation are
/// skipped.
fn collect_type_names(node: tree_sitter::Node<'_>, src: &[u8], out: &mut Vec<String>) {
    match node.kind() {
        "identifier" => {
            if let Ok(text) = node.utf8_text(src) {
                out.push(text.to_string());
            }
        }
        "qualified_name" => {
            // Take the rightmost `name` — the actual type identifier.
            if let Some(name) = node.child_by_field_name("name") {
                collect_type_names(name, src, out);
            } else if let Ok(text) = node.utf8_text(src) {
                if let Some(last) = text.rsplit('.').next() {
                    out.push(last.to_string());
                }
            }
        }
        "generic_name" => {
            if let Some(name) = node.child_by_field_name("name") {
                if let Ok(text) = name.utf8_text(src) {
                    out.push(text.to_string());
                }
            }
            // Descend into the type argument list.
            let mut c = node.walk();
            for child in node.named_children(&mut c) {
                if child.kind() == "type_argument_list" {
                    let mut cc = child.walk();
                    for arg in child.named_children(&mut cc) {
                        collect_type_names(arg, src, out);
                    }
                }
            }
        }
        "nullable_type" | "array_type" | "pointer_type" => {
            // The wrapped type is either the `type` field or the first named child.
            if let Some(inner) = node.child_by_field_name("type") {
                collect_type_names(inner, src, out);
            } else {
                let mut c = node.walk();
                let first = node.named_children(&mut c).next();
                if let Some(inner) = first {
                    collect_type_names(inner, src, out);
                }
            }
        }
        "tuple_type" | "tuple_element" => {
            let mut c = node.walk();
            for child in node.named_children(&mut c) {
                collect_type_names(child, src, out);
            }
        }
        "predefined_type" | "implicit_type" | "ref_type" => {}
        _ => {}
    }
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
                // Type-position extraction — record referenced simple names
                // from constructs that carry a `type` / return-type child.
                // Delegate declarations also declare a type (handled below),
                // so these branches don't early-return.
                if matches!(
                    other,
                    "object_creation_expression"
                        | "variable_declaration"
                        | "parameter"
                        | "typeof_expression"
                        | "cast_expression"
                        | "as_expression"
                        | "is_expression"
                        | "property_declaration"
                        | "indexer_declaration"
                        | "method_declaration"
                        | "delegate_declaration"
                        | "conversion_operator_declaration"
                        | "operator_declaration"
                ) {
                    if let Some(t) = node
                        .child_by_field_name("type")
                        .or_else(|| node.child_by_field_name("returns"))
                        .or_else(|| node.child_by_field_name("return_type"))
                    {
                        record_refs(t, src, ty_stack, out);
                    }
                } else if other == "base_list" {
                    let mut bc = node.walk();
                    for child in node.named_children(&mut bc) {
                        record_refs(child, src, ty_stack, out);
                    }
                } else if other == "attribute" {
                    if let Some(name) = node.child_by_field_name("name") {
                        record_refs(name, src, ty_stack, out);
                    }
                }

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
                        out.metrics.push((full.clone(), compute_metrics(node, src)));
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
        d.types.iter().any(|(k, n)| *k == kind && n == name)
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
    fn computes_type_metrics() {
        let src = r#"
namespace N {
    public class A {
        public int F;
        public void M(int x) {
            if (x > 0) {
                for (int i = 0; i < x; i++) {
                    if (i == 3) { }
                }
            } else {
                while (x < 0) { x++; }
            }
            try { } catch { }
            var y = x > 0 ? 1 : 2;
        }
        private class Inner {}
    }
}
"#;
        let d = extract_decls(src).unwrap();
        let a = d
            .metrics
            .iter()
            .find(|(n, _)| n == "N.A")
            .expect("N.A metrics");
        let m = &a.1;
        // One field + one method = 2 direct members (nested type isn't counted).
        assert_eq!(m.members, 2);
        // 2 if, 1 for, 1 while, 1 catch, 1 ternary = 6.
        assert_eq!(m.complexity, 6);
        assert!(m.loc >= 15);
        // Per-method breakdown: the single method M should appear.
        assert_eq!(m.methods.len(), 1);
        assert_eq!(m.methods[0].name, "M");
        assert_eq!(m.methods[0].complexity, 6);
        // Inner type is tracked separately with its own (zero) metrics.
        let inner = d.metrics.iter().find(|(n, _)| n == "N.A.Inner").unwrap();
        assert_eq!(inner.1.members, 0);
        assert_eq!(inner.1.complexity, 0);
    }

    #[test]
    fn extracts_type_and_method_attributes() {
        let src = r#"
namespace Acme.Billing {
    [ServiceContract]
    public interface IInvoiceService {
        [OperationContract]
        void Submit(int id);

        [OperationContract(IsOneWay = true)]
        void FireAndForget();

        void NotExposed();
    }

    [ApiController]
    [Route("api/invoices")]
    public class InvoicesController {
        [HttpGet, Authorize(Roles = "admin")]
        public void List() {}
    }
}
"#;
        let d = extract_decls(src).unwrap();

        let svc = d
            .metrics
            .iter()
            .find(|(n, _)| n == "Acme.Billing.IInvoiceService")
            .expect("contract metrics");
        // `Attribute` suffix is implicit here, but suffix-strip should still
        // be a no-op when the source already wrote the bare name.
        assert_eq!(svc.1.attributes, vec!["ServiceContract".to_string()]);
        let by_name: std::collections::HashMap<&str, &crate::model::MethodMetric> = svc
            .1
            .methods
            .iter()
            .map(|m| (m.name.as_str(), m))
            .collect();
        assert_eq!(
            by_name["Submit"].attributes,
            vec!["OperationContract".to_string()]
        );
        assert_eq!(
            by_name["FireAndForget"].attributes,
            vec!["OperationContract(IsOneWay = true)".to_string()]
        );
        assert!(by_name["NotExposed"].attributes.is_empty());

        let ctrl = d
            .metrics
            .iter()
            .find(|(n, _)| n == "Acme.Billing.InvoicesController")
            .expect("controller metrics");
        assert_eq!(
            ctrl.1.attributes,
            vec![
                "ApiController".to_string(),
                "Route('api/invoices')".to_string(),
            ]
        );
        let list = ctrl
            .1
            .methods
            .iter()
            .find(|m| m.name == "List")
            .expect("List method");
        assert_eq!(
            list.attributes,
            vec![
                "HttpGet".to_string(),
                "Authorize(Roles = 'admin')".to_string(),
            ]
        );
    }

    #[test]
    fn strips_attribute_suffix_and_qualifies_to_last_segment() {
        let src = r#"
namespace N {
    [System.SerializableAttribute]
    public class A {}
}
"#;
        let d = extract_decls(src).unwrap();
        let a = d.metrics.iter().find(|(n, _)| n == "N.A").unwrap();
        assert_eq!(a.1.attributes, vec!["Serializable".to_string()]);
    }

    #[test]
    fn method_one_liner_carries_attributes() {
        let m = crate::model::MethodMetric {
            name: "Get".to_string(),
            line_start: 20,
            line_end: 28,
            loc: 9,
            complexity: 2,
            file_id: None,
            attributes: vec!["HttpGet".to_string(), "Route('{id}')".to_string()],
            signature_types: vec![],
        };
        let s = serde_yaml::to_string(&m).unwrap();
        assert!(
            s.contains("Get L20-28 loc=9 cx=2 [HttpGet, Route('{id}')]"),
            "unexpected serialization: {s}"
        );
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
