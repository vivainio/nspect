use std::path::PathBuf;

use nspect::analysis::{self, Finding};
use nspect::graph::ProjectGraph;
use nspect::model::TypeKind;

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

#[test]
fn source_scan_extracts_types_namespaces_and_metrics() {
    let mut projects = nspect::cli::load_projects(&fixture("sourcescan")).unwrap();
    nspect::cli::apply_source_scan(&mut projects).unwrap();

    let app = projects.iter().find(|p| p.name == "App").unwrap();

    // Namespace and type declarations surface on Project.
    assert!(app.declared_namespaces.contains(&"App".to_string()));
    let classes = app.declared_types.get(&TypeKind::Class).unwrap();
    assert!(classes.contains(&"App.Program".to_string()));

    // Per-type metrics include the fixture's Program class.
    let program = app
        .type_metrics
        .get("App.Program")
        .expect("App.Program metrics");
    assert!(program.loc > 0);
    assert_eq!(program.members, 1, "expected one direct member (Main)");
    // Per-method breakdown surfaces too.
    let main = program
        .methods
        .iter()
        .find(|m| m.name == "Main")
        .expect("Main method");
    assert!(main.loc > 0);
}

#[test]
fn classes_snapshot_groups_by_namespace() {
    let root = fixture("sourcescan");
    let mut projects = nspect::cli::load_projects(&root).unwrap();
    nspect::cli::apply_source_scan(&mut projects).unwrap();

    let snap = nspect::classes::build(&projects, &root);
    let app = snap.projects.iter().find(|p| p.name == "App").unwrap();
    let ns = app
        .namespaces
        .get("App")
        .expect("App namespace bucket");
    let classes = ns.get(&TypeKind::Class).expect("class bucket");
    // The prefix is stripped, leaving just the local name.
    assert!(classes.contains(&"Program".to_string()));
}

#[test]
fn metrics_snapshot_has_totals_and_methods() {
    let root = fixture("sourcescan");
    let mut projects = nspect::cli::load_projects(&root).unwrap();
    nspect::cli::apply_source_scan(&mut projects).unwrap();

    let snap = nspect::metrics::build(&projects, &root);
    let app = snap.projects.iter().find(|p| p.name == "App").unwrap();
    assert!(app.totals.types >= 1);
    assert!(app.totals.loc > 0);

    // Walk into namespace -> kind -> local name to find Program's method list.
    let program = app
        .namespaces
        .get("App")
        .and_then(|kinds| kinds.get(&TypeKind::Class))
        .and_then(|by_name| by_name.get("Program"))
        .expect("App.Program metrics in snapshot");
    assert!(program.methods.iter().any(|m| m.name == "Main"));
}
