use std::path::PathBuf;

use nspect::graph::ProjectGraph;

fn fixture(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures");
    p.push(name);
    p
}

#[test]
fn multi_graph_has_edge() {
    let projects = nspect::cli::load_projects(&fixture("multi")).unwrap();
    let g = ProjectGraph::build(projects);
    // Default: project-only graph. Alpha → Beta is the only edge; packages are excluded.
    assert_eq!(g.graph.edge_count(), 1);
    assert!(g.package_nodes.is_empty());

    let g_with = ProjectGraph::build_with_packages(
        nspect::cli::load_projects(&fixture("multi")).unwrap(),
    );
    assert_eq!(g_with.package_nodes.len(), 2);
    assert_eq!(g_with.graph.edge_count(), 3);

    assert!(g.cycles().is_empty());
    assert!(g.orphans().is_empty());
    assert!(g.unresolved.is_empty());
}

#[test]
fn cycle_fixture_detects_cycle() {
    let projects = nspect::cli::load_projects(&fixture("cycle")).unwrap();
    let g = ProjectGraph::build(projects);
    let cycles = g.cycles();
    assert_eq!(cycles.len(), 1);
    assert_eq!(cycles[0].len(), 2);
}

#[test]
fn dot_output_contains_nodes() {
    let projects = nspect::cli::load_projects(&fixture("multi")).unwrap();
    let g = ProjectGraph::build(projects);
    let dot = g.to_dot();
    assert!(dot.contains("digraph projects"));
    assert!(dot.contains("Alpha"));
    assert!(dot.contains("Beta"));
    assert!(dot.contains(" -> "));
}
