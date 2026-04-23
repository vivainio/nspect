use std::collections::{BTreeMap, HashMap};

use petgraph::algo::tarjan_scc;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use serde::Serialize;

use crate::model::{Project, ProjectId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum EdgeKind {
    ProjectRef,
    PackageRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Node {
    Project(ProjectId),
    Package(String),
}

pub struct ProjectGraph {
    pub graph: DiGraph<Node, EdgeKind>,
    pub project_nodes: HashMap<ProjectId, NodeIndex>,
    pub package_nodes: BTreeMap<String, NodeIndex>,
    pub projects: BTreeMap<ProjectId, Project>,
    pub unresolved: Vec<UnresolvedRef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnresolvedRef {
    pub from: ProjectId,
    pub target: std::path::PathBuf,
}

impl ProjectGraph {
    /// Build a project-only graph. Use [`build_with_packages`] to include NuGet packages.
    pub fn build(projects: Vec<Project>) -> Self {
        Self::build_inner(projects, false)
    }

    /// Build a graph that also includes package nodes and `Project → Package` edges.
    /// Packages tend to dominate large monoliths, so this is opt-in.
    pub fn build_with_packages(projects: Vec<Project>) -> Self {
        Self::build_inner(projects, true)
    }

    fn build_inner(projects: Vec<Project>, include_packages: bool) -> Self {
        let mut graph = DiGraph::new();
        let mut project_nodes: HashMap<ProjectId, NodeIndex> = HashMap::new();
        let mut package_nodes: BTreeMap<String, NodeIndex> = BTreeMap::new();
        let mut by_path: HashMap<std::path::PathBuf, ProjectId> = HashMap::new();
        let mut by_id: BTreeMap<ProjectId, Project> = BTreeMap::new();

        for p in projects {
            let idx = graph.add_node(Node::Project(p.id));
            project_nodes.insert(p.id, idx);
            by_path.insert(p.path.clone(), p.id);
            by_id.insert(p.id, p);
        }

        let mut unresolved = Vec::new();
        for project in by_id.values() {
            let src = project_nodes[&project.id];

            for rel in &project.project_refs {
                let canon = crate::csproj::canonicalize(rel);
                if let Some(target_id) = by_path.get(&canon) {
                    let dst = project_nodes[target_id];
                    graph.add_edge(src, dst, EdgeKind::ProjectRef);
                } else {
                    unresolved.push(UnresolvedRef {
                        from: project.id,
                        target: canon,
                    });
                }
            }

            if include_packages {
                for pkg in &project.package_refs {
                    let dst = *package_nodes
                        .entry(pkg.name.clone())
                        .or_insert_with(|| graph.add_node(Node::Package(pkg.name.clone())));
                    graph.add_edge(src, dst, EdgeKind::PackageRef);
                }
            }
        }

        Self {
            graph,
            project_nodes,
            package_nodes,
            projects: by_id,
            unresolved,
        }
    }

    pub fn name(&self, id: ProjectId) -> &str {
        self.projects
            .get(&id)
            .map(|p| p.name.as_str())
            .unwrap_or("<unknown>")
    }

    fn node_label(&self, n: &Node) -> String {
        match n {
            Node::Project(id) => self.name(*id).to_string(),
            Node::Package(p) => format!("📦 {p}"),
        }
    }

    /// Cycles in the project-to-project subgraph.
    pub fn cycles(&self) -> Vec<Vec<ProjectId>> {
        // Build a project-only subgraph view.
        let mut sub: DiGraph<ProjectId, ()> = DiGraph::new();
        let mut idx = HashMap::new();
        for &id in self.projects.keys() {
            idx.insert(id, sub.add_node(id));
        }
        for e in self.graph.edge_references() {
            if *e.weight() != EdgeKind::ProjectRef {
                continue;
            }
            let (Node::Project(a), Node::Project(b)) =
                (&self.graph[e.source()], &self.graph[e.target()])
            else {
                continue;
            };
            sub.add_edge(idx[a], idx[b], ());
        }

        let mut out = Vec::new();
        for scc in tarjan_scc(&sub) {
            if scc.len() > 1 {
                out.push(scc.into_iter().map(|n| sub[n]).collect());
            } else if scc.len() == 1 {
                let n = scc[0];
                if sub.edges(n).any(|e| e.target() == n) {
                    out.push(vec![sub[n]]);
                }
            }
        }
        out
    }

    /// Projects with no incoming or outgoing project-ref edges (ignores packages).
    pub fn orphans(&self) -> Vec<ProjectId> {
        let mut out = Vec::new();
        for (&id, &idx) in &self.project_nodes {
            let any_project_edge = self
                .graph
                .edges_directed(idx, petgraph::Direction::Incoming)
                .chain(self.graph.edges_directed(idx, petgraph::Direction::Outgoing))
                .any(|e| *e.weight() == EdgeKind::ProjectRef);
            if !any_project_edge {
                out.push(id);
            }
        }
        out.sort_by(|a, b| self.name(*a).cmp(self.name(*b)));
        out
    }

    pub fn to_dot(&self) -> String {
        let mut s = String::from("digraph projects {\n");
        s.push_str("  rankdir=LR;\n");
        s.push_str("  node [shape=box, style=rounded];\n");
        for (idx, node) in self.graph.node_references_iter() {
            let id = self.node_key(node);
            let label = self.node_label(node);
            let style = match node {
                Node::Package(_) => ", shape=ellipse, style=\"filled,rounded\", fillcolor=\"#eeeeee\"",
                _ => "",
            };
            s.push_str(&format!("  \"{id}\" [label=\"{label}\"{style}];\n"));
            let _ = idx;
        }
        for e in self.graph.edge_references() {
            let from = self.node_key(&self.graph[e.source()]);
            let to = self.node_key(&self.graph[e.target()]);
            let style = match e.weight() {
                EdgeKind::PackageRef => " [style=dashed, color=gray]",
                _ => "",
            };
            s.push_str(&format!("  \"{from}\" -> \"{to}\"{style};\n"));
        }
        s.push_str("}\n");
        s
    }

    pub fn to_mermaid(&self) -> String {
        let mut s = String::from("flowchart LR\n");
        let mut short: HashMap<String, String> = HashMap::new();
        for (i, node) in self.graph.node_weights().enumerate() {
            let key = self.node_key(node);
            let nid = format!("n{i}");
            s.push_str(&format!("  {nid}[\"{}\"]\n", self.node_label(node)));
            short.insert(key, nid);
        }
        for e in self.graph.edge_references() {
            let from = &short[&self.node_key(&self.graph[e.source()])];
            let to = &short[&self.node_key(&self.graph[e.target()])];
            let arrow = match e.weight() {
                EdgeKind::PackageRef => "-.->",
                _ => "-->",
            };
            s.push_str(&format!("  {from} {arrow} {to}\n"));
        }
        s
    }

    pub fn to_json(&self) -> anyhow::Result<String> {
        #[derive(Serialize)]
        struct JsonNode<'a> {
            key: String,
            label: String,
            kind: &'a str,
        }
        #[derive(Serialize)]
        struct JsonEdge {
            from: String,
            to: String,
            kind: EdgeKind,
        }
        #[derive(Serialize)]
        struct Out<'a> {
            nodes: Vec<JsonNode<'a>>,
            edges: Vec<JsonEdge>,
            cycles: Vec<Vec<&'a str>>,
            orphans: Vec<&'a str>,
            unresolved: Vec<&'a UnresolvedRef>,
        }
        let nodes: Vec<JsonNode> = self
            .graph
            .node_weights()
            .map(|n| JsonNode {
                key: self.node_key(n),
                label: self.node_label(n),
                kind: match n {
                    Node::Project(_) => "project",
                    Node::Package(_) => "package",
                },
            })
            .collect();
        let edges: Vec<JsonEdge> = self
            .graph
            .edge_references()
            .map(|e| JsonEdge {
                from: self.node_key(&self.graph[e.source()]),
                to: self.node_key(&self.graph[e.target()]),
                kind: *e.weight(),
            })
            .collect();
        let cycles: Vec<Vec<&str>> = self
            .cycles()
            .into_iter()
            .map(|c| c.into_iter().map(|id| self.name(id)).collect())
            .collect();
        let orphans: Vec<&str> = self.orphans().into_iter().map(|id| self.name(id)).collect();
        Ok(serde_json::to_string_pretty(&Out {
            nodes,
            edges,
            cycles,
            orphans,
            unresolved: self.unresolved.iter().collect(),
        })?)
    }

    fn node_key(&self, n: &Node) -> String {
        match n {
            Node::Project(id) => format!("p{}", id.0),
            Node::Package(name) => format!("k:{name}"),
        }
    }
}

// petgraph doesn't offer `node_references_iter` by default on DiGraph; provide a tiny shim.
trait NodeRefIter<N, E> {
    fn node_references_iter(&self) -> Box<dyn Iterator<Item = (NodeIndex, &N)> + '_>;
}
impl<N, E> NodeRefIter<N, E> for DiGraph<N, E> {
    fn node_references_iter(&self) -> Box<dyn Iterator<Item = (NodeIndex, &N)> + '_> {
        Box::new(self.node_indices().map(move |i| (i, &self[i])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PackageRef, Project, ProjectId};
    use std::path::PathBuf;

    fn proj(name: &str, path: &str, refs: &[&str], pkgs: &[(&str, Option<&str>)]) -> Project {
        let p = PathBuf::from(path);
        Project {
            id: ProjectId::from_path(&p),
            path: p,
            name: name.to_string(),
            sdk_style: true,
            target_frameworks: vec!["net8.0".into()],
            package_refs: pkgs
                .iter()
                .map(|(n, v)| PackageRef {
                    name: (*n).to_string(),
                    version: v.map(|s| s.to_string()),
                    private_assets: None,
                })
                .collect(),
            project_refs: refs.iter().map(PathBuf::from).collect(),
            assembly_refs: Vec::new(),
            usings: Vec::new(),
            declared_namespaces: Vec::new(),
            declared_types: std::collections::BTreeMap::new(),
            type_metrics: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn detects_cycle() {
        let a = proj("A", "/r/A.csproj", &["/r/B.csproj"], &[]);
        let b = proj("B", "/r/B.csproj", &["/r/A.csproj"], &[]);
        let g = ProjectGraph::build(vec![a, b]);
        assert_eq!(g.cycles().len(), 1);
    }

    #[test]
    fn detects_orphan() {
        let a = proj("A", "/r/A.csproj", &[], &[("Serilog", Some("3"))]);
        let b = proj("B", "/r/B.csproj", &["/r/C.csproj"], &[]);
        let c = proj("C", "/r/C.csproj", &[], &[]);
        let g = ProjectGraph::build(vec![a, b, c]);
        let orphans = g.orphans();
        // A is an orphan (only has package refs, no project refs).
        assert_eq!(orphans.len(), 1);
        assert_eq!(g.name(orphans[0]), "A");
    }

    #[test]
    fn unresolved_project_refs() {
        let a = proj("A", "/r/A.csproj", &["/r/Missing.csproj"], &[]);
        let g = ProjectGraph::build(vec![a]);
        assert_eq!(g.unresolved.len(), 1);
    }

    #[test]
    fn default_build_excludes_packages() {
        let a = proj("A", "/r/A.csproj", &[], &[("Serilog", Some("3.1.1"))]);
        let g = ProjectGraph::build(vec![a]);
        assert!(g.package_nodes.is_empty());
    }

    #[test]
    fn opt_in_build_includes_packages() {
        let a = proj("A", "/r/A.csproj", &[], &[("Serilog", Some("3.1.1"))]);
        let b = proj("B", "/r/B.csproj", &[], &[("Serilog", Some("3.1.1"))]);
        let g = ProjectGraph::build_with_packages(vec![a, b]);
        assert_eq!(g.package_nodes.len(), 1);
        assert!(g.package_nodes.contains_key("Serilog"));
    }
}
