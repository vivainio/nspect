//! Legacy `<bindingRedirect>` cross-checks for `app.config` / `web.config` /
//! `*.exe.config` files.
//!
//! These are purely intra-repo, structural checks — no NuGet restore, no
//! assembly loading, no package-version resolution needed. Same posture as
//! the `PackageReference`/CPM version-conflict pass in `analysis.rs`, just
//! scoped to `.config` files instead of `.csproj`/CPM files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::analysis::{BindingRedirectVersionGroup, Finding};
use crate::graph::ProjectGraph;

#[derive(Debug, Clone)]
pub struct BindingRedirectEntry {
    pub config_path: PathBuf,
    pub assembly_name: String,
    pub public_key_token: Option<String>,
    pub old_version: String,
    pub new_version: String,
}

/// Find `app.config` / `web.config` / `*.exe.config` files sibling to
/// `project_dir` (a project's own directory). Config files are almost
/// always sibling to the `.csproj`/`.vbproj` they belong to, so this stays
/// a narrow, cheap per-project scan rather than a full repo walk.
pub fn discover(project_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(project_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let lower = name.to_lowercase();
        if lower == "app.config" || lower == "web.config" || lower.ends_with(".exe.config") {
            out.push(path);
        }
    }
    out.sort();
    out
}

pub fn parse_file(path: &Path) -> Result<Vec<BindingRedirectEntry>> {
    let text = std::fs::read_to_string(path)?;
    parse_str(&text, path)
}

/// Flat scan for `<dependentAssembly><assemblyIdentity .../><bindingRedirect
/// .../></dependentAssembly>` — no need for a general XML DOM, this is one
/// element shape repeated throughout `<runtime><assemblyBinding>`. Mirrors
/// `cpm.rs`'s `quick_xml::Reader` pattern.
pub fn parse_str(xml: &str, config_path: &Path) -> Result<Vec<BindingRedirectEntry>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out = Vec::new();

    let mut cur_name: Option<String> = None;
    let mut cur_token: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "xml error in {}: {e}",
                    config_path.display()
                ))
            }
            Ok(Event::Eof) => break,
            Ok(Event::Start(e) | Event::Empty(e)) => {
                let name_owned = e.name();
                let local = local_name(name_owned.as_ref());
                if local.eq_ignore_ascii_case("assemblyIdentity") {
                    cur_name = None;
                    cur_token = None;
                    for a in e.attributes().flatten() {
                        let key = a.key.as_ref();
                        let val = String::from_utf8_lossy(&a.value).into_owned();
                        if key.eq_ignore_ascii_case(b"name") {
                            cur_name = Some(val);
                        } else if key.eq_ignore_ascii_case(b"publicKeyToken") {
                            cur_token = Some(val);
                        }
                    }
                } else if local.eq_ignore_ascii_case("bindingRedirect") {
                    let mut old_version = None;
                    let mut new_version = None;
                    for a in e.attributes().flatten() {
                        let key = a.key.as_ref();
                        let val = String::from_utf8_lossy(&a.value).into_owned();
                        if key.eq_ignore_ascii_case(b"oldVersion") {
                            old_version = Some(val);
                        } else if key.eq_ignore_ascii_case(b"newVersion") {
                            new_version = Some(val);
                        }
                    }
                    if let (Some(name), Some(old_version), Some(new_version)) =
                        (cur_name.clone(), old_version, new_version)
                    {
                        out.push(BindingRedirectEntry {
                            config_path: config_path.to_path_buf(),
                            assembly_name: name,
                            public_key_token: cur_token.clone(),
                            old_version,
                            new_version,
                        });
                    }
                }
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

fn local_name(qname: &[u8]) -> &str {
    let s = std::str::from_utf8(qname).unwrap_or("");
    s.rsplit(':').next().unwrap_or(s)
}

/// Split an `oldVersion` attribute into its `(lower, upper)` bounds —
/// `"0.0.0.0-4.0.5.0"` splits on `-`; a bare `"4.0.5.0"` is its own bound
/// on both ends. Shared with [`crate::dotnet_dll`], which checks whether an
/// on-disk `AssemblyRef` version actually falls inside a redirect's range.
pub(crate) fn old_version_bounds(old_version: &str) -> (&str, &str) {
    match old_version.split_once('-') {
        Some((lo, hi)) => (lo.trim(), hi.trim()),
        None => (old_version.trim(), old_version.trim()),
    }
}

/// The upper bound of an `oldVersion` range (`"0.0.0.0-4.0.5.0"` → the part
/// after `-`; a bare `"4.0.5.0"` is its own upper bound).
fn old_version_upper(old_version: &str) -> &str {
    old_version_bounds(old_version).1
}

/// Parse a `System.Version`-style dotted string into a 4-tuple for
/// ordering. Missing trailing parts default to 0. Returns `None` if any
/// present part fails to parse as an integer. Shared with
/// [`crate::dotnet_dll`].
pub(crate) fn parse_version(v: &str) -> Option<(u32, u32, u32, u32)> {
    let mut parts = [0u32; 4];
    for (i, part) in v.trim().split('.').enumerate() {
        if i >= 4 {
            break;
        }
        parts[i] = part.parse().ok()?;
    }
    Some((parts[0], parts[1], parts[2], parts[3]))
}

/// `65535` is `ushort::MaxValue` — NuGet/Visual Studio's "Add Binding
/// Redirect" tooling stamps `oldVersion="0.0.0.0-65535.65535.65535.65535"`
/// as a deliberate catch-all meaning "redirect any version that could ever
/// be requested", not a real version ceiling. Comparing `newVersion`
/// against that sentinel is meaningless and would flag the overwhelming
/// majority of ordinary, auto-generated redirects.
const WILDCARD_UPPER: (u32, u32, u32, u32) = (65535, 65535, 65535, 65535);

/// A redirect must point at or above the top of the range it claims to
/// cover. When `newVersion` is lower, it silently misroutes assembly loads
/// at runtime for versions right at the boundary — exactly why this
/// survives human review — rather than erroring immediately.
pub fn check_inverted(entries: &[BindingRedirectEntry]) -> Vec<Finding> {
    let mut out = Vec::new();
    for e in entries {
        let upper = old_version_upper(&e.old_version);
        let (Some(upper_v), Some(new_v)) = (parse_version(upper), parse_version(&e.new_version))
        else {
            continue;
        };
        if upper_v == WILDCARD_UPPER {
            continue;
        }
        if new_v < upper_v {
            out.push(Finding::BindingRedirectInverted {
                config_path: e.config_path.clone(),
                assembly_name: e.assembly_name.clone(),
                old_version: e.old_version.clone(),
                new_version: e.new_version.clone(),
            });
        }
    }
    out
}

/// Repo-wide: group by `(assembly_name, public_key_token)` and flag when
/// `new_version` isn't uniform across the group. Same posture as
/// `Finding::VersionConflict` — surface the group and let the human judge
/// rather than guessing which side is "right".
pub fn check_inconsistent(entries: &[BindingRedirectEntry]) -> Vec<Finding> {
    let mut by_assembly: BTreeMap<(String, Option<String>), Vec<&BindingRedirectEntry>> =
        BTreeMap::new();
    for e in entries {
        by_assembly
            .entry((e.assembly_name.clone(), e.public_key_token.clone()))
            .or_default()
            .push(e);
    }
    let mut out = Vec::new();
    for ((name, _token), group) in by_assembly {
        let mut distinct: Vec<&str> = group.iter().map(|e| e.new_version.as_str()).collect();
        distinct.sort();
        distinct.dedup();
        if distinct.len() <= 1 {
            continue;
        }
        let mut by_version: BTreeMap<&str, Vec<PathBuf>> = BTreeMap::new();
        for e in &group {
            by_version
                .entry(e.new_version.as_str())
                .or_default()
                .push(e.config_path.clone());
        }
        let mut groups: Vec<BindingRedirectVersionGroup> = by_version
            .into_iter()
            .map(|(ver, mut paths)| {
                paths.sort();
                paths.dedup();
                BindingRedirectVersionGroup {
                    new_version: ver.to_string(),
                    config_paths: paths,
                }
            })
            .collect();
        groups.sort_by(|a, b| {
            b.config_paths
                .len()
                .cmp(&a.config_paths.len())
                .then_with(|| a.new_version.cmp(&b.new_version))
        });
        out.push(Finding::BindingRedirectInconsistent {
            assembly_name: name,
            versions: groups,
        });
    }
    out
}

/// Same `(assembly_name, public_key_token)` appearing more than once in the
/// *same* config file. Flagged even when every occurrence agrees on
/// `new_version` (redundant XML) — `Finding::severity` grades a
/// disagreeing duplicate higher than an agreeing one.
pub fn check_duplicates(entries: &[BindingRedirectEntry]) -> Vec<Finding> {
    let mut by_file: BTreeMap<&PathBuf, Vec<&BindingRedirectEntry>> = BTreeMap::new();
    for e in entries {
        by_file.entry(&e.config_path).or_default().push(e);
    }
    let mut out = Vec::new();
    for (path, group) in by_file {
        let mut seen: BTreeMap<(String, Option<String>), Vec<String>> = BTreeMap::new();
        for e in &group {
            seen.entry((e.assembly_name.clone(), e.public_key_token.clone()))
                .or_default()
                .push(e.new_version.clone());
        }
        for ((name, _token), versions) in seen {
            if versions.len() > 1 {
                out.push(Finding::DuplicateBindingRedirect {
                    config_path: path.clone(),
                    assembly_name: name,
                    versions,
                });
            }
        }
    }
    out
}

/// Discover and parse every `app.config`/`web.config`/`*.exe.config` sibling
/// to each project in `g`. Shared by [`analyze`] and by
/// [`crate::dotnet_dll::find_redirect_causes`], which cross-references
/// these against real on-disk `AssemblyRef`s.
///
/// Reading needs the absolute path, but nothing downstream (findings, report
/// text, `checks.yaml`) benefits from an absolute path — repo-relative to
/// `scan_root` is shorter, portable across machines, and (on Windows) sidesteps
/// `std::fs::canonicalize`'s `\\?\`-prefixed "verbatim" paths entirely, since
/// a relative path never carries one. `config_path` is relativized once here
/// rather than at each display site.
pub fn collect_entries(g: &ProjectGraph, scan_root: &Path) -> Vec<BindingRedirectEntry> {
    let scan_root = crate::csproj::canonicalize(scan_root);
    let mut config_files: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    for project in g.projects.values() {
        let Some(dir) = project.path.parent() else {
            continue;
        };
        for cfg in discover(dir) {
            config_files.insert(cfg);
        }
    }

    let mut entries = Vec::new();
    for cfg in &config_files {
        match parse_file(cfg) {
            Ok(mut es) => entries.append(&mut es),
            Err(e) => tracing::warn!("skipping {}: {e:#}", cfg.display()),
        }
    }
    for e in &mut entries {
        e.config_path = relativize(&e.config_path, &scan_root);
    }
    entries
}

fn relativize(path: &Path, root: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Discover and parse every `app.config`/`web.config`/`*.exe.config` sibling
/// to each project in `g`, then run all binding-redirect checks over the
/// combined entry list.
pub fn analyze(g: &ProjectGraph, scan_root: &Path) -> Vec<Finding> {
    let entries = collect_entries(g, scan_root);
    let mut out = check_inverted(&entries);
    out.extend(check_inconsistent(&entries));
    out.extend(check_duplicates(&entries));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(xml: &str) -> Vec<BindingRedirectEntry> {
        parse_str(xml, Path::new("app.config")).unwrap()
    }

    const WRAP_OPEN: &str = r#"<configuration><runtime><assemblyBinding xmlns="urn:schemas-microsoft-com:asm.v1"><dependentAssembly>"#;
    const WRAP_CLOSE: &str = r#"</dependentAssembly></assemblyBinding></runtime></configuration>"#;

    #[test]
    fn parses_binding_redirect() {
        let xml = format!(
            r#"{WRAP_OPEN}
    <assemblyIdentity name="System.ValueTuple" publicKeyToken="cc7b13ffcd2ddd51" culture="neutral" />
    <bindingRedirect oldVersion="0.0.0.0-4.0.3.0" newVersion="4.0.3.0" />
{WRAP_CLOSE}"#
        );
        let entries = parse(&xml);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].assembly_name, "System.ValueTuple");
        assert_eq!(
            entries[0].public_key_token.as_deref(),
            Some("cc7b13ffcd2ddd51")
        );
        assert_eq!(entries[0].old_version, "0.0.0.0-4.0.3.0");
        assert_eq!(entries[0].new_version, "4.0.3.0");
    }

    #[test]
    fn detects_inverted_redirect() {
        let entries = vec![BindingRedirectEntry {
            config_path: PathBuf::from("app.config"),
            assembly_name: "System.ValueTuple".into(),
            public_key_token: None,
            old_version: "0.0.0.0-4.0.5.0".into(),
            new_version: "4.0.0.0".into(),
        }];
        let findings = check_inverted(&entries);
        assert_eq!(findings.len(), 1);
        assert!(matches!(
            findings[0],
            Finding::BindingRedirectInverted { .. }
        ));
    }

    #[test]
    fn ignores_wildcard_catch_all_redirect() {
        // The standard NuGet/VS "Add Binding Redirect" catch-all: oldVersion's
        // upper bound is ushort::MAX in every component, not a real ceiling.
        let entries = vec![BindingRedirectEntry {
            config_path: PathBuf::from("app.config"),
            assembly_name: "Microsoft.Owin".into(),
            public_key_token: None,
            old_version: "0.0.0.0-65535.65535.65535.65535".into(),
            new_version: "4.2.2.0".into(),
        }];
        assert!(check_inverted(&entries).is_empty());
    }

    #[test]
    fn ignores_valid_redirect() {
        let entries = vec![BindingRedirectEntry {
            config_path: PathBuf::from("app.config"),
            assembly_name: "System.ValueTuple".into(),
            public_key_token: None,
            old_version: "0.0.0.0-4.0.5.0".into(),
            new_version: "4.0.5.0".into(),
        }];
        assert!(check_inverted(&entries).is_empty());
    }

    #[test]
    fn detects_inconsistent_across_files() {
        let entries = vec![
            BindingRedirectEntry {
                config_path: PathBuf::from("A/app.config"),
                assembly_name: "System.Threading.Tasks.Extensions".into(),
                public_key_token: None,
                old_version: "0.0.0.0-4.2.4.0".into(),
                new_version: "4.2.1.0".into(),
            },
            BindingRedirectEntry {
                config_path: PathBuf::from("B/app.config"),
                assembly_name: "System.Threading.Tasks.Extensions".into(),
                public_key_token: None,
                old_version: "0.0.0.0-4.2.4.0".into(),
                new_version: "4.2.4.0".into(),
            },
        ];
        let findings = check_inconsistent(&entries);
        assert_eq!(findings.len(), 1);
        assert!(matches!(
            findings[0],
            Finding::BindingRedirectInconsistent { .. }
        ));
    }

    #[test]
    fn detects_duplicate_in_same_file() {
        let entries = vec![
            BindingRedirectEntry {
                config_path: PathBuf::from("app.config"),
                assembly_name: "System.Collections.Immutable".into(),
                public_key_token: None,
                old_version: "0.0.0.0-10.0.0.0".into(),
                new_version: "10.0.0.0".into(),
            },
            BindingRedirectEntry {
                config_path: PathBuf::from("app.config"),
                assembly_name: "System.Collections.Immutable".into(),
                public_key_token: None,
                old_version: "0.0.0.0-10.0.0.5".into(),
                new_version: "10.0.0.5".into(),
            },
        ];
        let findings = check_duplicates(&entries);
        assert_eq!(findings.len(), 1);
        assert!(matches!(
            findings[0],
            Finding::DuplicateBindingRedirect { .. }
        ));
    }
}
