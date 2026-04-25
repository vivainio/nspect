//! Emit `build-deps.json` — a flat per-project rebuild index for use by
//! build runners. Answers "after modifying component X, what needs to be
//! rebuilt?" via the `rebuild_on_change` reverse transitive closure.
//!
//! Pure restructure of `Project.project_refs`; no new analysis. Emitted as
//! JSON because the audience is build tools, not humans.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::model::Project;

#[derive(Debug, Serialize)]
pub struct BuildDeps {
    pub projects: BTreeMap<String, ProjectDeps>,
}

#[derive(Debug, Serialize)]
pub struct ProjectDeps {
    pub path: PathBuf,
    /// Forward-dep depth (`max(deps.wave) + 1`, leaves at 0). Matches the
    /// wave index a parallel build runner would schedule this project in.
    pub wave: u32,
    /// Direct project references (projects this one declares
    /// `<ProjectReference>` for).
    pub deps: Vec<String>,
    /// Direct dependents — projects that reference this one.
    pub dependents: Vec<String>,
    /// Reverse transitive closure plus this project itself, ordered by
    /// layer (leaves first). Iterating this list in order is a valid
    /// rebuild plan after any source change inside this project.
    pub rebuild_on_change: Vec<String>,
}

pub fn build(projects: &[Project], scan_root: &Path) -> BuildDeps {
    let root = scan_root
        .canonicalize()
        .unwrap_or_else(|_| scan_root.to_path_buf());

    let (deps, dependents) = dep_graph(projects);
    // Layer = longest forward-dep chain length. Drives the topological
    // ordering inside `rebuild_on_change`. Cycles are tolerated: a node
    // inside a cycle gets the depth of its last-resolved entry.
    let layers = compute_layers(&deps);

    // BFS the reverse graph from each project to get its transitive
    // dependent set, then sort by layer ascending.
    let mut out: BTreeMap<String, ProjectDeps> = BTreeMap::new();
    for p in projects {
        let direct_deps: Vec<String> = deps
            .get(&p.name)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        let direct_dependents: Vec<String> = dependents
            .get(&p.name)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();

        let mut reverse_closure: BTreeSet<String> = BTreeSet::new();
        reverse_closure.insert(p.name.clone());
        let mut queue: VecDeque<String> = VecDeque::new();
        queue.push_back(p.name.clone());
        while let Some(cur) = queue.pop_front() {
            if let Some(ds) = dependents.get(&cur) {
                for n in ds {
                    if reverse_closure.insert(n.clone()) {
                        queue.push_back(n.clone());
                    }
                }
            }
        }
        let mut rebuild: Vec<String> = reverse_closure.into_iter().collect();
        rebuild.sort_by(|a, b| {
            let la = layers.get(a).copied().unwrap_or(0);
            let lb = layers.get(b).copied().unwrap_or(0);
            la.cmp(&lb).then_with(|| a.cmp(b))
        });

        out.insert(
            p.name.clone(),
            ProjectDeps {
                path: relativize(&p.path, &root),
                wave: layers.get(&p.name).copied().unwrap_or(0),
                deps: direct_deps,
                dependents: direct_dependents,
                rebuild_on_change: rebuild,
            },
        );
    }

    BuildDeps { projects: out }
}

fn relativize(path: &Path, root: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Build the forward and reverse project-ref graphs, keyed by project name.
/// Names of unresolved refs are silently dropped — same behavior as atlas's
/// `unresolved` reporting (we don't double-warn here).
pub(crate) fn dep_graph(
    projects: &[Project],
) -> (
    HashMap<String, BTreeSet<String>>,
    HashMap<String, BTreeSet<String>>,
) {
    let mut name_by_path: HashMap<PathBuf, String> = HashMap::new();
    for p in projects {
        name_by_path.insert(crate::csproj::canonicalize(&p.path), p.name.clone());
    }
    let mut deps: HashMap<String, BTreeSet<String>> = HashMap::new();
    let mut dependents: HashMap<String, BTreeSet<String>> = HashMap::new();
    for p in projects {
        let entry = deps.entry(p.name.clone()).or_default();
        for r in &p.project_refs {
            let canon = crate::csproj::canonicalize(r);
            let Some(target) = name_by_path.get(&canon) else {
                continue;
            };
            entry.insert(target.clone());
            dependents
                .entry(target.clone())
                .or_default()
                .insert(p.name.clone());
        }
    }
    (deps, dependents)
}

/// Forward-dep depth per project. Memoized DFS with cycle protection.
pub(crate) fn compute_layers(deps: &HashMap<String, BTreeSet<String>>) -> HashMap<String, u32> {
    let mut out: HashMap<String, u32> = HashMap::new();
    for name in deps.keys() {
        let mut stack: BTreeSet<String> = BTreeSet::new();
        depth(name, deps, &mut out, &mut stack);
    }
    out
}

fn depth(
    name: &str,
    deps: &HashMap<String, BTreeSet<String>>,
    memo: &mut HashMap<String, u32>,
    on_stack: &mut BTreeSet<String>,
) -> u32 {
    if let Some(&d) = memo.get(name) {
        return d;
    }
    if !on_stack.insert(name.to_string()) {
        // Cycle — bottom out at 0 and let the eventual memo fill in a
        // stable value. The exact ordering inside cycles isn't important
        // for the rebuild list as long as it's deterministic.
        return 0;
    }
    let mut d = 0u32;
    if let Some(ds) = deps.get(name) {
        for n in ds {
            d = d.max(depth(n, deps, memo, on_stack).saturating_add(1));
        }
    }
    on_stack.remove(name);
    memo.insert(name.to_string(), d);
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProjectId;

    fn mk(name: &str, refs: &[&str]) -> Project {
        Project {
            id: ProjectId::from_path(Path::new(name)),
            path: PathBuf::from(format!("/r/{name}.csproj")),
            name: name.to_string(),
            sdk_style: true,
            target_frameworks: vec![],
            package_refs: vec![],
            project_refs: refs.iter().map(|n| PathBuf::from(format!("/r/{n}.csproj"))).collect(),
            assembly_refs: vec![],
            usings: vec![],
            declared_namespaces: vec![],
            declared_types: BTreeMap::new(),
            type_metrics: BTreeMap::new(),
            referenced_types: vec![],
            source_files: vec![],
        }
    }

    #[test]
    fn rebuild_set_is_reverse_transitive_closure() {
        // Domain <- Web <- Tests, Domain <- Worker
        // Changing Domain rebuilds itself + Web + Tests + Worker.
        let projects = vec![
            mk("Domain", &[]),
            mk("Web", &["Domain"]),
            mk("Worker", &["Domain"]),
            mk("Tests", &["Web"]),
        ];
        let bd = build(&projects, Path::new("/r"));
        let dom = bd.projects.get("Domain").unwrap();
        assert_eq!(dom.deps, Vec::<String>::new());
        assert_eq!(
            dom.dependents,
            vec!["Web".to_string(), "Worker".to_string()]
        );
        assert_eq!(dom.wave, 0); // leaf
        // Topological: leaves first → Domain (layer 0), Web/Worker (1),
        // Tests (2).
        assert_eq!(
            dom.rebuild_on_change,
            vec![
                "Domain".to_string(),
                "Web".to_string(),
                "Worker".to_string(),
                "Tests".to_string(),
            ]
        );

        // Tests is a leaf consumer — only itself rebuilds.
        let tests = bd.projects.get("Tests").unwrap();
        assert_eq!(tests.rebuild_on_change, vec!["Tests".to_string()]);
        assert_eq!(tests.wave, 2);
        assert_eq!(bd.projects.get("Web").unwrap().wave, 1);
    }

    #[test]
    fn handles_unresolved_refs() {
        // Web declares a ref to a project we don't know about — should be
        // dropped silently (matches atlas's behavior for unresolved refs).
        let projects = vec![mk("Web", &["Ghost"])];
        let bd = build(&projects, Path::new("/r"));
        let web = bd.projects.get("Web").unwrap();
        assert!(web.deps.is_empty());
        assert!(web.dependents.is_empty());
        assert_eq!(web.rebuild_on_change, vec!["Web".to_string()]);
    }

    #[test]
    fn cycle_does_not_loop() {
        // A -> B -> A. Both rebuild together. Layer assignment doesn't
        // matter for correctness, only termination + determinism.
        let projects = vec![mk("A", &["B"]), mk("B", &["A"])];
        let bd = build(&projects, Path::new("/r"));
        let a = bd.projects.get("A").unwrap();
        let mut sorted = a.rebuild_on_change.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["A".to_string(), "B".to_string()]);
    }
}
