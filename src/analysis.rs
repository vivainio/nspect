use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;

use crate::graph::ProjectGraph;
use crate::model::ProjectId;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Finding {
    Cycle {
        projects: Vec<String>,
    },
    OrphanProject {
        project: String,
    },
    UnresolvedProjectRef {
        project: String,
        target: PathBuf,
    },
    VersionConflict {
        package: String,
        versions: Vec<(String, String)>, // (project name, version)
    },
    UnusedPackageRef {
        project: String,
        package: String,
    },
    UndeclaredUsage {
        project: String,
        namespace: String,
    },
}

/// Grouped view of `Vec<Finding>` for `checks.yaml` / `atlas.findings` —
/// collapses the repetitive `kind:` tag by bucketing each variant into its
/// own field. Empty sections are omitted.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ChecksReport {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cycles: Vec<Vec<String>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub orphan_projects: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unresolved_project_refs: Vec<UnresolvedRefEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub version_conflicts: Vec<VersionConflictEntry>,
    /// `{project: [package, ...]}`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub unused_package_refs: BTreeMap<String, Vec<String>>,
    /// `{project: [namespace, ...]}`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub undeclared_usages: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnresolvedRefEntry {
    pub project: String,
    pub target: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct VersionConflictEntry {
    pub package: String,
    pub versions: Vec<(String, String)>,
}

impl ChecksReport {
    pub fn from_findings(findings: &[Finding]) -> Self {
        let mut out = ChecksReport::default();
        for f in findings {
            match f {
                Finding::Cycle { projects } => out.cycles.push(projects.clone()),
                Finding::OrphanProject { project } => out.orphan_projects.push(project.clone()),
                Finding::UnresolvedProjectRef { project, target } => {
                    out.unresolved_project_refs.push(UnresolvedRefEntry {
                        project: project.clone(),
                        target: target.clone(),
                    })
                }
                Finding::VersionConflict { package, versions } => {
                    out.version_conflicts.push(VersionConflictEntry {
                        package: package.clone(),
                        versions: versions.clone(),
                    })
                }
                Finding::UnusedPackageRef { project, package } => {
                    out.unused_package_refs
                        .entry(project.clone())
                        .or_default()
                        .push(package.clone());
                }
                Finding::UndeclaredUsage { project, namespace } => {
                    out.undeclared_usages
                        .entry(project.clone())
                        .or_default()
                        .push(namespace.clone());
                }
            }
        }
        out.orphan_projects.sort();
        out.orphan_projects.dedup();
        for v in out.unused_package_refs.values_mut() {
            v.sort();
            v.dedup();
        }
        for v in out.undeclared_usages.values_mut() {
            v.sort();
            v.dedup();
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.cycles.is_empty()
            && self.orphan_projects.is_empty()
            && self.unresolved_project_refs.is_empty()
            && self.version_conflicts.is_empty()
            && self.unused_package_refs.is_empty()
            && self.undeclared_usages.is_empty()
    }
}

impl Finding {
    pub fn severity(&self) -> Severity {
        match self {
            Finding::Cycle { .. } => Severity::Error,
            Finding::VersionConflict { .. } => Severity::Error,
            Finding::UnresolvedProjectRef { .. } => Severity::Warning,
            Finding::UnusedPackageRef { .. } => Severity::Warning,
            Finding::UndeclaredUsage { .. } => Severity::Warning,
            Finding::OrphanProject { .. } => Severity::Info,
        }
    }
}

/// Known mappings from NuGet package name → namespaces the package surfaces.
/// Most of the time the package name IS the root namespace; this table captures
/// the common exceptions.
fn package_namespaces(pkg: &str) -> Vec<&'static str> {
    match pkg {
        "Newtonsoft.Json" => vec!["Newtonsoft.Json"],
        "Serilog" => vec!["Serilog"],
        "Serilog.AspNetCore" => vec!["Serilog"],
        "Microsoft.Extensions.DependencyInjection" => {
            vec!["Microsoft.Extensions.DependencyInjection"]
        }
        "Microsoft.Extensions.Logging" => vec!["Microsoft.Extensions.Logging"],
        "Microsoft.Extensions.Configuration" => vec!["Microsoft.Extensions.Configuration"],
        "Microsoft.Extensions.Hosting" => vec!["Microsoft.Extensions.Hosting"],
        "Microsoft.EntityFrameworkCore" => vec!["Microsoft.EntityFrameworkCore"],
        "AutoMapper" => vec!["AutoMapper"],
        "FluentValidation" => vec!["FluentValidation"],
        "MediatR" => vec!["MediatR"],
        "Dapper" => vec!["Dapper"],
        "NUnit" => vec!["NUnit.Framework"],
        "NUnit3TestAdapter" => vec!["NUnit.Framework"],
        "xunit" => vec!["Xunit"],
        "xunit.core" => vec!["Xunit"],
        "MSTest.TestFramework" => vec!["Microsoft.VisualStudio.TestTools.UnitTesting"],
        "Moq" => vec!["Moq"],
        _ => Vec::new(),
    }
}

/// Namespace prefixes considered "always available" — part of the BCL, the SDK, or
/// routinely pulled in by project types (e.g. test SDKs auto-import xunit helpers).
/// These are neither flagged as undeclared nor required to match a declared package.
const BUILTIN_NAMESPACE_PREFIXES: &[&str] = &[
    "System",
    "Microsoft.CSharp",
    "Microsoft.VisualBasic",
    "Microsoft.Win32",
    "Windows",
    "Internal",
];

fn is_builtin(ns: &str) -> bool {
    BUILTIN_NAMESPACE_PREFIXES
        .iter()
        .any(|p| ns == *p || ns.starts_with(&format!("{p}.")))
}

/// True if `using_ns` should be considered "served" by `pkg_ns`
/// (prefix match on namespace segments).
fn ns_matches(using_ns: &str, pkg_ns: &str) -> bool {
    using_ns == pkg_ns || using_ns.starts_with(&format!("{pkg_ns}."))
}

/// Packages that deliberately don't appear in `using` directives — test runners,
/// analyzers, loggers, and runtime shims — and so shouldn't be flagged as unused.
fn is_tool_package(pkg: &crate::model::PackageRef) -> bool {
    if pkg
        .private_assets
        .as_deref()
        .map(|s| s.eq_ignore_ascii_case("all"))
        .unwrap_or(false)
    {
        return true;
    }
    let name = pkg.name.as_str();
    matches!(
        name,
        "Microsoft.NET.Test.Sdk"
            | "MSTest.TestAdapter"
            | "NUnit3TestAdapter"
            | "xunit.runner.visualstudio"
            | "xunit.runner.console"
            | "NunitXml.TestLogger"
            | "JUnitTestLogger"
            | "coverlet.collector"
            | "coverlet.msbuild"
            | "ReportGenerator"
    ) || name.starts_with("Microsoft.CodeAnalysis.")
        || name.starts_with("StyleCop.")
        || (name.starts_with("System.") && is_runtime_shim(name))
}

/// `System.*` runtime compatibility packages (.NET Framework back-ports) that don't
/// require an explicit `using` because they extend the BCL in place.
fn is_runtime_shim(name: &str) -> bool {
    matches!(
        name,
        "System.Buffers"
            | "System.Memory"
            | "System.Numerics.Vectors"
            | "System.Reflection.Metadata"
            | "System.Runtime.CompilerServices.Unsafe"
            | "System.Threading.Tasks.Extensions"
            | "System.ValueTuple"
            | "System.Collections.Immutable"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

pub fn analyze(g: &ProjectGraph) -> Vec<Finding> {
    let mut out = Vec::new();

    for cycle in g.cycles() {
        out.push(Finding::Cycle {
            projects: cycle.into_iter().map(|id| g.name(id).to_string()).collect(),
        });
    }

    for id in g.orphans() {
        out.push(Finding::OrphanProject {
            project: g.name(id).to_string(),
        });
    }

    for u in &g.unresolved {
        out.push(Finding::UnresolvedProjectRef {
            project: g.name(u.from).to_string(),
            target: u.target.clone(),
        });
    }

    // Version conflicts: for each package, collect the set of distinct versions.
    let mut versions_by_pkg: BTreeMap<&str, BTreeMap<&str, Vec<ProjectId>>> = BTreeMap::new();
    for project in g.projects.values() {
        for pkg in &project.package_refs {
            let Some(v) = pkg.version.as_deref() else {
                continue;
            };
            versions_by_pkg
                .entry(pkg.name.as_str())
                .or_default()
                .entry(v)
                .or_default()
                .push(project.id);
        }
    }
    // Unused/undeclared package analysis per project, using the scanned `usings`.
    for project in g.projects.values() {
        // Skip if no source scan was performed for this project.
        if project.usings.is_empty() && project.package_refs.is_empty() {
            continue;
        }

        let declared: Vec<(&str, Vec<&str>)> = project
            .package_refs
            .iter()
            .map(|p| {
                let mapped = package_namespaces(&p.name);
                if mapped.is_empty() {
                    (p.name.as_str(), vec![p.name.as_str()])
                } else {
                    (p.name.as_str(), mapped)
                }
            })
            .collect();

        // Unused packages: declared but no `using` prefix-matches any of its namespaces.
        // Skip build/tool packages (PrivateAssets=all, test SDKs, analyzers) — by design
        // they do not appear in `using`s.
        if !project.usings.is_empty() {
            for pkg in &project.package_refs {
                if is_tool_package(pkg) {
                    continue;
                }
                let namespaces = package_namespaces(&pkg.name);
                let candidates: Vec<&str> = if namespaces.is_empty() {
                    vec![pkg.name.as_str()]
                } else {
                    namespaces
                };
                let used = project
                    .usings
                    .iter()
                    .any(|u| candidates.iter().any(|ns| ns_matches(u, ns)));
                if !used {
                    out.push(Finding::UnusedPackageRef {
                        project: project.name.clone(),
                        package: pkg.name.clone(),
                    });
                }
            }
        }

        // Undeclared usages: a `using` that doesn't match any declared package
        // namespace, isn't a BCL namespace, and doesn't correspond to a project ref.
        let project_ref_names: std::collections::HashSet<&str> = project
            .project_refs
            .iter()
            .filter_map(|p| p.file_stem().and_then(|s| s.to_str()))
            .collect();

        // Legacy `<Reference Include="Foo.Bar">` entries bind to a DLL directly; a
        // `using Foo.Bar;` is covered by them just as much as by a PackageReference.
        // Strip off any ", Version=..." culture/token metadata from the assembly name.
        let assembly_ref_names: Vec<&str> = project
            .assembly_refs
            .iter()
            .map(|a| a.split(',').next().unwrap_or(a).trim())
            .collect();

        for u in &project.usings {
            if is_builtin(u) {
                continue;
            }
            if project_ref_names.iter().any(|n| ns_matches(u, n)) {
                continue;
            }
            if assembly_ref_names.iter().any(|a| ns_matches(u, a)) {
                continue;
            }
            let matched = declared
                .iter()
                .any(|(_, namespaces)| namespaces.iter().any(|ns| ns_matches(u, ns)));
            if !matched {
                out.push(Finding::UndeclaredUsage {
                    project: project.name.clone(),
                    namespace: u.clone(),
                });
            }
        }
    }

    for (pkg, versions) in versions_by_pkg {
        if versions.len() <= 1 {
            continue;
        }
        let mut entries: Vec<(String, String)> = Vec::new();
        for (ver, projects) in &versions {
            for pid in projects {
                entries.push((g.name(*pid).to_string(), (*ver).to_string()));
            }
        }
        entries.sort();
        out.push(Finding::VersionConflict {
            package: pkg.to_string(),
            versions: entries,
        });
    }

    out
}
