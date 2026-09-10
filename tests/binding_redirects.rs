use std::path::PathBuf;

use nspect::analysis::Finding;
use nspect::binding_redirects;
use nspect::graph::ProjectGraph;

fn fixtures_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures/bindingredirects");
    p
}

fn fixture(name: &str) -> PathBuf {
    fixtures_root().join(name)
}

#[test]
fn detects_inverted_redirect() {
    let projects = nspect::cli::load_projects(&fixture("Inverted")).unwrap();
    let g = ProjectGraph::build(projects);
    let findings = binding_redirects::analyze(&g, &fixture("Inverted"));
    assert!(findings
        .iter()
        .any(|f| matches!(f, Finding::BindingRedirectInverted { assembly_name, .. } if assembly_name == "System.ValueTuple")));
}

#[test]
fn detects_inconsistent_redirect_across_files() {
    let mut a = nspect::cli::load_projects(&fixture("IncA")).unwrap();
    let b = nspect::cli::load_projects(&fixture("IncB")).unwrap();
    a.extend(b);
    let g = ProjectGraph::build(a);
    let findings = binding_redirects::analyze(&g, &fixtures_root());
    let inconsistent = findings.iter().find(|f| {
        matches!(f, Finding::BindingRedirectInconsistent { assembly_name, .. } if assembly_name == "System.Threading.Tasks.Extensions")
    });
    assert!(inconsistent.is_some(), "expected an inconsistency finding");
    if let Some(Finding::BindingRedirectInconsistent { versions, .. }) = inconsistent {
        assert_eq!(versions.len(), 2);
    }
}

#[test]
fn detects_duplicate_redirect_in_same_file() {
    let projects = nspect::cli::load_projects(&fixture("Dup")).unwrap();
    let g = ProjectGraph::build(projects);
    let findings = binding_redirects::analyze(&g, &fixture("Dup"));
    let dup = findings.iter().find(|f| {
        matches!(f, Finding::DuplicateBindingRedirect { assembly_name, .. } if assembly_name == "System.Collections.Immutable")
    });
    assert!(dup.is_some(), "expected a duplicate finding");
    if let Some(f) = dup {
        assert_eq!(f.severity(), nspect::analysis::Severity::Error);
    }
}

#[test]
fn clean_fixture_has_no_findings() {
    let projects = nspect::cli::load_projects(&fixture("Clean")).unwrap();
    let g = ProjectGraph::build(projects);
    let findings = binding_redirects::analyze(&g, &fixture("Clean"));
    assert!(findings.is_empty(), "unexpected findings: {findings:?}");
}
