//! Atlas: a machine-readable structural snapshot of a C# codebase, grouped by
//! area (top-level directory cluster of projects) and project (one .csproj).
//!
//! One deterministic pass over the repo that emits edges, layers, areas,
//! fan-in/out, and cycles in a single JSON artifact.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use petgraph::visit::EdgeRef;
use serde::Serialize;

use crate::analysis::{analyze, Finding};
use crate::graph::{EdgeKind, Node, ProjectGraph, UnresolvedRef};
use crate::model::{Project, ProjectId};

/// Knobs that expand what `build` includes in the returned atlas.
#[derive(Debug, Default, Clone, Copy)]
pub struct AtlasOptions {
    /// Include findings from `analysis::analyze` (cycles, version conflicts,
    /// unused/undeclared package refs, orphans).
    pub check: bool,
}

#[derive(Debug, Serialize)]
pub struct Atlas {
    pub root: PathBuf,
    /// Projects that nothing depends on but that depend on others — entry
    /// points / composition roots. Typically apps, services, websites, tests.
    pub composition_roots: Vec<String>,
    pub areas: Vec<Area>,
    pub projects: Vec<AtlasProject>,
    pub cycles: Vec<Vec<String>>,
    pub orphans: Vec<String>,
    pub unresolved: Vec<UnresolvedEntry>,
    /// Populated when `AtlasOptions::check` is set. Empty otherwise and omitted
    /// from the serialized output.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub findings: Vec<Finding>,
}

#[derive(Debug, Serialize)]
pub struct Area {
    pub name: String,
    pub root: PathBuf,
    pub project_count: usize,
    pub projects: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct AtlasProject {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub area: String,
    pub sdk_style: bool,
    pub target_frameworks: Vec<String>,
    /// Internal csproj→csproj refs, resolved to project ids.
    pub project_refs: Vec<String>,
    /// External refs — `<PackageReference>` and `<Reference>` assemblies merged.
    /// `version` is set for NuGet packages and None for bare assembly refs.
    pub refs: Vec<AtlasRef>,
    pub fan_in: usize,
    pub fan_out: usize,
    /// Longest path from this project to any leaf in the project-ref DAG.
    /// Leaves (no outgoing project refs) are layer 0. Nodes in a cycle share
    /// the cycle's max layer; `in_cycle` flags the ambiguity.
    pub layer: u32,
    pub in_cycle: bool,
    /// Aggregate structural "weight" — sum of per-type metrics in this
    /// project. `None` when the tree-sitter source scan didn't run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<crate::metrics::ProjectTotals>,
}

/// External ref emitted as a plain string when it's a bare name, or as a
/// `{name, version}` object when a NuGet version is known. The untagged
/// serialization keeps the common case (bare assembly refs) compact.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum AtlasRef {
    Bare(String),
    Versioned { name: String, version: String },
}

#[derive(Debug, Serialize)]
pub struct UnresolvedEntry {
    pub from: String,
    pub target: PathBuf,
}

pub fn build(projects: Vec<Project>, scan_root: &Path, opts: AtlasOptions) -> Atlas {
    let root = scan_root
        .canonicalize()
        .unwrap_or_else(|_| scan_root.to_path_buf());
    let g = ProjectGraph::build(projects);
    let relativize = |p: &Path| -> PathBuf {
        p.strip_prefix(&root)
            .map(|r| r.to_path_buf())
            .unwrap_or_else(|_| p.to_path_buf())
    };

    let project_ids: Vec<ProjectId> = g.projects.keys().copied().collect();
    let ids = assign_ids(&g);
    let id_str = |id: ProjectId| ids[&id].clone();

    // Area per project: first path segment of the project's path relative to
    // the scan root, skipping a leading `src` / `source` wrapper if present.
    let mut area_of: HashMap<ProjectId, String> = HashMap::new();
    let mut area_root_of: HashMap<String, PathBuf> = HashMap::new();
    for id in &project_ids {
        let p = &g.projects[id];
        let (area, area_root) = derive_area(&p.path, scan_root);
        area_root_of.entry(area.clone()).or_insert(area_root);
        area_of.insert(*id, area);
    }

    // Fan-in / fan-out over project refs only.
    let mut fan_in: HashMap<ProjectId, usize> = HashMap::new();
    let mut fan_out: HashMap<ProjectId, usize> = HashMap::new();
    for id in &project_ids {
        let idx = g.project_nodes[id];
        let out = g
            .graph
            .edges_directed(idx, petgraph::Direction::Outgoing)
            .filter(|e| *e.weight() == EdgeKind::ProjectRef)
            .count();
        let inc = g
            .graph
            .edges_directed(idx, petgraph::Direction::Incoming)
            .filter(|e| *e.weight() == EdgeKind::ProjectRef)
            .count();
        fan_out.insert(*id, out);
        fan_in.insert(*id, inc);
    }

    // Cycle membership.
    let cycles = g.cycles();
    let mut in_cycle: HashSet<ProjectId> = HashSet::new();
    for c in &cycles {
        for id in c {
            in_cycle.insert(*id);
        }
    }

    let layers = compute_layers(&g);

    // Project records.
    let mut atlas_projects: Vec<AtlasProject> = project_ids
        .iter()
        .map(|id| {
            let p = &g.projects[id];
            let refs: Vec<String> = p
                .project_refs
                .iter()
                .filter_map(|rel| {
                    let canon = crate::csproj::canonicalize(rel);
                    let target_id = ProjectId::from_path(&canon);
                    if g.projects.contains_key(&target_id) {
                        Some(id_str(target_id))
                    } else {
                        None
                    }
                })
                .collect();
            // Only surface weight when the source scan actually ran — an
            // empty map would otherwise misreport every project as zero.
            let weight = if p.type_metrics.is_empty() {
                None
            } else {
                Some(crate::metrics::project_totals(p))
            };
            AtlasProject {
                id: id_str(*id),
                name: p.name.clone(),
                path: relativize(&p.path),
                area: area_of[id].clone(),
                sdk_style: p.sdk_style,
                target_frameworks: p.target_frameworks.clone(),
                project_refs: refs,
                refs: merge_external_refs(p),
                fan_in: fan_in[id],
                fan_out: fan_out[id],
                layer: layers.get(id).copied().unwrap_or(0),
                in_cycle: in_cycle.contains(id),
                weight,
            }
        })
        .collect();
    atlas_projects.sort_by(|a, b| a.name.cmp(&b.name));

    // Area records.
    let mut by_area: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in &atlas_projects {
        by_area
            .entry(p.area.clone())
            .or_default()
            .push(p.id.clone());
    }
    let areas: Vec<Area> = by_area
        .into_iter()
        .map(|(name, projects)| Area {
            root: area_root_of
                .get(&name)
                .map(|p| relativize(p))
                .unwrap_or_else(|| PathBuf::from(&name)),
            project_count: projects.len(),
            projects,
            name,
        })
        .collect();

    let cycles_out: Vec<Vec<String>> = cycles
        .into_iter()
        .map(|c| c.into_iter().map(id_str).collect())
        .collect();

    let orphans_out: Vec<String> = g.orphans().into_iter().map(id_str).collect();

    // Composition roots: nothing depends on them (fan_in == 0) but they pull
    // in a real stack (fan_out >= 3). The threshold filters out unreferenced
    // leaf libraries — Interface/Contracts/Model projects that nothing
    // consumes — which match the zero-fan-in pattern but aren't entry points.
    // Tests and installers are also filtered by name.
    const COMPOSITION_ROOT_MIN_FAN_OUT: usize = 3;
    let mut composition_roots: Vec<String> = atlas_projects
        .iter()
        .filter(|p| {
            p.fan_in == 0 && p.fan_out >= COMPOSITION_ROOT_MIN_FAN_OUT && !looks_like_test(&p.name)
        })
        .map(|p| p.id.clone())
        .collect();
    composition_roots.sort();

    let unresolved: Vec<UnresolvedEntry> = g
        .unresolved
        .iter()
        .map(|u: &UnresolvedRef| UnresolvedEntry {
            from: id_str(u.from),
            target: u.target.clone(),
        })
        .collect();

    let findings = if opts.check {
        analyze(&g)
    } else {
        Vec::new()
    };

    Atlas {
        root,
        composition_roots,
        areas,
        projects: atlas_projects,
        cycles: cycles_out,
        orphans: orphans_out,
        unresolved,
        findings,
    }
}

/// Assign each project a stable, human-readable id derived from its name.
///
/// Strips a dominant dotted prefix (e.g. `Basware.P2P.`) from names that share
/// it, provided the stripping doesn't cause collisions with the rest. Then
/// breaks any remaining duplicate-name collisions with `#2`, `#3` ordinal
/// suffixes (ordered by csproj path).
fn assign_ids(g: &ProjectGraph) -> HashMap<ProjectId, String> {
    let names: Vec<&str> = g.projects.values().map(|p| p.name.as_str()).collect();
    let strip = pick_strip_prefix(&names);

    let derive = |name: &str| -> String {
        if let Some(prefix) = &strip {
            if let Some(rest) = name.strip_prefix(prefix.as_str()) {
                if !rest.is_empty() {
                    return rest.to_string();
                }
            }
        }
        name.to_string()
    };

    // Group by the (candidate) derived id; suffix collisions with path-stable #N.
    let mut by_derived: HashMap<String, Vec<ProjectId>> = HashMap::new();
    for (&id, p) in &g.projects {
        by_derived.entry(derive(&p.name)).or_default().push(id);
    }
    let mut out: HashMap<ProjectId, String> = HashMap::new();
    for (derived, mut ids) in by_derived {
        if ids.len() == 1 {
            out.insert(ids[0], derived);
            continue;
        }
        ids.sort_by(|a, b| g.projects[a].path.cmp(&g.projects[b].path));
        for (i, id) in ids.iter().enumerate() {
            let label = if i == 0 {
                derived.clone()
            } else {
                format!("{derived}#{}", i + 1)
            };
            out.insert(*id, label);
        }
    }
    out
}

/// Pick a dotted prefix (including trailing dot) to strip from project names,
/// or None if no candidate is worth it.
///
/// Strategy: prefer longer prefixes, subject to applying to a majority and not
/// colliding with the names that don't carry the prefix.
fn pick_strip_prefix(names: &[&str]) -> Option<String> {
    let total = names.len();
    if total < 6 {
        return None;
    }

    // Try prefixes of 3, then 2 dotted segments. 1-segment prefixes ("Basware")
    // strip too aggressively — rarely worth it.
    for seg_count in [3, 2] {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for name in names {
            let segs: Vec<&str> = name.split('.').collect();
            if segs.len() <= seg_count {
                continue;
            }
            let prefix: String = segs[..seg_count].join(".") + ".";
            *counts.entry(prefix).or_insert(0) += 1;
        }
        let Some((prefix, count)) = counts.into_iter().max_by_key(|(_, c)| *c) else {
            continue;
        };
        if count * 2 < total {
            continue; // not a majority — skip
        }

        // Collision check: stripped ids must not clash with unstripped ids.
        let mut stripped: HashSet<String> = HashSet::new();
        let mut unstripped: HashSet<String> = HashSet::new();
        for name in names {
            if let Some(rest) = name.strip_prefix(prefix.as_str()) {
                if rest.is_empty() {
                    continue;
                }
                stripped.insert(rest.to_string());
            } else {
                unstripped.insert((*name).to_string());
            }
        }
        if stripped.is_disjoint(&unstripped) {
            return Some(prefix);
        }
    }
    None
}

fn merge_external_refs(p: &Project) -> Vec<AtlasRef> {
    let mut out: Vec<AtlasRef> = Vec::with_capacity(p.package_refs.len() + p.assembly_refs.len());
    for pk in &p.package_refs {
        out.push(match &pk.version {
            Some(v) => AtlasRef::Versioned {
                name: pk.name.clone(),
                version: v.clone(),
            },
            None => AtlasRef::Bare(pk.name.clone()),
        });
    }
    for name in &p.assembly_refs {
        out.push(AtlasRef::Bare(name.clone()));
    }
    out.sort_by(|a, b| ref_name(a).cmp(ref_name(b)));
    out.dedup_by(|a, b| match (&*a, &*b) {
        (AtlasRef::Bare(x), AtlasRef::Bare(y)) => x == y,
        (
            AtlasRef::Versioned {
                name: an,
                version: av,
            },
            AtlasRef::Versioned {
                name: bn,
                version: bv,
            },
        ) => an == bn && av == bv,
        _ => false,
    });
    out
}

/// True if the project name looks like a test or installer project — a
/// dotted segment ends with `test`, `tests`, `testing`, or `installer`
/// (case-insensitive). Matches `.Tests`, `.IntegrationTesting`,
/// `.ServiceInstaller`, `.NServiceBus.Installer`, etc.
fn looks_like_test(name: &str) -> bool {
    name.split('.').any(|seg| {
        let lower = seg.to_ascii_lowercase();
        lower.ends_with("test")
            || lower.ends_with("tests")
            || lower.ends_with("testing")
            || lower.ends_with("installer")
    })
}

fn ref_name(r: &AtlasRef) -> &str {
    match r {
        AtlasRef::Bare(n) => n,
        AtlasRef::Versioned { name, .. } => name,
    }
}

/// Area = first path segment of `project_path` relative to `scan_root`,
/// skipping a leading `src` / `source` wrapper segment if present.
///
/// Falls back to "unmapped" if the project lives outside the scan root.
fn derive_area(project_path: &Path, scan_root: &Path) -> (String, PathBuf) {
    let scan_root = scan_root
        .canonicalize()
        .unwrap_or_else(|_| scan_root.to_path_buf());
    let project = project_path
        .canonicalize()
        .unwrap_or_else(|_| project_path.to_path_buf());

    let rel = match project.strip_prefix(&scan_root) {
        Ok(r) => r.to_path_buf(),
        Err(_) => return ("unmapped".to_string(), scan_root.clone()),
    };

    let mut segs: Vec<&std::ffi::OsStr> = rel.iter().collect();
    // Drop trailing filename.
    if segs
        .last()
        .map(|s| s.to_string_lossy().ends_with(".csproj"))
        .unwrap_or(false)
    {
        segs.pop();
    }

    let mut prefix = scan_root.clone();
    // Skip a leading `src` / `source` / `sources` segment.
    if let Some(first) = segs.first() {
        let low = first.to_string_lossy().to_lowercase();
        if low == "src" || low == "source" || low == "sources" {
            prefix.push(first);
            segs.remove(0);
        }
    }

    let Some(area_seg) = segs.first() else {
        return ("root".to_string(), prefix);
    };
    let area_name = area_seg.to_string_lossy().to_string();
    let area_root = prefix.join(area_seg);
    (area_name, area_root)
}

/// Layer = longest path to any leaf in the project-ref DAG.
/// Cycles: every member gets the max layer across the SCC.
fn compute_layers(g: &ProjectGraph) -> HashMap<ProjectId, u32> {
    use petgraph::algo::tarjan_scc;
    use petgraph::graph::{DiGraph, NodeIndex};

    // Project-only subgraph.
    let mut sub: DiGraph<ProjectId, ()> = DiGraph::new();
    let mut idx: HashMap<ProjectId, NodeIndex> = HashMap::new();
    for &id in g.projects.keys() {
        idx.insert(id, sub.add_node(id));
    }
    for e in g.graph.edge_references() {
        if *e.weight() != EdgeKind::ProjectRef {
            continue;
        }
        let (Node::Project(a), Node::Project(b)) = (&g.graph[e.source()], &g.graph[e.target()])
        else {
            continue;
        };
        sub.add_edge(idx[a], idx[b], ());
    }

    // Condense SCCs so we can compute layer on a DAG even when cycles exist.
    let sccs = tarjan_scc(&sub);
    let mut scc_of: HashMap<NodeIndex, usize> = HashMap::new();
    for (i, comp) in sccs.iter().enumerate() {
        for &n in comp {
            scc_of.insert(n, i);
        }
    }

    // Condensed DAG: one node per SCC, edges between distinct SCCs.
    let n_scc = sccs.len();
    let mut cond_out: Vec<HashSet<usize>> = vec![HashSet::new(); n_scc];
    for e in sub.edge_references() {
        let a = scc_of[&e.source()];
        let b = scc_of[&e.target()];
        if a != b {
            cond_out[a].insert(b);
        }
    }

    // Longest-path layer via memoized DFS (condensed graph is a DAG).
    let mut layer_of: Vec<Option<u32>> = vec![None; n_scc];
    fn dfs(v: usize, cond_out: &[HashSet<usize>], memo: &mut [Option<u32>]) -> u32 {
        if let Some(l) = memo[v] {
            return l;
        }
        let mut best: u32 = 0;
        for &w in &cond_out[v] {
            let lw = dfs(w, cond_out, memo);
            if lw + 1 > best {
                best = lw + 1;
            }
        }
        memo[v] = Some(best);
        best
    }
    for v in 0..n_scc {
        dfs(v, &cond_out, &mut layer_of);
    }

    let mut out: HashMap<ProjectId, u32> = HashMap::new();
    for (i, comp) in sccs.iter().enumerate() {
        let l = layer_of[i].unwrap_or(0);
        for &n in comp {
            out.insert(sub[n], l);
        }
    }
    out
}

/// Resolve a free-form project query to exactly one project id.
///
/// Matching rules, tried in order:
/// 1. Exact name match (case-insensitive).
/// 2. Unique suffix match (project name ends with `query`, case-insensitive).
///    Handles `OperationContext` → `Basware.P2P.Common.Infrastructure.OperationContext`.
/// 3. Unique substring match (case-insensitive).
///
/// Returns `Err` with the candidate list if the query is ambiguous or unknown.
pub fn resolve_project(atlas: &Atlas, query: &str) -> Result<String, ResolveError> {
    let q = query.to_lowercase();
    let names: Vec<(&str, &str)> = atlas
        .projects
        .iter()
        .map(|p| (p.id.as_str(), p.name.as_str()))
        .collect();

    let exact: Vec<_> = names
        .iter()
        .filter(|(_, n)| n.to_lowercase() == q)
        .collect();
    if exact.len() == 1 {
        return Ok(exact[0].0.to_string());
    }
    if exact.len() > 1 {
        return Err(ResolveError::Ambiguous(
            exact.iter().map(|(_, n)| (*n).to_string()).collect(),
        ));
    }

    let suffix: Vec<_> = names
        .iter()
        .filter(|(_, n)| n.to_lowercase().ends_with(&q))
        .collect();
    if suffix.len() == 1 {
        return Ok(suffix[0].0.to_string());
    }
    if suffix.len() > 1 && suffix.len() <= 20 {
        return Err(ResolveError::Ambiguous(
            suffix.iter().map(|(_, n)| (*n).to_string()).collect(),
        ));
    }

    let contains: Vec<_> = names
        .iter()
        .filter(|(_, n)| n.to_lowercase().contains(&q))
        .collect();
    match contains.len() {
        0 => Err(ResolveError::NotFound),
        1 => Ok(contains[0].0.to_string()),
        n if n <= 20 => Err(ResolveError::Ambiguous(
            contains.iter().map(|(_, n)| (*n).to_string()).collect(),
        )),
        _ => Err(ResolveError::TooMany(contains.len())),
    }
}

#[derive(Debug)]
pub enum ResolveError {
    NotFound,
    Ambiguous(Vec<String>),
    TooMany(usize),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::NotFound => write!(f, "no project matched"),
            ResolveError::Ambiguous(names) => {
                writeln!(f, "ambiguous project query, candidates:")?;
                for n in names {
                    writeln!(f, "  {n}")?;
                }
                Ok(())
            }
            ResolveError::TooMany(n) => write!(f, "{n} projects matched; refine the query"),
        }
    }
}

impl std::error::Error for ResolveError {}

/// A neighborhood view: the focus project plus up to `up` hops of reverse
/// project-refs ("what depends on me") and `down` hops of forward refs
/// ("what I depend on"). Edges are filtered to those between retained nodes.
#[derive(Debug)]
pub struct Focus<'a> {
    pub focus_id: &'a str,
    pub nodes: Vec<&'a AtlasProject>,
    pub edges: Vec<(&'a str, &'a str)>,
    pub up: u32,
    pub down: u32,
}

pub fn focus<'a>(atlas: &'a Atlas, focus_id: &'a str, up: u32, down: u32) -> Focus<'a> {
    let by_id: HashMap<&str, &AtlasProject> =
        atlas.projects.iter().map(|p| (p.id.as_str(), p)).collect();

    // Forward adjacency: a -> [b, c] means a depends on b, c.
    // Derived from each project's `project_refs` (already-resolved internal refs).
    let mut fwd: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut rev: HashMap<&str, Vec<&str>> = HashMap::new();
    for p in &atlas.projects {
        for r in &p.project_refs {
            fwd.entry(p.id.as_str()).or_default().push(r.as_str());
            rev.entry(r.as_str()).or_default().push(p.id.as_str());
        }
    }

    let mut kept: HashSet<&str> = HashSet::new();
    kept.insert(focus_id);

    // BFS downward (follow `fwd`).
    if down > 0 {
        let mut frontier: Vec<&str> = vec![focus_id];
        for _ in 0..down {
            let mut next: Vec<&str> = Vec::new();
            for v in &frontier {
                if let Some(ns) = fwd.get(v) {
                    for &w in ns {
                        if kept.insert(w) {
                            next.push(w);
                        }
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
    }

    // BFS upward (follow `rev`).
    if up > 0 {
        let mut frontier: Vec<&str> = vec![focus_id];
        for _ in 0..up {
            let mut next: Vec<&str> = Vec::new();
            for v in &frontier {
                if let Some(ns) = rev.get(v) {
                    for &w in ns {
                        if kept.insert(w) {
                            next.push(w);
                        }
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
    }

    let mut nodes: Vec<&AtlasProject> = kept
        .iter()
        .filter_map(|id| by_id.get(id).copied())
        .collect();
    nodes.sort_by(|a, b| (a.layer, a.name.as_str()).cmp(&(b.layer, b.name.as_str())));

    let edges: Vec<(&str, &str)> = atlas
        .projects
        .iter()
        .filter(|p| kept.contains(p.id.as_str()))
        .flat_map(|p| {
            p.project_refs
                .iter()
                .filter(|r| kept.contains(r.as_str()))
                .map(move |r| (p.id.as_str(), r.as_str()))
        })
        .collect();

    Focus {
        focus_id,
        nodes,
        edges,
        up,
        down,
    }
}

impl<'a> Focus<'a> {
    pub fn to_mermaid(&self) -> String {
        let mut s = String::from("flowchart LR\n");
        let short: HashMap<&str, String> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, p)| (p.id.as_str(), format!("n{i}")))
            .collect();
        for p in &self.nodes {
            let nid = &short[p.id.as_str()];
            let marker = if p.id == self.focus_id {
                ":::focus"
            } else {
                ""
            };
            let label = format!("{}<br/>L{} · {}", p.name, p.layer, p.area);
            s.push_str(&format!("  {nid}[\"{label}\"]{marker}\n"));
        }
        for (f, t) in &self.edges {
            s.push_str(&format!("  {} --> {}\n", short[f], short[t]));
        }
        s.push_str("  classDef focus fill:#ffd966,stroke:#c08000,stroke-width:2px;\n");
        s
    }

    pub fn to_dot(&self) -> String {
        let mut s =
            String::from("digraph focus {\n  rankdir=LR;\n  node [shape=box, style=rounded];\n");
        for p in &self.nodes {
            let extra = if p.id == self.focus_id {
                ", style=\"filled,rounded\", fillcolor=\"#ffd966\""
            } else {
                ""
            };
            s.push_str(&format!(
                "  \"{}\" [label=\"{}\\nL{} · {}\"{}];\n",
                p.id, p.name, p.layer, p.area, extra
            ));
        }
        for (f, t) in &self.edges {
            s.push_str(&format!("  \"{f}\" -> \"{t}\";\n"));
        }
        s.push_str("}\n");
        s
    }

    pub fn to_text(&self) -> String {
        let focus = self
            .nodes
            .iter()
            .find(|p| p.id == self.focus_id)
            .expect("focus node present");

        let by_id: HashMap<&str, &AtlasProject> =
            self.nodes.iter().map(|p| (p.id.as_str(), *p)).collect();

        let mut outs: Vec<&AtlasProject> = self
            .edges
            .iter()
            .filter(|(f, _)| *f == self.focus_id)
            .filter_map(|(_, t)| by_id.get(t).copied())
            .collect();
        outs.sort_by(|a, b| a.name.cmp(&b.name));

        let mut ins: Vec<&AtlasProject> = self
            .edges
            .iter()
            .filter(|(_, t)| *t == self.focus_id)
            .filter_map(|(f, _)| by_id.get(f).copied())
            .collect();
        ins.sort_by(|a, b| a.name.cmp(&b.name));

        let mut s = String::new();
        s.push_str(&format!(
            "focus: {}\n  area={}  layer={}  fan_in={}  fan_out={}\n",
            focus.name, focus.area, focus.layer, focus.fan_in, focus.fan_out
        ));
        s.push_str(&format!(
            "\nnodes: {}  edges: {}  (up={} down={})\n",
            self.nodes.len(),
            self.edges.len(),
            self.up,
            self.down
        ));
        s.push_str(&format!(
            "\ndepended on by (depth 1, {} shown):\n",
            ins.len()
        ));
        for p in &ins {
            s.push_str(&format!("  ← {} [{} L{}]\n", p.name, p.area, p.layer));
        }
        s.push_str(&format!("\ndepends on (depth 1, {} shown):\n", outs.len()));
        for p in &outs {
            s.push_str(&format!("  → {} [{} L{}]\n", p.name, p.area, p.layer));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PackageRef, Project, ProjectId};

    fn proj(name: &str, path: &str, refs: &[&str]) -> Project {
        let p = PathBuf::from(path);
        Project {
            id: ProjectId::from_path(&p),
            path: p,
            name: name.to_string(),
            sdk_style: true,
            target_frameworks: vec!["net8.0".into()],
            package_refs: Vec::new(),
            project_refs: refs.iter().map(PathBuf::from).collect(),
            assembly_refs: Vec::new(),
            usings: Vec::new(),
            declared_namespaces: Vec::new(),
            declared_types: std::collections::BTreeMap::new(),
            type_metrics: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn layers_basic_chain() {
        // C <- B <- A : A depends on B, B on C. C is layer 0, B is 1, A is 2.
        let a = proj("A", "/r/A.csproj", &["/r/B.csproj"]);
        let b = proj("B", "/r/B.csproj", &["/r/C.csproj"]);
        let c = proj("C", "/r/C.csproj", &[]);
        let g = ProjectGraph::build(vec![a, b, c]);
        let layers = compute_layers(&g);
        let by_name: HashMap<_, _> = g
            .projects
            .values()
            .map(|p| (p.name.clone(), layers[&p.id]))
            .collect();
        assert_eq!(by_name["C"], 0);
        assert_eq!(by_name["B"], 1);
        assert_eq!(by_name["A"], 2);
    }

    #[test]
    fn area_from_src_prefix() {
        // Use a tempdir so canonicalize works.
        let tmp = tempdir_new();
        let scan_root = tmp.clone();
        let area_dir = scan_root.join("Src").join("InvoiceAutomation").join("Core");
        std::fs::create_dir_all(&area_dir).unwrap();
        let csproj = area_dir.join("Core.csproj");
        std::fs::write(&csproj, b"<Project/>").unwrap();
        let (area, root) = derive_area(&csproj, &scan_root);
        assert_eq!(area, "InvoiceAutomation");
        assert!(
            root.ends_with("Src/InvoiceAutomation") || root.ends_with("Src\\InvoiceAutomation")
        );
    }

    fn tempdir_new() -> PathBuf {
        let base = std::env::temp_dir().join(format!("nspect-atlas-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base.canonicalize().unwrap()
    }

    #[test]
    fn fan_counts() {
        let a = proj("A", "/r/A.csproj", &["/r/B.csproj", "/r/C.csproj"]);
        let b = proj("B", "/r/B.csproj", &["/r/C.csproj"]);
        let c = proj("C", "/r/C.csproj", &[]);
        let g = ProjectGraph::build(vec![a, b, c]);
        let atlas = build(
            g.projects.values().cloned().collect(),
            std::path::Path::new("/r"),
            AtlasOptions::default(),
        );
        let by_name: HashMap<_, _> = atlas.projects.iter().map(|p| (p.name.clone(), p)).collect();
        assert_eq!(by_name["A"].fan_out, 2);
        assert_eq!(by_name["A"].fan_in, 0);
        assert_eq!(by_name["C"].fan_in, 2);
        assert_eq!(by_name["C"].fan_out, 0);
        // Silence unused import warning for PackageRef in this file — the
        // public schema uses it even though tests don't construct one.
        let _ = std::mem::size_of::<PackageRef>();
    }
}
