//! `atlas lookup` — resolve what the atlas bundle knows about a type name or
//! a source file.
//!
//! Consumes the YAML artifacts produced by `atlas --output-dir` (typically
//! `classes.yaml`, `metrics.yaml`, `references.yaml`) and produces AI-friendly
//! output: file paths + line ranges, per-method metrics, and cross-project
//! callers.
//!
//! Type-name mode (`run`) accepts a simple name (matches every namespace /
//! project declaring that name) or a fully-qualified name (exact match).
//! File mode (`run_file`) takes a path and lists the types + methods declared
//! in that file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_yaml::Value;

use crate::signatures;

#[derive(Debug, Serialize)]
pub struct LookupResult {
    pub query: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matches: Vec<Match>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ambiguous_in: Vec<String>,
    /// Types whose `base_list` names the query. Grouped by declaring project.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub subclasses: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Serialize)]
pub struct Match {
    pub fqn: String,
    pub kind: String,
    pub project: String,
    pub project_path: PathBuf,
    pub namespace: String,
    /// One entry per declaration site. Partial classes have multiple.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub at: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loc: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub members: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub complexity: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bases: Vec<String>,
    /// Pre-formatted single-line methods: `"Name  path:start-end  loc=N  cx=N"`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub referenced_by: Vec<String>,
    /// Endpoint classification when this type appears in `endpoints.yaml`
    /// (WCF contract, Web API controller, SignalR hub, etc.). Stored as a
    /// passthrough YAML node so the lookup output mirrors whatever shape
    /// endpoints.yaml ships, without re-parsing each field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<Value>,
}

/// Output of `run_file`. Lists everything the atlas knows about types
/// declared in a given source file.
#[derive(Debug, Serialize)]
pub struct FileResult {
    pub query: PathBuf,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matches: Vec<FileMatch>,
}

#[derive(Debug, Serialize)]
pub struct FileMatch {
    pub file: PathBuf,
    pub project: String,
    pub project_path: PathBuf,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<String>,
}

/// Options controlling output shape. Signatures are on by default; pass
/// `signatures: false` (wired to `--no-sig`) to skip the tree-sitter re-parse.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub signatures: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { signatures: true }
    }
}

/// Shared tree-sitter signature cache — one entry per source file. Reusing
/// the same cache across multiple lookups in a batch call means each
/// `.cs` file is parsed at most once per invocation.
#[derive(Default)]
pub struct SigCache {
    map: std::collections::HashMap<PathBuf, std::collections::HashMap<u32, String>>,
}

impl SigCache {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Top-level output shape for `nspect lookup`. Always wrapped, even for a
/// single query — makes batch and single calls parseable by the same code.
#[derive(Debug, Serialize)]
pub struct BatchOutput {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<LookupResult>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileResult>,
}

pub fn run(atlas_dir: &Path, query: &str) -> Result<LookupResult> {
    let mut cache = SigCache::new();
    run_with(atlas_dir, query, Options::default(), &mut cache)
}

pub fn run_with(
    atlas_dir: &Path,
    query: &str,
    opts: Options,
    cache: &mut SigCache,
) -> Result<LookupResult> {
    let repo_root = repo_root_from(atlas_dir);
    let metrics_doc = load_optional(atlas_dir, "metrics.yaml")?;
    let classes_doc = load_optional(atlas_dir, "classes.yaml")?;
    let references_doc = load_optional(atlas_dir, "references.yaml")?;

    // Prefer metrics.yaml for declarations (has kind + metrics). Fall back
    // to classes.yaml if metrics artifact wasn't written.
    let decls_doc = metrics_doc.as_ref().or(classes_doc.as_ref());
    let Some(decls) = decls_doc else {
        bail!(
            "atlas dir {} contains neither metrics.yaml nor classes.yaml",
            atlas_dir.display()
        );
    };

    let (simple, fqn_filter) = match query.rsplit_once('.') {
        Some((ns, name)) => (name.to_string(), Some((ns.to_string(), name.to_string()))),
        None => (query.to_string(), None),
    };

    let mut matches: Vec<Match> = Vec::new();
    let mut pending_methods: Vec<Vec<MethodInfo>> = Vec::new();
    for project in projects_of(decls) {
        let pname = string_at(project, "name").unwrap_or_default();
        let ppath = string_at(project, "path").unwrap_or_default();
        let files = source_files_of(project);
        let Some(namespaces) = project.get("namespaces").and_then(Value::as_mapping) else {
            continue;
        };
        for (ns_key, kinds) in namespaces {
            let ns = ns_key.as_str().unwrap_or("").to_string();
            let Some(kinds_map) = kinds.as_mapping() else {
                continue;
            };
            for (kind_key, entries) in kinds_map {
                let kind = kind_key.as_str().unwrap_or("").to_string();
                for (local, metrics_val) in iter_entries(entries) {
                    if !local_matches(&local, &simple) {
                        continue;
                    }
                    if let Some((want_ns, want_name)) = &fqn_filter {
                        if &ns != want_ns || !local.ends_with(want_name) {
                            continue;
                        }
                    }
                    let fqn = if ns == "<global>" || ns.is_empty() {
                        local.clone()
                    } else {
                        format!("{ns}.{local}")
                    };
                    let mut m = Match {
                        fqn,
                        kind: kind.clone(),
                        project: pname.clone(),
                        project_path: PathBuf::from(&ppath),
                        namespace: if ns == "<global>" {
                            String::new()
                        } else {
                            ns.clone()
                        },
                        at: Vec::new(),
                        loc: None,
                        members: None,
                        complexity: None,
                        bases: Vec::new(),
                        methods: Vec::new(),
                        referenced_by: Vec::new(),
                        endpoint: None,
                    };
                    let infos = if let Some(body) = metrics_val.as_ref() {
                        populate_from_metrics(&mut m, body, &files)
                    } else {
                        Vec::new()
                    };
                    matches.push(m);
                    pending_methods.push(infos);
                }
            }
        }
    }

    // Reverse-lookup callers via references.yaml.
    let mut ambiguous_in: Vec<String> = Vec::new();
    if let Some(refs) = &references_doc {
        for project in projects_of(refs) {
            let caller = string_at(project, "name").unwrap_or_default();
            if let Some(xp) = project
                .get("resolved_cross_project")
                .and_then(Value::as_mapping)
            {
                for (declaring, names) in xp {
                    let declaring = declaring.as_str().unwrap_or("");
                    let Some(list) = names.as_sequence() else {
                        continue;
                    };
                    if list.iter().any(|n| n.as_str() == Some(&simple)) {
                        for m in matches.iter_mut() {
                            if m.project == declaring {
                                m.referenced_by.push(caller.clone());
                            }
                        }
                    }
                }
            }
            if let Some(amb) = project.get("ambiguous").and_then(Value::as_mapping) {
                if let Some(list) = amb
                    .get(Value::from(simple.clone()))
                    .and_then(Value::as_sequence)
                {
                    if list.iter().any(|p| p.as_str().is_some()) {
                        ambiguous_in.push(caller.clone());
                    }
                }
            }
        }
    }

    for (m, infos) in matches.iter_mut().zip(pending_methods.iter()) {
        m.methods = format_methods(infos, &repo_root, opts.signatures, cache);
    }
    for m in matches.iter_mut() {
        m.referenced_by.sort();
        m.referenced_by.dedup();
    }
    ambiguous_in.sort();
    ambiguous_in.dedup();

    // Subclasses: scan metrics.yaml for any type whose `bases` list contains
    // the query's simple name.
    let mut subclasses: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Some(metrics) = &metrics_doc {
        for project in projects_of(metrics) {
            let pname = string_at(project, "name").unwrap_or_default();
            let Some(namespaces) = project.get("namespaces").and_then(Value::as_mapping) else {
                continue;
            };
            for (ns_key, kinds) in namespaces {
                let ns = ns_key.as_str().unwrap_or("").to_string();
                let Some(kinds_map) = kinds.as_mapping() else {
                    continue;
                };
                for (_kind, entries) in kinds_map {
                    let Some(map) = entries.as_mapping() else {
                        continue;
                    };
                    for (local_key, body) in map {
                        let Some(local) = local_key.as_str() else {
                            continue;
                        };
                        let Some(bases) = body.get("bases").and_then(Value::as_sequence) else {
                            continue;
                        };
                        if bases.iter().any(|b| b.as_str() == Some(&simple)) {
                            let fqn = if ns == "<global>" || ns.is_empty() {
                                local.to_string()
                            } else {
                                format!("{ns}.{local}")
                            };
                            subclasses.entry(pname.clone()).or_default().push(fqn);
                        }
                    }
                }
            }
        }
        for names in subclasses.values_mut() {
            names.sort();
            names.dedup();
        }
    }

    // Annotate each match with its `endpoints.yaml` entry, when one exists.
    // Built lazily — only the FQNs we matched against are looked up.
    let endpoints_doc = load_optional(atlas_dir, "endpoints.yaml")?;
    if let Some(eps) = &endpoints_doc {
        let by_fqn = endpoints_index(eps);
        for m in matches.iter_mut() {
            if let Some(node) = by_fqn.get(&m.fqn) {
                m.endpoint = Some((*node).clone());
            }
        }
    }

    Ok(LookupResult {
        query: query.to_string(),
        matches,
        ambiguous_in,
        subclasses,
    })
}

/// Build a `type -> Endpoint` index from a parsed `endpoints.yaml`.
fn endpoints_index(doc: &Value) -> BTreeMap<String, &Value> {
    let mut out: BTreeMap<String, &Value> = BTreeMap::new();
    let Some(projects) = doc.get("projects").and_then(Value::as_sequence) else {
        return out;
    };
    for project in projects {
        let Some(eps) = project.get("endpoints").and_then(Value::as_sequence) else {
            continue;
        };
        for ep in eps {
            if let Some(t) = ep.get("type").and_then(Value::as_str) {
                out.insert(t.to_string(), ep);
            }
        }
    }
    out
}

/// List types declared in a source file. Matches by suffix (so the caller
/// can pass `Customer.cs` or a deeper `Src/.../Customer.cs`).
pub fn run_file(atlas_dir: &Path, query: &Path) -> Result<FileResult> {
    let metrics_doc = load_optional(atlas_dir, "metrics.yaml")?;
    let classes_doc = load_optional(atlas_dir, "classes.yaml")?;
    let decls = metrics_doc
        .as_ref()
        .or(classes_doc.as_ref())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "atlas dir {} contains neither metrics.yaml nor classes.yaml",
                atlas_dir.display()
            )
        })?;

    let needle = query.to_string_lossy().replace('\\', "/");
    let mut matches: Vec<FileMatch> = Vec::new();
    for project in projects_of(decls) {
        let pname = string_at(project, "name").unwrap_or_default();
        let ppath = string_at(project, "path").unwrap_or_default();
        let files = source_files_of(project);
        // Find file_ids whose path matches the needle (suffix or equals).
        let hit_ids: Vec<u32> = files
            .iter()
            .enumerate()
            .filter_map(|(i, p)| {
                let s = p.to_string_lossy().replace('\\', "/");
                if s == needle || s.ends_with(&needle) {
                    Some(i as u32)
                } else {
                    None
                }
            })
            .collect();
        if hit_ids.is_empty() {
            continue;
        }

        let Some(namespaces) = project.get("namespaces").and_then(Value::as_mapping) else {
            continue;
        };
        let mut per_file: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        for (ns_key, kinds) in namespaces {
            let ns = ns_key.as_str().unwrap_or("");
            let Some(kinds_map) = kinds.as_mapping() else {
                continue;
            };
            for (_kind, entries) in kinds_map {
                let Some(map) = entries.as_mapping() else {
                    continue;
                };
                for (local_key, body) in map {
                    let Some(local) = local_key.as_str() else {
                        continue;
                    };
                    let fqn = if ns == "<global>" || ns.is_empty() {
                        local.to_string()
                    } else {
                        format!("{ns}.{local}")
                    };
                    for span in parse_spans(body) {
                        if hit_ids.contains(&span.file_id) {
                            per_file
                                .entry(span.file_id)
                                .or_default()
                                .push(format!("{fqn} L{}-{}", span.line_start, span.line_end));
                        }
                    }
                }
            }
        }
        for fid in hit_ids {
            let mut types = per_file.remove(&fid).unwrap_or_default();
            types.sort();
            types.dedup();
            matches.push(FileMatch {
                file: files
                    .get(fid as usize)
                    .cloned()
                    .unwrap_or_else(|| PathBuf::from(format!("f{fid}"))),
                project: pname.clone(),
                project_path: PathBuf::from(&ppath),
                types,
            });
        }
    }

    Ok(FileResult {
        query: query.to_path_buf(),
        matches,
    })
}

// ---- helpers ----------------------------------------------------------

/// Transient per-method info collected during the main pass, before we know
/// which files we need to re-parse for signatures.
struct MethodInfo {
    name: String,
    path: PathBuf,
    line_start: u32,
    line_end: u32,
    loc: u64,
    cx: u64,
}

fn populate_from_metrics(m: &mut Match, body: &Value, files: &[PathBuf]) -> Vec<MethodInfo> {
    m.loc = body.get("loc").and_then(Value::as_u64);
    m.members = body.get("members").and_then(Value::as_u64);
    m.complexity = body.get("complexity").and_then(Value::as_u64);
    if let Some(bases) = body.get("bases").and_then(Value::as_sequence) {
        m.bases = bases
            .iter()
            .filter_map(|b| b.as_str().map(str::to_string))
            .collect();
    }
    let spans = parse_spans(body);
    let primary_file_id = spans.first().map(|s| s.file_id);
    for sp in &spans {
        m.at.push(format_at(files, sp.file_id, sp.line_start, sp.line_end));
    }
    let mut out = Vec::new();
    if let Some(methods) = body.get("methods").and_then(Value::as_sequence) {
        for mv in methods {
            let Some(s) = mv.as_str() else { continue };
            let Some(pm) = parse_method(s) else { continue };
            let file_id = pm.file_id.or(primary_file_id).unwrap_or(0);
            let path = files
                .get(file_id as usize)
                .cloned()
                .unwrap_or_else(|| PathBuf::from(format!("f{file_id}")));
            out.push(MethodInfo {
                name: pm.name,
                path,
                line_start: pm.line_start,
                line_end: pm.line_end,
                loc: pm.loc,
                cx: pm.cx,
            });
        }
    }
    out
}

/// For each method, prefer the tree-sitter-extracted signature over the bare
/// name. Parses each source file at most once across an entire batch call.
fn format_methods(
    infos: &[MethodInfo],
    repo_root: &Path,
    want_sigs: bool,
    cache: &mut SigCache,
) -> Vec<String> {
    let mut out = Vec::with_capacity(infos.len());
    for mi in infos {
        let display_name = if want_sigs {
            let sigs = cache
                .map
                .entry(mi.path.clone())
                .or_insert_with(|| signatures::extract_signatures(&repo_root.join(&mi.path)));
            sigs.get(&mi.line_start)
                .cloned()
                .unwrap_or_else(|| mi.name.clone())
        } else {
            mi.name.clone()
        };
        out.push(format!(
            "{}  {}:{}-{}  loc={}  cx={}",
            display_name,
            mi.path.display(),
            mi.line_start,
            mi.line_end,
            mi.loc,
            mi.cx
        ));
    }
    out
}

fn repo_root_from(atlas_dir: &Path) -> PathBuf {
    // When atlas_dir is `<root>/.nspect/gen` (the shape `nspect init`
    // writes), pop two levels to reach the repo root. Otherwise fall back to
    // the atlas_dir itself so relative paths still resolve something.
    if atlas_dir.file_name().map(|n| n == "gen").unwrap_or(false) {
        if let Some(parent) = atlas_dir.parent() {
            if parent.file_name().map(|n| n == ".nspect").unwrap_or(false) {
                if let Some(root) = parent.parent() {
                    return root.to_path_buf();
                }
            }
        }
    }
    atlas_dir.to_path_buf()
}

fn format_at(files: &[PathBuf], file_id: u32, line_start: u32, line_end: u32) -> String {
    let path = files
        .get(file_id as usize)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("f{file_id}"));
    format!("{path}:{line_start}-{line_end}")
}

#[derive(Debug, Clone, Copy)]
struct ParsedSpan {
    file_id: u32,
    line_start: u32,
    line_end: u32,
}

fn parse_span(s: &str) -> Option<ParsedSpan> {
    // "f55:16-195"
    let rest = s.strip_prefix('f')?;
    let (fid, range) = rest.split_once(':')?;
    let (start, end) = range.split_once('-')?;
    Some(ParsedSpan {
        file_id: fid.parse().ok()?,
        line_start: start.parse().ok()?,
        line_end: end.parse().ok()?,
    })
}

fn parse_spans(body: &Value) -> Vec<ParsedSpan> {
    body.get("spans")
        .and_then(Value::as_sequence)
        .map(|v| {
            v.iter()
                .filter_map(|x| x.as_str().and_then(parse_span))
                .collect()
        })
        .unwrap_or_default()
}

struct ParsedMethod {
    name: String,
    line_start: u32,
    line_end: u32,
    loc: u64,
    cx: u64,
    file_id: Option<u32>,
}

fn parse_method(s: &str) -> Option<ParsedMethod> {
    // "name L<start>-<end> loc=N cx=N [f=N]"
    let (name, rest) = s.split_once(' ')?;
    let mut line_start = 0u32;
    let mut line_end = 0u32;
    let mut loc = 0u64;
    let mut cx = 0u64;
    let mut file_id: Option<u32> = None;
    for tok in rest.split_whitespace() {
        if let Some(range) = tok.strip_prefix('L') {
            if let Some((a, b)) = range.split_once('-') {
                line_start = a.parse().unwrap_or(0);
                line_end = b.parse().unwrap_or(0);
            }
        } else if let Some(v) = tok.strip_prefix("loc=") {
            loc = v.parse().unwrap_or(0);
        } else if let Some(v) = tok.strip_prefix("cx=") {
            cx = v.parse().unwrap_or(0);
        } else if let Some(v) = tok.strip_prefix("f=") {
            file_id = v.parse().ok();
        }
    }
    Some(ParsedMethod {
        name: name.to_string(),
        line_start,
        line_end,
        loc,
        cx,
        file_id,
    })
}

fn source_files_of(project: &Value) -> Vec<PathBuf> {
    let Some(sf) = project.get("source_files") else {
        return Vec::new();
    };
    // Grouped form (new): mapping of parent_dir -> [basename, ...].
    if let Some(map) = sf.as_mapping() {
        let mut entries: Vec<(String, Vec<String>)> = map
            .iter()
            .filter_map(|(k, v)| {
                let dir = k.as_str()?.to_string();
                let files = v
                    .as_sequence()?
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect();
                Some((dir, files))
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut out = Vec::new();
        for (dir, files) in entries {
            for f in files {
                out.push(if dir == "." || dir.is_empty() {
                    PathBuf::from(&f)
                } else {
                    PathBuf::from(&dir).join(&f)
                });
            }
        }
        return out;
    }
    // Back-compat: flat list form (pre-grouped atlases).
    if let Some(seq) = sf.as_sequence() {
        return seq
            .iter()
            .filter_map(|x| x.as_str().map(PathBuf::from))
            .collect();
    }
    Vec::new()
}

fn load_optional(dir: &Path, name: &str) -> Result<Option<Value>> {
    let path = dir.join(name);
    if !path.exists() {
        return Ok(None);
    }
    let body =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let doc: Value =
        serde_yaml::from_str(&body).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(doc))
}

fn projects_of(doc: &Value) -> &[Value] {
    doc.get("projects")
        .and_then(Value::as_sequence)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn string_at(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str().map(str::to_string))
}

/// An `entries` node can be either a flat list of names (classes.yaml shape)
/// or a mapping of name -> metrics block (metrics.yaml shape).
fn iter_entries(entries: &Value) -> Vec<(String, Option<Value>)> {
    if let Some(seq) = entries.as_sequence() {
        return seq
            .iter()
            .filter_map(|v| v.as_str().map(|s| (s.to_string(), None)))
            .collect();
    }
    if let Some(map) = entries.as_mapping() {
        return map
            .iter()
            .filter_map(|(k, v)| k.as_str().map(|s| (s.to_string(), Some(v.clone()))))
            .collect();
    }
    Vec::new()
}

fn local_matches(local: &str, simple: &str) -> bool {
    local == simple
        || local
            .rsplit('.')
            .next()
            .map(|last| last == simple)
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fixture(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("classes.yaml"),
            r#"projects:
- name: Domain
  path: src/Domain.csproj
  namespaces:
    Acme.Domain:
      class:
      - Customer
      - Order
- name: Web
  path: src/Web.csproj
  namespaces:
    Acme.Web:
      class:
      - Customer
"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("references.yaml"),
            r#"projects:
- name: App
  path: src/App.csproj
  resolved_cross_project:
    Domain:
    - Customer
    - Order
- name: Reporting
  path: src/Reporting.csproj
  ambiguous:
    Customer:
    - Domain
    - Web
"#,
        )
        .unwrap();
    }

    #[test]
    fn simple_name_lookup_with_callers_and_ambiguity() {
        let dir = std::env::temp_dir().join(format!("nspect-lookup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_fixture(&dir);

        let r = run(&dir, "Customer").unwrap();
        assert_eq!(r.matches.len(), 2);
        let by_proj: std::collections::HashMap<_, _> =
            r.matches.iter().map(|m| (m.project.clone(), m)).collect();
        assert!(by_proj["Domain"].referenced_by.iter().any(|c| c == "App"));
        assert!(by_proj["Web"].referenced_by.is_empty());
        assert_eq!(r.ambiguous_in, vec!["Reporting".to_string()]);

        let r = run(&dir, "Acme.Domain.Customer").unwrap();
        assert_eq!(r.matches.len(), 1);
        assert_eq!(r.matches[0].project, "Domain");

        let r = run(&dir, "Nope").unwrap();
        assert!(r.matches.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn metrics_resolves_file_ids_and_method_lines() {
        let dir = std::env::temp_dir().join(format!("nspect-lookup-m-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("metrics.yaml"),
            r#"projects:
- name: Domain
  path: src/Domain.csproj
  source_files:
  - src/Domain/Customer.cs
  - src/Domain/CustomerValidator.cs
  namespaces:
    Acme.Domain:
      class:
        Customer:
          loc: 50
          members: 3
          complexity: 7
          spans:
          - f0:10-80
          methods:
          - Validate L22-40 loc=19 cx=5
          - Save L42-60 loc=19 cx=2
"#,
        )
        .unwrap();

        let r = run(&dir, "Customer").unwrap();
        assert_eq!(r.matches.len(), 1);
        let m = &r.matches[0];
        assert_eq!(m.at, vec!["src/Domain/Customer.cs:10-80".to_string()]);
        assert_eq!(m.loc, Some(50));
        assert_eq!(m.complexity, Some(7));
        assert!(m.methods[0].contains("src/Domain/Customer.cs:22-40"));
        assert!(m.methods[0].starts_with("Validate"));

        let fr = run_file(&dir, Path::new("Customer.cs")).unwrap();
        assert_eq!(fr.matches.len(), 1);
        assert_eq!(fr.matches[0].file, PathBuf::from("src/Domain/Customer.cs"));
        assert_eq!(
            fr.matches[0].types,
            vec!["Acme.Domain.Customer L10-80".to_string()]
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lookup_attaches_endpoint_when_present() {
        let dir = std::env::temp_dir().join(format!("nspect-lookup-ep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("metrics.yaml"),
            r#"projects:
- name: Billing
  path: src/Billing.csproj
  namespaces:
    Acme.Billing:
      interface:
        IInvoiceService:
          loc: 14
          members: 2
          complexity: 0
          spans:
          - f0:8-21
"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("endpoints.yaml"),
            r#"projects:
- name: Billing
  path: src/Billing.csproj
  endpoints:
  - kind: wcf-contract
    type: Acme.Billing.IInvoiceService
    methods: [Submit, Cancel]
    dtos: [InvoiceDto, OperationResult]
    users:
      Acme.Web:
      - InvoicesController
"#,
        )
        .unwrap();

        let r = run(&dir, "IInvoiceService").unwrap();
        assert_eq!(r.matches.len(), 1);
        let ep = r.matches[0].endpoint.as_ref().expect("endpoint attached");
        assert_eq!(ep.get("kind").and_then(Value::as_str), Some("wcf-contract"));
        assert_eq!(
            ep.get("type").and_then(Value::as_str),
            Some("Acme.Billing.IInvoiceService")
        );
        let methods = ep.get("methods").and_then(Value::as_sequence).unwrap();
        assert_eq!(methods.len(), 2);

        // A type that isn't an endpoint stays endpoint-less.
        std::fs::write(
            dir.join("metrics.yaml"),
            r#"projects:
- name: Domain
  path: src/Domain.csproj
  namespaces:
    Acme.Domain:
      class:
        Helper:
          loc: 5
          spans:
          - f0:1-5
"#,
        )
        .unwrap();
        let r = run(&dir, "Helper").unwrap();
        assert_eq!(r.matches.len(), 1);
        assert!(r.matches[0].endpoint.is_none());

        std::fs::remove_dir_all(&dir).ok();
    }
}
