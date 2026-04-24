use std::path::PathBuf;

use nspect::atlas;

fn fixture(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures");
    p.push(name);
    p
}

fn build_atlas(name: &str) -> atlas::Atlas {
    let root = fixture(name);
    let projects = nspect::cli::load_projects(&root).expect("load");
    atlas::build(projects, &root, atlas::AtlasOptions::default())
}

#[test]
fn projects_are_loaded_from_all_source_trees() {
    let a = build_atlas("atlas");
    let names: Vec<&str> = a.projects.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(a.projects.len(), 7, "got {names:?}");
    for expected in [
        "Core",
        "Utils",
        "Domain",
        "Api",
        "Ui",
        "Domain.Tests",
        "Widget",
    ] {
        assert!(names.contains(&expected), "missing {expected} in {names:?}");
    }
}

#[test]
fn areas_are_derived_from_path_after_src() {
    let a = build_atlas("atlas");
    let by_name: std::collections::HashMap<&str, &atlas::AtlasProject> =
        a.projects.iter().map(|p| (p.name.as_str(), p)).collect();
    // Src/Common/* → "Common"; Src/Billing/* → "Billing"; Legacy/Widget → "Legacy".
    assert_eq!(by_name["Core"].area, "Common");
    assert_eq!(by_name["Utils"].area, "Common");
    assert_eq!(by_name["Domain"].area, "Billing");
    assert_eq!(by_name["Api"].area, "Billing");
    assert_eq!(by_name["Ui"].area, "Billing");
    assert_eq!(by_name["Domain.Tests"].area, "Billing");
    assert_eq!(by_name["Widget"].area, "Legacy");

    let areas: Vec<&str> = a.areas.iter().map(|ar| ar.name.as_str()).collect();
    assert_eq!(areas, vec!["Billing", "Common", "Legacy"]);
}

#[test]
fn legacy_reference_resolves_to_sibling_project() {
    // Widget uses bare `<Reference Include="Core">` (legacy .NET Fx idiom).
    // Atlas must treat that as a project-ref, not an external ref.
    let a = build_atlas("atlas");
    let widget = a.projects.iter().find(|p| p.name == "Widget").unwrap();
    assert_eq!(
        widget.fan_out, 1,
        "Widget should have 1 resolved project ref"
    );
    assert_eq!(widget.project_refs, vec!["Core".to_string()]);
    // System stays as an external ref, Core does not appear there.
    let ext_names: Vec<&str> = widget
        .refs
        .iter()
        .map(|r| match r {
            atlas::AtlasRef::Bare(n) => n.as_str(),
            atlas::AtlasRef::Versioned { name, .. } => name.as_str(),
        })
        .collect();
    assert!(ext_names.contains(&"System"));
    assert!(!ext_names.contains(&"Core"));
}

#[test]
fn layers_follow_dependency_depth() {
    let a = build_atlas("atlas");
    let by_name: std::collections::HashMap<&str, &atlas::AtlasProject> =
        a.projects.iter().map(|p| (p.name.as_str(), p)).collect();
    assert_eq!(by_name["Core"].layer, 0);
    // Utils depends on Core.
    assert_eq!(by_name["Utils"].layer, 1);
    // Widget depends on Core via resolved assembly-ref.
    assert_eq!(by_name["Widget"].layer, 1);
    // Domain → Utils → Core.
    assert_eq!(by_name["Domain"].layer, 2);
    // Api, Ui, Domain.Tests all depend on Domain directly.
    assert_eq!(by_name["Api"].layer, 3);
    assert_eq!(by_name["Ui"].layer, 3);
    assert_eq!(by_name["Domain.Tests"].layer, 3);
}

#[test]
fn composition_roots_exclude_tests_and_low_fan_out() {
    let a = build_atlas("atlas");
    // Api and Ui each have fan_in=0 and fan_out>=3 → composition roots.
    // Domain.Tests has fan_in=0 and fan_out>=3 but name ends with .Tests → excluded.
    // Widget has fan_in=0 and fan_out=1 → below threshold → excluded.
    let mut roots = a.composition_roots.clone();
    roots.sort();
    assert_eq!(roots, vec!["Api", "Ui"]);
    assert!(!roots.contains(&"Domain.Tests".to_string()));
    assert!(!roots.contains(&"Widget".to_string()));
}

#[test]
fn no_cycles_and_no_orphans_in_clean_fixture() {
    let a = build_atlas("atlas");
    assert!(a.cycles.is_empty(), "unexpected cycles: {:?}", a.cycles);
    assert!(a.orphans.is_empty(), "unexpected orphans: {:?}", a.orphans);
    assert!(
        a.unresolved.is_empty(),
        "unexpected unresolved: {:?}",
        a.unresolved
    );
}

#[test]
fn fan_in_and_fan_out_counts_are_correct() {
    let a = build_atlas("atlas");
    let by_name: std::collections::HashMap<&str, &atlas::AtlasProject> =
        a.projects.iter().map(|p| (p.name.as_str(), p)).collect();
    // Core has four dependents: Utils, Widget, Api, Ui.
    assert_eq!(by_name["Core"].fan_in, 4);
    assert_eq!(by_name["Core"].fan_out, 0);
    // Domain has three dependents: Api, Ui, Domain.Tests.
    assert_eq!(by_name["Domain"].fan_in, 3);
    assert_eq!(by_name["Domain"].fan_out, 1);
    // Api has three project refs (Domain, Utils, Core).
    assert_eq!(by_name["Api"].fan_out, 3);
    assert_eq!(by_name["Api"].fan_in, 0);
}

#[test]
fn spec_areas_yaml_overrides_inferred_area() {
    // Copy the atlas fixture into a tempdir and drop a spec/areas.yaml that
    // claims the Legacy/Widget tree for Billing and introduces a new
    // "Platform" area covering Src/Common. Also include a zero-match entry
    // to verify warnings don't crash the build.
    let src = fixture("atlas");
    let dst = std::env::temp_dir().join(format!("nspect-spec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dst);
    copy_dir(&src, &dst);

    let spec_dir = dst.join(".nspect").join("spec");
    std::fs::create_dir_all(&spec_dir).unwrap();
    std::fs::write(
        spec_dir.join("areas.yaml"),
        "areas:\n  Billing:\n    - Legacy/Widget\n  Platform:\n    - Src/Common\n  Ghost:\n    - does/not/exist\n",
    )
    .unwrap();

    let projects = nspect::cli::load_projects(&dst).expect("load");
    let a = atlas::build(projects, &dst, atlas::AtlasOptions::default());
    let by_name: std::collections::HashMap<&str, &atlas::AtlasProject> =
        a.projects.iter().map(|p| (p.name.as_str(), p)).collect();

    // Widget was Legacy → now Billing (single-csproj subtree claim).
    assert_eq!(by_name["Widget"].area, "Billing");
    // Core/Utils were Common → now Platform (directory claim renames).
    assert_eq!(by_name["Core"].area, "Platform");
    assert_eq!(by_name["Utils"].area, "Platform");
    // Billing-side untouched.
    assert_eq!(by_name["Domain"].area, "Billing");

    let areas: Vec<&str> = a.areas.iter().map(|ar| ar.name.as_str()).collect();
    assert!(areas.contains(&"Platform"));
    assert!(!areas.contains(&"Common"));
    assert!(!areas.contains(&"Legacy"));

    let _ = std::fs::remove_dir_all(&dst);
}

fn copy_dir(src: &std::path::Path, dst: &std::path::Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let ft = entry.file_type().unwrap();
        let target = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[test]
fn paths_are_relative_to_scan_root() {
    let a = build_atlas("atlas");
    for p in &a.projects {
        assert!(
            p.path.is_relative(),
            "path {} should be relative to scan root",
            p.path.display()
        );
    }
    for ar in &a.areas {
        assert!(
            ar.root.is_relative(),
            "area root {} should be relative",
            ar.root.display()
        );
    }
}
