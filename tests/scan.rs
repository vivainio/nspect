use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures");
    p.push(name);
    p
}

#[test]
fn scans_multi_project_solution() {
    let root = fixture("multi");
    let projects = nspect::cli::load_projects(&root).expect("load");

    assert_eq!(projects.len(), 2);
    let alpha = projects.iter().find(|p| p.name == "Alpha").unwrap();
    let beta = projects.iter().find(|p| p.name == "Beta").unwrap();

    assert!(alpha.sdk_style);
    assert_eq!(alpha.target_frameworks, vec!["net8.0"]);
    assert_eq!(alpha.package_refs.len(), 1);
    assert_eq!(alpha.package_refs[0].name, "Serilog");
    assert_eq!(alpha.project_refs.len(), 1);

    assert_eq!(beta.target_frameworks, vec!["net8.0", "netstandard2.0"]);
    assert_eq!(beta.package_refs[0].name, "Newtonsoft.Json");
}

#[test]
fn scans_single_csproj() {
    let mut p = fixture("multi");
    p.push("src/Beta/Beta.csproj");
    let projects = nspect::cli::load_projects(&p).expect("load");
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].name, "Beta");
}
