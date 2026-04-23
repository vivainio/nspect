use std::path::PathBuf;

use nspect::analysis::{self, Finding};
use nspect::graph::ProjectGraph;

fn fixture(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures");
    p.push(name);
    p
}

#[test]
fn cpm_resolves_version_less_package_refs() {
    let projects = nspect::cli::load_projects(&fixture("cpm")).unwrap();
    let app = projects.iter().find(|p| p.name == "App").unwrap();
    assert_eq!(app.package_refs.len(), 2);
    for pkg in &app.package_refs {
        assert!(
            pkg.version.is_some(),
            "{} should be resolved via CPM",
            pkg.name
        );
    }
}

#[test]
fn detects_version_conflict() {
    let projects = nspect::cli::load_projects(&fixture("conflict")).unwrap();
    let g = ProjectGraph::build(projects);
    let findings = analysis::analyze(&g);
    let conflict = findings
        .iter()
        .find(|f| matches!(f, Finding::VersionConflict { package, .. } if package == "Newtonsoft.Json"))
        .expect("version conflict finding");
    if let Finding::VersionConflict { versions, .. } = conflict {
        assert_eq!(versions.len(), 2);
    }
}

#[test]
fn cycle_fixture_produces_cycle_finding() {
    let projects = nspect::cli::load_projects(&fixture("cycle")).unwrap();
    let g = ProjectGraph::build(projects);
    let findings = analysis::analyze(&g);
    assert!(findings.iter().any(|f| matches!(f, Finding::Cycle { .. })));
}

#[test]
fn multi_fixture_has_no_error_findings() {
    let projects = nspect::cli::load_projects(&fixture("multi")).unwrap();
    let g = ProjectGraph::build(projects);
    let findings = analysis::analyze(&g);
    assert!(
        findings
            .iter()
            .all(|f| f.severity() != analysis::Severity::Error),
        "unexpected errors: {findings:?}"
    );
}
