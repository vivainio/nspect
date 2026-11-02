//! Lens-based call tracing: tag methods that call a *sink* (a `(receiver,
//! method)` pattern such as `SaveChanges` or `Task.WhenAll`) and propagate the
//! tag upward through the call graph. A *lens* is a named sink set; `writes`
//! and `threads` are built in, more can be declared in
//! `.nspect/spec/lenses.yaml`.
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

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::model::{Project, TypeMetrics};

/// `writes` lens: method names that persist to a database.
pub const WRITE_SINKS: &[&str] = &[
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

/// `threads` lens: concurrency primitives. `Recv.Method` entries match a call
/// on that receiver (a static type name like `Task`, or a field whose declared
/// type is that name); a bare name matches the method on any receiver.
pub const THREAD_SINKS: &[&str] = &[
    "Task.WhenAll",
    "Task.WhenAny",
    "Task.WaitAll",
    "Task.WaitAny",
    "Task.Run",
    "Parallel.For",
    "Parallel.ForEach",
    "Parallel.ForEachAsync",
    "Parallel.Invoke",
    "ThreadPool.QueueUserWorkItem",
    "AsParallel",
];

/// One sink pattern: `Method` (any receiver) or `Recv.Method`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkPattern {
    pub recv: Option<String>,
    pub method: String,
}

impl SinkPattern {
    pub fn parse(s: &str) -> Self {
        match s.rsplit_once('.') {
            Some((r, m)) if !r.is_empty() && !m.is_empty() => SinkPattern {
                recv: Some(r.to_string()),
                method: m.to_string(),
            },
            _ => SinkPattern {
                recv: None,
                method: s.to_string(),
            },
        }
    }

    /// `field_ty` is the declared type of `recv` when it names a field.
    fn matches(&self, recv: &str, field_ty: Option<&str>, name: &str) -> bool {
        self.method == name
            && match &self.recv {
                None => true,
                Some(r) => r == recv || Some(r.as_str()) == field_ty,
            }
    }
}

/// A named question asked of the call graph.
#[derive(Debug, Clone)]
pub struct Lens {
    pub name: String,
    pub sinks: Vec<SinkPattern>,
    /// Entry attributes for `--entries`; empty means `DEFAULT_ENTRY_ATTRS`.
    pub entry_attrs: Vec<String>,
}

impl Lens {
    fn from_names(name: &str, sinks: &[&str]) -> Self {
        Lens {
            name: name.to_string(),
            sinks: sinks.iter().map(|s| SinkPattern::parse(s)).collect(),
            entry_attrs: Vec::new(),
        }
    }
}

pub fn builtin_lenses() -> Vec<Lens> {
    vec![
        Lens::from_names("writes", WRITE_SINKS),
        Lens::from_names("threads", THREAD_SINKS),
    ]
}

#[derive(Debug, Deserialize)]
struct LensFile {
    #[serde(default)]
    lenses: BTreeMap<String, LensDef>,
}

#[derive(Debug, Deserialize)]
struct LensDef {
    #[serde(default)]
    sinks: Vec<String>,
    #[serde(default)]
    entry_attrs: Vec<String>,
}

/// Parse a `lenses.yaml` document.
pub fn parse_lens_file(text: &str) -> Result<Vec<Lens>> {
    let f: LensFile = serde_yaml::from_str(text)?;
    Ok(f.lenses
        .into_iter()
        .map(|(name, d)| Lens {
            name,
            sinks: d.sinks.iter().map(|s| SinkPattern::parse(s)).collect(),
            entry_attrs: d.entry_attrs,
        })
        .collect())
}

/// Find a lens by name: `<root>/.nspect/spec/lenses.yaml` first (so a repo can
/// override a built-in), then the built-ins.
pub fn resolve_lens(name: &str, root: &Path) -> Result<Lens> {
    let spec = root.join(".nspect").join("spec").join("lenses.yaml");
    let mut available: Vec<String> = Vec::new();
    if spec.is_file() {
        let text = std::fs::read_to_string(&spec)
            .with_context(|| format!("reading {}", spec.display()))?;
        for l in parse_lens_file(&text).with_context(|| format!("parsing {}", spec.display()))? {
            if l.name == name {
                return Ok(l);
            }
            available.push(l.name);
        }
    }
    for l in builtin_lenses() {
        if l.name == name {
            return Ok(l);
        }
        available.push(l.name);
    }
    anyhow::bail!("unknown lens `{name}`; available: {}", available.join(", "))
}

/// A method node: `(type fqn, method name)`. Overloads share a node.
pub type Node = (String, String);

#[derive(Debug, Default)]
pub struct TraceGraph {
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

pub fn build_with(
    projects: &[Project],
    sinks: &[SinkPattern],
    include_tests: bool,
    entry_attrs: &BTreeSet<String>,
) -> TraceGraph {
    let maps: Vec<&BTreeMap<String, TypeMetrics>> = projects
        .iter()
        .filter(|p| include_tests || !is_test_project(&p.name))
        .map(|p| &p.type_metrics)
        .collect();
    build_from_types(&maps, sinks, include_tests, entry_attrs)
}

pub fn build_from_types(
    maps: &[&BTreeMap<String, TypeMetrics>],
    sinks: &[SinkPattern],
    include_tests: bool,
    entry_attrs: &BTreeSet<String>,
) -> TraceGraph {
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
                let field_ty = fields.get(recv.as_str()).copied();
                if sinks.iter().any(|p| p.matches(recv, field_ty, name)) {
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
        return TraceGraph {
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

    TraceGraph {
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
pub fn render(g: &TraceGraph, from: Option<&str>, max_depth: usize) -> String {
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
    g: &TraceGraph,
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

    fn write_sinks() -> Vec<SinkPattern> {
        WRITE_SINKS.iter().map(|s| SinkPattern::parse(s)).collect()
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
        let sinks = write_sinks();
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
        let sinks = write_sinks();
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
        let sinks = write_sinks();
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
        let sinks = write_sinks();
        let entries: BTreeSet<String> = ["OperationContract".to_string()].into();
        let g = build_from_types(&[&p], &sinks, false, &entries);
        assert_eq!(g.roots, vec![("A.Svc".to_string(), "Save".to_string())]);
    }

    #[test]
    fn threads_lens_matches_receiver_and_method() {
        let a = TypeMetrics {
            methods: vec![
                method("Fan", &[("Task", "WhenAll")]),
                method("Other", &[("_x", "Run")]),
                method("ViaField", &[("_p", "Run")]),
            ],
            service_fields: vec![("_p".into(), "Task".into())],
            ..Default::default()
        };
        let p = project(vec![("A.Svc", a)]);
        let lens = Lens::from_names("threads", THREAD_SINKS);
        let g = build_from_types(&[&p], &lens.sinks, false, &BTreeSet::new());
        let names: Vec<&str> = g.roots.iter().map(|n| n.1.as_str()).collect();
        assert_eq!(names, vec!["Fan", "ViaField"]);
    }

    #[test]
    fn user_lens_file_parses_and_patterns_split() {
        let l = parse_lens_file(
            "lenses:\n  http:\n    sinks: [HttpClient.GetAsync, SendAsync]\n    entry_attrs: [Route]\n",
        )
        .unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].name, "http");
        assert_eq!(
            l[0].sinks,
            vec![
                SinkPattern {
                    recv: Some("HttpClient".into()),
                    method: "GetAsync".into()
                },
                SinkPattern {
                    recv: None,
                    method: "SendAsync".into()
                },
            ]
        );
        assert_eq!(l[0].entry_attrs, vec!["Route".to_string()]);
        assert!(resolve_lens("nope", Path::new("/nonexistent")).is_err());
        assert_eq!(
            resolve_lens("threads", Path::new("/nonexistent"))
                .unwrap()
                .name,
            "threads"
        );
    }
}
