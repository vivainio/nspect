//! Emit `build-plan.json` — a parallel-build wave plan derived from the
//! same project-ref graph as `build-deps.json`. Answers "what's the most
//! efficient order to build everything when I have N parallel workers?".
//!
//! Each wave is a set of projects with no inter-dependency; the build
//! runner schedules wave N concurrently, waits for all to finish, then
//! starts wave N+1. The critical path identifies the chain of dependent
//! projects that bottlenecks the total wall-clock build time even with
//! infinite parallelism.

use std::collections::HashMap;
use std::path::Path;

use serde::Serialize;

use crate::model::Project;

#[derive(Debug, Serialize)]
pub struct BuildPlan {
    pub total_projects: usize,
    pub wave_count: usize,
    /// Length of the longest chain of dependent projects. The build's
    /// theoretical minimum wall-clock cost is the sum of build times of
    /// projects on this chain, regardless of how many workers are added.
    pub critical_path_length: usize,
    /// One representative critical path, leaves first → entry point last.
    /// When multiple paths tie for length, the lexicographically smallest
    /// chain is chosen for determinism.
    pub critical_path: Vec<String>,
    /// `waves[i]` lists every project assignable to wave `i`, sorted
    /// alphabetically. Within a wave, projects can build in parallel.
    pub waves: Vec<Vec<String>>,
}

pub fn build(projects: &[Project], _scan_root: &Path) -> BuildPlan {
    let (deps, _dependents) = crate::build_deps::dep_graph(projects);
    let layers = crate::build_deps::compute_layers(&deps);

    // Bucket projects by wave.
    let mut waves: Vec<Vec<String>> = Vec::new();
    for p in projects {
        let w = layers.get(&p.name).copied().unwrap_or(0) as usize;
        if waves.len() <= w {
            waves.resize(w + 1, Vec::new());
        }
        waves[w].push(p.name.clone());
    }
    for v in &mut waves {
        v.sort();
    }

    let critical_path = trace_critical_path(&deps, &layers);

    BuildPlan {
        total_projects: projects.len(),
        wave_count: waves.len(),
        critical_path_length: critical_path.len(),
        critical_path,
        waves,
    }
}

/// Walk forward dep edges, repeatedly picking a dep whose layer is exactly
/// `current - 1`. Starting from the project with the highest layer (ties
/// broken alphabetically) gives one representative critical path.
fn trace_critical_path(
    deps: &HashMap<String, std::collections::BTreeSet<String>>,
    layers: &HashMap<String, u32>,
) -> Vec<String> {
    if layers.is_empty() {
        return Vec::new();
    }
    let max_layer = layers.values().copied().max().unwrap_or(0);
    let mut starters: Vec<&String> = layers
        .iter()
        .filter_map(|(n, l)| (*l == max_layer).then_some(n))
        .collect();
    starters.sort();
    let Some(start) = starters.first() else {
        return Vec::new();
    };

    let mut path: Vec<String> = vec![(*start).clone()];
    let mut cur = (*start).clone();
    let mut cur_layer = max_layer;
    while cur_layer > 0 {
        let want = cur_layer - 1;
        let Some(ds) = deps.get(&cur) else {
            break;
        };
        let mut candidates: Vec<&String> = ds
            .iter()
            .filter(|d| layers.get(*d).copied() == Some(want))
            .collect();
        candidates.sort();
        let Some(next) = candidates.first() else {
            break;
        };
        path.push((*next).clone());
        cur = (*next).clone();
        cur_layer = want;
    }
    // Reverse so the path reads leaves-first → entry-point-last, matching
    // build order.
    path.reverse();
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProjectId;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn mk(name: &str, refs: &[&str]) -> Project {
        Project {
            id: ProjectId::from_path(Path::new(name)),
            path: PathBuf::from(format!("/r/{name}.csproj")),
            name: name.to_string(),
            sdk_style: true,
            target_frameworks: vec![],
            package_refs: vec![],
            project_refs: refs
                .iter()
                .map(|n| PathBuf::from(format!("/r/{n}.csproj")))
                .collect(),
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
    fn waves_match_forward_dep_layers() {
        // Foundation -> Common -> {Web, Worker} -> Tests
        let projects = vec![
            mk("Foundation", &[]),
            mk("Common", &["Foundation"]),
            mk("Web", &["Common"]),
            mk("Worker", &["Common"]),
            mk("Tests", &["Web", "Worker"]),
        ];
        let plan = build(&projects, Path::new("/r"));
        assert_eq!(plan.total_projects, 5);
        assert_eq!(plan.wave_count, 4);
        assert_eq!(plan.waves[0], vec!["Foundation".to_string()]);
        assert_eq!(plan.waves[1], vec!["Common".to_string()]);
        assert_eq!(
            plan.waves[2],
            vec!["Web".to_string(), "Worker".to_string()]
        );
        assert_eq!(plan.waves[3], vec!["Tests".to_string()]);
    }

    #[test]
    fn critical_path_traces_longest_chain() {
        let projects = vec![
            mk("Foundation", &[]),
            mk("Common", &["Foundation"]),
            mk("Web", &["Common"]),
            mk("Worker", &["Common"]),
            mk("Tests", &["Web", "Worker"]),
        ];
        let plan = build(&projects, Path::new("/r"));
        assert_eq!(plan.critical_path_length, 4);
        // Tied between "Web" and "Worker" at layer 2 — alphabetical wins,
        // so the trace runs through Web.
        assert_eq!(
            plan.critical_path,
            vec![
                "Foundation".to_string(),
                "Common".to_string(),
                "Web".to_string(),
                "Tests".to_string(),
            ]
        );
    }

    #[test]
    fn empty_repo_produces_empty_plan() {
        let plan = build(&[], Path::new("/r"));
        assert_eq!(plan.total_projects, 0);
        assert_eq!(plan.wave_count, 0);
        assert!(plan.waves.is_empty());
        assert!(plan.critical_path.is_empty());
    }
}
