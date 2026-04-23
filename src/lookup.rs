//! `atlas lookup` — resolve what the atlas bundle knows about a type name.
//!
//! Consumes the YAML artifacts produced by `atlas --output-dir` (typically
//! `classes.yaml`, `metrics.yaml`, `references.yaml`) and collates per-match
//! info: declaring project, namespace, kind, per-type metrics, and which
//! other projects reference the type.
//!
//! Accepts either a simple name (matches every namespace/project declaring
//! that name) or a fully-qualified name (exact match).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_yaml::Value;

#[derive(Debug, Serialize)]
pub struct LookupResult {
    pub query: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matches: Vec<Match>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ambiguous_in: Vec<String>,
    /// Types whose `base_list` names the query. Grouped by declaring
    /// project. Drawn from `metrics.yaml`, which is the only artifact that
    /// records per-type base lists.
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub referenced_by: Vec<String>,
}

pub fn run(atlas_dir: &Path, query: &str) -> Result<LookupResult> {
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
    let has_metrics = metrics_doc.is_some();

    let (simple, fqn_filter) = match query.rsplit_once('.') {
        Some((ns, name)) => (name.to_string(), Some((ns.to_string(), name.to_string()))),
        None => (query.to_string(), None),
    };

    let mut matches: Vec<Match> = Vec::new();
    for project in projects_of(decls) {
        let pname = string_at(project, "name").unwrap_or_default();
        let ppath = string_at(project, "path").unwrap_or_default();
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
                    if local_matches(&local, &simple) {
                        let fqn = if ns == "<global>" || ns.is_empty() {
                            local.clone()
                        } else {
                            format!("{ns}.{local}")
                        };
                        if let Some((want_ns, want_name)) = &fqn_filter {
                            if &ns != want_ns || !local.ends_with(want_name) {
                                continue;
                            }
                        }
                        matches.push(Match {
                            fqn,
                            kind: kind.clone(),
                            project: pname.clone(),
                            project_path: PathBuf::from(&ppath),
                            namespace: if ns == "<global>" {
                                String::new()
                            } else {
                                ns.clone()
                            },
                            metrics: if has_metrics { metrics_val } else { None },
                            referenced_by: Vec::new(),
                        });
                    }
                }
            }
        }
    }

    // Reverse-lookup callers via references.yaml.
    let mut ambiguous_in: Vec<String> = Vec::new();
    if let Some(refs) = &references_doc {
        for project in projects_of(refs) {
            let caller = string_at(project, "name").unwrap_or_default();
            // resolved_cross_project: {declaring_project: [simple_names]}
            if let Some(xp) = project.get("resolved_cross_project").and_then(Value::as_mapping) {
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
            // ambiguous: {simple_name: [projects]}
            if let Some(amb) = project.get("ambiguous").and_then(Value::as_mapping) {
                if let Some(list) = amb.get(Value::from(simple.clone())).and_then(Value::as_sequence) {
                    for p in list {
                        if p.as_str().is_some() {
                            ambiguous_in.push(caller.clone());
                            break;
                        }
                    }
                }
            }
        }
    }

    for m in matches.iter_mut() {
        m.referenced_by.sort();
        m.referenced_by.dedup();
    }
    ambiguous_in.sort();
    ambiguous_in.dedup();

    // Subclasses: scan metrics.yaml for any type whose `bases` list contains
    // the query's simple name. Only works when metrics.yaml is present —
    // classes.yaml doesn't carry base lists.
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

    Ok(LookupResult {
        query: query.to_string(),
        matches,
        ambiguous_in,
        subclasses,
    })
}

fn load_optional(dir: &Path, name: &str) -> Result<Option<Value>> {
    let path = dir.join(name);
    if !path.exists() {
        return Ok(None);
    }
    let body =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let doc: Value = serde_yaml::from_str(&body)
        .with_context(|| format!("parsing {}", path.display()))?;
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

/// Match a declared local name (possibly a nested-type dotted path like
/// `Outer.Inner`) against the query's simple name. Matches against the
/// outermost declared segment — nested types are reported via their dotted
/// local path, and we only want to match the "real" type name of each row.
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
        assert!(by_proj["Domain"]
            .referenced_by
            .iter()
            .any(|c| c == "App"));
        // Web's Customer has no direct caller in this fixture.
        assert!(by_proj["Web"].referenced_by.is_empty());
        assert_eq!(r.ambiguous_in, vec!["Reporting".to_string()]);

        // FQN narrows to one.
        let r = run(&dir, "Acme.Domain.Customer").unwrap();
        assert_eq!(r.matches.len(), 1);
        assert_eq!(r.matches[0].project, "Domain");

        // Nothing matched.
        let r = run(&dir, "Nope").unwrap();
        assert!(r.matches.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }
}
