//! Write-path tracing: tag methods that call a "write sink" (`SaveChanges`,
//! `Commit`, ...) and propagate upward through the call graph.
//!
//! Uses only data the source scan already collects:
//! * `MethodMetric::invocations` — `(receiver, method)` pairs,
//! * `TypeMetrics::service_fields` — receiver name → declared simple type,
//! * `TypeMetrics::bases` — to fan interface calls out to implementers.
//!
//! Resolution is best-effort and name-based. Chained receivers
//! (`_db.Orders.Add(x)`) and local variables aren't in the invocation data, so
//! they are neither sinks nor edges.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::model::{Project, TypeMetrics};

/// Method names treated as write sinks unless overridden.
pub const DEFAULT_SINKS: &[&str] = &[
    "SaveChanges",
    "SaveChangesAsync",
    "ExecuteNonQuery",
    "ExecuteNonQueryAsync",
    "ExecuteSqlRaw",
    "ExecuteSqlRawAsync",
    "ExecuteSqlInterpolated",
    "ExecuteSqlInterpolatedAsync",
    "ExecuteUpdate",
    "ExecuteUpdateAsync",
    "ExecuteDelete",
    "ExecuteDeleteAsync",
    "Commit",
    "CommitAsync",
];

/// A method node: `(type fqn, method name)`. Overloads share a node.
pub type Node = (String, String);

#[derive(Debug, Default)]
pub struct WriteGraph {
    /// Direct sink calls per method: `"_db.SaveChanges"`.
    pub sinks: BTreeMap<Node, BTreeSet<String>>,
    /// Resolved call edges, restricted to methods that (transitively) write.
    pub edges: BTreeMap<Node, BTreeSet<Node>>,
    /// Writer methods that no other writer calls.
    pub roots: Vec<Node>,
}

/// Heuristic: does this project hold tests? Matches `Test`/`Tests`/`Testing`
/// anywhere in the project name (case-insensitive).
fn is_test_project(name: &str) -> bool {
    name.to_ascii_lowercase().contains("test")
}

/// Heuristic: is this type test code? Name suffix (`Test`, `Tests`, `Fixture`,
/// `TestBase`), a test-framework type attribute, or any method carrying a
/// test attribute.
fn is_test_type(fqn: &str, m: &TypeMetrics) -> bool {
    const TYPE_SUFFIXES: &[&str] = &["Test", "Tests", "Fixture", "TestBase", "TestsBase"];
    const ATTRS: &[&str] = &[
        "TestFixture",
        "TestClass",
        "Test",
        "TestCase",
        "TestCaseSource",
        "Fact",
        "Theory",
        "TestMethod",
        "SetUp",
        "OneTimeSetUp",
        "TearDown",
    ];
    let simple = fqn.rsplit('.').next().unwrap_or(fqn);
    if TYPE_SUFFIXES.iter().any(|s| simple.ends_with(s)) {
        return true;
    }
    let attr_name = |a: &String| a.split('(').next().unwrap_or(a).trim().to_string();
    m.attributes
        .iter()
        .any(|a| ATTRS.contains(&attr_name(a).as_str()))
        || m.methods.iter().any(|x| {
            x.attributes
                .iter()
                .any(|a| ATTRS.contains(&attr_name(a).as_str()))
        })
}

/// Attributes marking a method as an externally reachable entry point.
pub const DEFAULT_ENTRY_ATTRS: &[&str] = &[
    "OperationContract",
    "HttpGet",
    "HttpPost",
    "HttpPut",
    "HttpDelete",
    "HttpPatch",
];

pub fn build(
    projects: &[Project],
    sinks: &BTreeSet<String>,
    include_tests: bool,
    entry_attrs: &BTreeSet<String>,
) -> WriteGraph {
    let maps: Vec<&BTreeMap<String, TypeMetrics>> = projects
        .iter()
        .filter(|p| include_tests || !is_test_project(&p.name))
        .map(|p| &p.type_metrics)
        .collect();
    build_from_types(&maps, sinks, include_tests, entry_attrs)
}

pub fn build_from_types(
    maps: &[&BTreeMap<String, TypeMetrics>],
    sinks: &BTreeSet<String>,
    include_tests: bool,
    entry_attrs: &BTreeSet<String>,
) -> WriteGraph {
    let keep = |fqn: &str, m: &TypeMetrics| include_tests || !is_test_type(fqn, m);
    // simple type name -> fqns (across all projects)
    let mut by_simple: HashMap<&str, Vec<&str>> = HashMap::new();
    // simple base name -> fqns directly deriving/implementing it
    let mut derived: HashMap<&str, Vec<&str>> = HashMap::new();
    for tm in maps {
        for (fqn, m) in tm.iter().filter(|(f, m)| keep(f, m)) {
            let simple = fqn.rsplit('.').next().unwrap_or(fqn);
            by_simple.entry(simple).or_default().push(fqn);
            for b in &m.bases {
                derived.entry(b.as_str()).or_default().push(fqn);
            }
        }
    }
    let methods_of: HashMap<&str, &TypeMetrics> = maps
        .iter()
        .flat_map(|tm| {
            tm.iter()
                .filter(|(k, v)| keep(k, v))
                .map(|(k, v)| (k.as_str(), v))
        })
        .collect();

    // Types reachable "downward" from a simple name: itself + implementers.
    let expand = |simple: &str| -> Vec<&str> {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut visited: BTreeSet<&str> = BTreeSet::new();
        let mut stack: Vec<&str> = vec![simple];
        let mut out = Vec::new();
        while let Some(s) = stack.pop() {
            if !visited.insert(s) {
                continue;
            }
            if let Some(fqns) = by_simple.get(s) {
                for f in fqns {
                    if seen.insert(f) {
                        out.push(*f);
                    }
                }
            }
            if let Some(ds) = derived.get(s) {
                for d in ds {
                    let ds_simple = d.rsplit('.').next().unwrap_or(d);
                    if seen.insert(d) {
                        out.push(*d);
                    }
                    stack.push(ds_simple);
                }
            }
        }
        out
    };

    let mut direct: BTreeMap<Node, BTreeSet<String>> = BTreeMap::new();
    let mut all_edges: BTreeMap<Node, BTreeSet<Node>> = BTreeMap::new();
    for (fqn, m) in &methods_of {
        let fields: HashMap<&str, &str> = m
            .service_fields
            .iter()
            .map(|(n, t)| (n.as_str(), t.as_str()))
            .collect();
        for meth in &m.methods {
            let node: Node = (fqn.to_string(), meth.name.clone());
            for (recv, name) in &meth.invocations {
                if sinks.contains(name) {
                    direct
                        .entry(node.clone())
                        .or_default()
                        .insert(format!("{recv}.{name}"));
                    continue;
                }
                let target_simple = fields.get(recv.as_str()).copied().unwrap_or(recv.as_str());
                for t in expand(target_simple) {
                    let Some(tm) = methods_of.get(t) else {
                        continue;
                    };
                    if tm.methods.iter().any(|x| &x.name == name) {
                        let callee: Node = (t.to_string(), name.clone());
                        if callee != node {
                            all_edges.entry(node.clone()).or_default().insert(callee);
                        }
                    }
                }
            }
        }
    }

    // Reverse BFS from sink-tagged methods.
    let mut rev: HashMap<&Node, Vec<&Node>> = HashMap::new();
    for (from, tos) in &all_edges {
        for to in tos {
            rev.entry(to).or_default().push(from);
        }
    }
    let mut writers: BTreeSet<Node> = direct.keys().cloned().collect();
    let mut queue: Vec<Node> = writers.iter().cloned().collect();
    while let Some(n) = queue.pop() {
        if let Some(callers) = rev.get(&n) {
            for c in callers {
                if writers.insert((*c).clone()) {
                    queue.push((*c).clone());
                }
            }
        }
    }

    let mut edges: BTreeMap<Node, BTreeSet<Node>> = BTreeMap::new();
    let mut has_writer_caller: BTreeSet<&Node> = BTreeSet::new();
    for (from, tos) in &all_edges {
        if !writers.contains(from) {
            continue;
        }
        let kept: BTreeSet<Node> = tos
            .iter()
            .filter(|t| writers.contains(*t))
            .cloned()
            .collect();
        if !kept.is_empty() {
            edges.insert(from.clone(), kept);
        }
    }
    for tos in edges.values() {
        for t in tos {
            has_writer_caller.insert(t);
        }
    }
    if !entry_attrs.is_empty() {
        // Entry mode: roots are exactly the writer methods that are entry
        // points. The attribute may sit on the method itself or on the same-
        // named method of any base type / interface (WCF puts
        // `[OperationContract]` on the interface, not the implementation).
        let is_attr = |attrs: &[String]| {
            attrs
                .iter()
                .any(|a| entry_attrs.contains(a.split('(').next().unwrap_or(a).trim()))
        };
        let has_entry_attr = |fqn: &str, name: &str| -> bool {
            let mut seen: BTreeSet<&str> = BTreeSet::new();
            let mut stack: Vec<&str> = vec![fqn];
            while let Some(f) = stack.pop() {
                if !seen.insert(f) {
                    continue;
                }
                let Some(tm) = methods_of.get(f) else {
                    continue;
                };
                if tm
                    .methods
                    .iter()
                    .any(|x| x.name == name && is_attr(&x.attributes))
                {
                    return true;
                }
                for b in &tm.bases {
                    if let Some(fqns) = by_simple.get(b.as_str()) {
                        stack.extend(fqns.iter().copied());
                    }
                }
            }
            false
        };
        let roots: Vec<Node> = writers
            .iter()
            .filter(|(t, m)| has_entry_attr(t, m))
            .cloned()
            .collect();
        return WriteGraph {
            sinks: direct,
            edges,
            roots,
        };
    }
    let mut roots: Vec<Node> = writers
        .iter()
        .filter(|n| !has_writer_caller.contains(n))
        .cloned()
        .collect();
    // Pure cycles have no root; surface their smallest member so nothing hides.
    let mut covered = reachable(&edges, &roots);
    for n in &writers {
        if !covered.contains(n) {
            roots.push(n.clone());
            covered.extend(reachable(&edges, std::slice::from_ref(n)));
        }
    }

    WriteGraph {
        sinks: direct,
        edges,
        roots,
    }
}

fn reachable(edges: &BTreeMap<Node, BTreeSet<Node>>, from: &[Node]) -> BTreeSet<Node> {
    let mut seen: BTreeSet<Node> = from.iter().cloned().collect();
    let mut stack: Vec<Node> = from.to_vec();
    while let Some(n) = stack.pop() {
        for t in edges.get(&n).into_iter().flatten() {
            if seen.insert(t.clone()) {
                stack.push(t.clone());
            }
        }
    }
    seen
}

/// Render the write tree from each root (or from `from`-matching nodes).
pub fn render(g: &WriteGraph, from: Option<&str>, max_depth: usize) -> String {
    let roots: Vec<Node> = match from {
        None => g.roots.clone(),
        Some(q) => {
            let all: BTreeSet<&Node> = g
                .sinks
                .keys()
                .chain(g.edges.keys())
                .chain(g.edges.values().flatten())
                .collect();
            all.into_iter().filter(|n| matches(n, q)).cloned().collect()
        }
    };
    let mut out = String::new();
    let mut expanded: BTreeSet<Node> = BTreeSet::new();
    for r in &roots {
        walk(g, r, 0, max_depth, &mut Vec::new(), &mut expanded, &mut out);
        out.push('\n');
    }
    out
}

/// `q` is `Method`, `Type`, `Type.Method`, or a fqn prefix thereof (suffix match).
fn matches(n: &Node, q: &str) -> bool {
    let full = format!("{}.{}", n.0, n.1);
    full == q || full.ends_with(&format!(".{q}")) || n.0 == q || n.0.ends_with(&format!(".{q}"))
}

fn walk(
    g: &WriteGraph,
    n: &Node,
    depth: usize,
    max_depth: usize,
    path: &mut Vec<Node>,
    expanded: &mut BTreeSet<Node>,
    out: &mut String,
) {
    let indent = "  ".repeat(depth);
    let simple = n.0.rsplit('.').next().unwrap_or(&n.0);
    let label = format!("{simple}.{}", n.1);
    let sink_note = g
        .sinks
        .get(n)
        .map(|s| format!("  => {}", s.iter().cloned().collect::<Vec<_>>().join(", ")))
        .unwrap_or_default();
    let children = g.edges.get(n);
    if path.contains(n) {
        out.push_str(&format!("{indent}{label} (cycle)\n"));
        return;
    }
    if children.is_some() && !expanded.insert(n.clone()) {
        out.push_str(&format!("{indent}{label}{sink_note} (*)\n"));
        return;
    }
    out.push_str(&format!("{indent}{label}{sink_note}\n"));
    if depth >= max_depth {
        return;
    }
    path.push(n.clone());
    for c in children.into_iter().flatten() {
        walk(g, c, depth + 1, max_depth, path, expanded, out);
    }
    path.pop();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MethodMetric, TypeMetrics};

    fn method(name: &str, inv: &[(&str, &str)]) -> MethodMetric {
        MethodMetric {
            name: name.into(),
            invocations: inv
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    fn project(types: Vec<(&str, TypeMetrics)>) -> BTreeMap<String, TypeMetrics> {
        types.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    #[test]
    fn propagates_through_interface_to_impl() {
        let ctl = TypeMetrics {
            methods: vec![method("Post", &[("_svc", "Place")])],
            service_fields: vec![("_svc".into(), "IOrderService".into())],
            ..Default::default()
        };
        let iface = TypeMetrics {
            methods: vec![method("Place", &[])],
            ..Default::default()
        };
        let imp = TypeMetrics {
            methods: vec![method("Place", &[("_db", "SaveChangesAsync")])],
            bases: vec!["IOrderService".into()],
            ..Default::default()
        };
        let unrelated = TypeMetrics {
            methods: vec![method("Get", &[("_svc", "Read")])],
            service_fields: vec![("_svc".into(), "IOrderService".into())],
            ..Default::default()
        };
        let p = project(vec![
            ("A.OrdersController", ctl),
            ("A.IOrderService", iface),
            ("A.OrderService", imp),
            ("A.Reader", unrelated),
        ]);
        let sinks: BTreeSet<String> = DEFAULT_SINKS.iter().map(|s| s.to_string()).collect();
        let g = build_from_types(&[&p], &sinks, false, &BTreeSet::new());
        assert_eq!(
            g.roots,
            vec![("A.OrdersController".to_string(), "Post".to_string())]
        );
        let text = render(&g, None, 10);
        assert!(text.contains("OrdersController.Post"));
        assert!(text.contains("  OrderService.Place  => _db.SaveChangesAsync"));
        assert!(!text.contains("Reader"));
    }

    #[test]
    fn cycles_terminate() {
        let a = TypeMetrics {
            methods: vec![method("F", &[("B", "G"), ("_db", "SaveChanges")])],
            ..Default::default()
        };
        let b = TypeMetrics {
            methods: vec![method("G", &[("A", "F")])],
            ..Default::default()
        };
        let p = project(vec![("A", a), ("B", b)]);
        let sinks: BTreeSet<String> = DEFAULT_SINKS.iter().map(|s| s.to_string()).collect();
        let g = build_from_types(&[&p], &sinks, false, &BTreeSet::new());
        assert!(!g.roots.is_empty());
        let _ = render(&g, None, 10);
    }

    #[test]
    fn test_types_are_filtered_unless_included() {
        let t = TypeMetrics {
            methods: vec![method("Seed", &[("_db", "SaveChanges")])],
            ..Default::default()
        };
        let attr_only = TypeMetrics {
            methods: vec![MethodMetric {
                name: "Works".into(),
                attributes: vec!["Fact".into()],
                invocations: vec![("_db".into(), "SaveChanges".into())],
                ..Default::default()
            }],
            ..Default::default()
        };
        let p = project(vec![("A.ChatServiceTest", t), ("A.Misc", attr_only)]);
        let sinks: BTreeSet<String> = DEFAULT_SINKS.iter().map(|s| s.to_string()).collect();
        assert!(build_from_types(&[&p], &sinks, false, &BTreeSet::new())
            .roots
            .is_empty());
        assert_eq!(
            build_from_types(&[&p], &sinks, true, &BTreeSet::new())
                .roots
                .len(),
            2
        );
    }

    #[test]
    fn entry_attr_on_interface_marks_impl_as_entry() {
        let iface = TypeMetrics {
            methods: vec![MethodMetric {
                name: "Save".into(),
                attributes: vec!["OperationContract".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let imp = TypeMetrics {
            methods: vec![
                method("Save", &[("_db", "SaveChanges")]),
                method("Helper", &[("_db", "SaveChanges")]),
            ],
            bases: vec!["ISvc".into()],
            ..Default::default()
        };
        let p = project(vec![("A.ISvc", iface), ("A.Svc", imp)]);
        let sinks: BTreeSet<String> = DEFAULT_SINKS.iter().map(|s| s.to_string()).collect();
        let entries: BTreeSet<String> = ["OperationContract".to_string()].into();
        let g = build_from_types(&[&p], &sinks, false, &entries);
        assert_eq!(g.roots, vec![("A.Svc".to_string(), "Save".to_string())]);
    }
}
