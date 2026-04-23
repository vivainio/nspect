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
fn source_scan_extracts_usings_and_drives_analysis() {
    let mut projects = nspect::cli::load_projects(&fixture("sourcescan")).unwrap();
    nspect::cli::apply_source_scan(&mut projects).unwrap();

    let app = projects.iter().find(|p| p.name == "App").unwrap();
    assert!(app.usings.contains(&"Serilog".to_string()));
    assert!(app.usings.contains(&"Newtonsoft.Json".to_string()));
    assert!(app.usings.contains(&"System".to_string()));

    let g = ProjectGraph::build(projects);
    let findings = analysis::analyze(&g);

    // Dapper is declared but never used.
    assert!(
        findings.iter().any(|f| matches!(
            f,
            Finding::UnusedPackageRef { project, package } if project == "App" && package == "Dapper"
        )),
        "expected unused Dapper finding, got {findings:?}"
    );
    // Newtonsoft.Json is used but not declared.
    assert!(
        findings.iter().any(|f| matches!(
            f,
            Finding::UndeclaredUsage { project, namespace } if project == "App" && namespace == "Newtonsoft.Json"
        )),
        "expected undeclared Newtonsoft.Json finding, got {findings:?}"
    );
    // Serilog is declared AND used — no unused finding for it.
    assert!(!findings.iter().any(|f| matches!(
        f,
        Finding::UnusedPackageRef { package, .. } if package == "Serilog"
    )));
    // System.* must not appear as undeclared.
    assert!(!findings.iter().any(|f| matches!(
        f,
        Finding::UndeclaredUsage { namespace, .. } if namespace.starts_with("System")
    )));
}
