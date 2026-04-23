use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use crate::{
    atlas, classes, cpm, csproj, discovery, graph::ProjectGraph, lookup, metrics, model::Project,
    references, report, sln, source_scan,
};

#[derive(Debug, Parser)]
#[command(
    name = "nspect",
    version,
    about = "Analyze the structure of C# projects and solutions"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List projects found under <path> with their target frameworks and package references.
    Scan(ScanArgs),
    /// Build a project-to-project dependency graph and emit DOT / Mermaid / JSON.
    Graph(GraphArgs),
    /// Dump the tree-sitter C# parse of a single `.cs` file (S-expression + extracted `using`s).
    TsDump(TsDumpArgs),
    /// Emit a structural snapshot (areas, projects, edges, layers) as JSON.
    Atlas(AtlasArgs),
    /// Text summary of structural metrics for a path (repo root or subdir).
    Metrics(MetricsArgs),
    /// Visualize the dependency neighborhood of a single project.
    Focus(FocusArgs),
    /// Report everything the atlas artifacts know about a type (declaring
    /// project, namespace, metrics, cross-project callers).
    Lookup(LookupArgs),
}

#[derive(Debug, Parser)]
pub struct LookupArgs {
    /// Type name (simple, e.g. `Customer`) or fully-qualified (e.g.
    /// `Acme.Domain.Customer`). Nested types may be passed with their
    /// dotted local path.
    pub name: String,
    /// Directory containing `atlas.yaml` / `classes.yaml` / `metrics.yaml` /
    /// `references.yaml`. Defaults to the current directory.
    #[arg(long, default_value = ".")]
    pub atlas_dir: PathBuf,
}

pub fn run_lookup(args: LookupArgs) -> Result<()> {
    let result = lookup::run(&args.atlas_dir, &args.name)?;
    print!("{}", serde_yaml::to_string(&result)?);
    Ok(())
}

#[derive(Debug, Parser)]
pub struct FocusArgs {
    /// Repository root, a `.sln`, or a `.csproj` file.
    pub path: PathBuf,
    /// Project name (exact, suffix, or unique substring match).
    pub project: String,
    /// How many hops of reverse refs to include (projects that depend on this one).
    #[arg(long, default_value_t = 1)]
    pub up: u32,
    /// How many hops of forward refs to include (projects this one depends on).
    #[arg(long, default_value_t = 1)]
    pub down: u32,
    #[arg(long, value_enum, default_value_t = GraphFormat::Text)]
    pub format: GraphFormat,
}

pub fn run_focus(args: FocusArgs) -> Result<()> {
    let projects = load_projects(&args.path)?;
    let atlas_model = atlas::build(projects, &args.path, atlas::AtlasOptions::default());
    let focus_id = match atlas::resolve_project(&atlas_model, &args.project) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let view = atlas::focus(&atlas_model, &focus_id, args.up, args.down);
    let out = match args.format {
        GraphFormat::Dot => view.to_dot(),
        GraphFormat::Mermaid => view.to_mermaid(),
        GraphFormat::Text => view.to_text(),
        GraphFormat::Json => serde_json::to_string_pretty(&serde_json::json!({
            "focus": focus_id,
            "nodes": view.nodes.iter().map(|p| &p.id).collect::<Vec<_>>(),
            "edges": view.edges.iter().map(|(f, t)| [f, t]).collect::<Vec<_>>(),
        }))?,
    };
    println!("{out}");
    Ok(())
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum AtlasFormat {
    Json,
    Yaml,
}

#[derive(Debug, Parser)]
pub struct AtlasArgs {
    /// Repository root, a `.sln`, or a `.csproj` file.
    pub path: PathBuf,
    #[arg(long, value_enum, default_value_t = AtlasFormat::Yaml)]
    pub format: AtlasFormat,
    /// Emit compact single-line JSON (has no effect on YAML output).
    #[arg(long)]
    pub compact: bool,
    /// Write multiple artifacts into this directory instead of printing to
    /// stdout. Produces `atlas.<ext>`, `classes.<ext>`, `metrics.<ext>`, and
    /// (with `--check`) `checks.<ext>`.
    #[arg(long)]
    pub output_dir: Option<PathBuf>,
    /// Run findings (cycles, orphans, unresolved refs, version conflicts,
    /// unused/undeclared packages) and embed them under `findings` in
    /// `atlas.<ext>`. With `--output-dir`, also writes `checks.<ext>`.
    #[arg(long)]
    pub check: bool,
    /// Resolve each project's type-position tokens against the cross-project
    /// declarations catalog. With `--output-dir`, writes `references.<ext>`.
    /// Requires the tree-sitter source scan and so implies `--classes`
    /// (already produced in `--output-dir` mode).
    #[arg(long)]
    pub references: bool,
}

#[derive(Debug, Parser)]
pub struct MetricsArgs {
    /// Repository root, a subdirectory, a `.sln`, or a `.csproj` file.
    pub path: PathBuf,
    /// How many top methods by complexity to list (0 disables the section).
    #[arg(long, default_value_t = 20)]
    pub top: usize,
    /// Restrict the top-methods section to a single project (name or suffix).
    #[arg(long)]
    pub project: Option<String>,
}

pub fn run_metrics(args: MetricsArgs) -> Result<()> {
    let mut projects = load_projects(&args.path)?;
    apply_source_scan(&mut projects)?;

    let mut rows: Vec<(String, metrics::ProjectTotals)> = projects
        .iter()
        .map(|p| (p.name.clone(), metrics::project_totals(p)))
        .collect();
    rows.sort_by(|a, b| b.1.complexity.cmp(&a.1.complexity).then(a.0.cmp(&b.0)));

    let name_w = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(4).max(12);
    println!(
        "{:<name_w$}  {:>6}  {:>7}  {:>7}  {:>10}",
        "project",
        "types",
        "loc",
        "members",
        "complexity",
        name_w = name_w
    );
    println!("{}", "-".repeat(name_w + 2 + 6 + 2 + 7 + 2 + 7 + 2 + 10));
    let mut grand = metrics::ProjectTotals::default();
    for (name, t) in &rows {
        println!(
            "{:<name_w$}  {:>6}  {:>7}  {:>7}  {:>10}",
            name,
            t.types,
            t.loc,
            t.members,
            t.complexity,
            name_w = name_w
        );
        grand.types += t.types;
        grand.loc += t.loc;
        grand.members += t.members;
        grand.complexity += t.complexity;
    }
    println!("{}", "-".repeat(name_w + 2 + 6 + 2 + 7 + 2 + 7 + 2 + 10));
    println!(
        "{:<name_w$}  {:>6}  {:>7}  {:>7}  {:>10}",
        "TOTAL",
        grand.types,
        grand.loc,
        grand.members,
        grand.complexity,
        name_w = name_w
    );

    if args.top > 0 {
        print_top_methods(&projects, &args.project, args.top);
    }

    Ok(())
}

fn print_top_methods(projects: &[Project], filter: &Option<String>, top: usize) {
    let matches_project = |p: &Project| -> bool {
        match filter {
            None => true,
            Some(q) => p.name == *q || p.name.ends_with(&format!(".{q}")) || p.name.contains(q),
        }
    };

    // Flatten: (project_name, type_fqn, method_name, loc, complexity)
    let mut methods: Vec<(String, String, String, u32, u32)> = Vec::new();
    for p in projects {
        if !matches_project(p) {
            continue;
        }
        for (type_fqn, tm) in &p.type_metrics {
            for mm in &tm.methods {
                methods.push((
                    p.name.clone(),
                    type_fqn.clone(),
                    mm.name.clone(),
                    mm.loc,
                    mm.complexity,
                ));
            }
        }
    }
    if methods.is_empty() {
        return;
    }
    methods.sort_by(|a, b| b.4.cmp(&a.4).then(b.3.cmp(&a.3)).then(a.0.cmp(&b.0)));

    // When only one project is in scope, drop the redundant `project::` prefix.
    let distinct_projects: std::collections::HashSet<&str> =
        methods.iter().map(|m| m.0.as_str()).collect();
    let single = distinct_projects.len() == 1;

    let header = match filter {
        None => format!("top {} methods by complexity", top.min(methods.len())),
        Some(q) => format!(
            "top {} methods by complexity in {}",
            top.min(methods.len()),
            q
        ),
    };
    println!("\n{header}:");
    let loc_w = 5;
    let cx_w = 10;
    for (project, type_fqn, method, loc, cx) in methods.iter().take(top) {
        if single {
            println!(
                "  {cx:>cx_w$}  {loc:>loc_w$}  {type_fqn}.{method}",
                cx = cx,
                loc = loc,
                cx_w = cx_w,
                loc_w = loc_w,
            );
        } else {
            println!(
                "  {cx:>cx_w$}  {loc:>loc_w$}  {project}::{type_fqn}.{method}",
                cx = cx,
                loc = loc,
                cx_w = cx_w,
                loc_w = loc_w,
            );
        }
    }
}

pub fn run_atlas(args: AtlasArgs) -> Result<()> {
    let mut projects = load_projects(&args.path)?;
    let opts = atlas::AtlasOptions { check: args.check };

    // Tree-sitter source pass is needed for the classes/metrics artifacts
    // (output-dir case), for the package-ref findings inside `--check`, and
    // for the references resolution.
    if args.output_dir.is_some() || args.check || args.references {
        apply_source_scan(&mut projects)?;
    }

    match &args.output_dir {
        None => {
            if args.references {
                let refs = references::build(&projects, &args.path);
                print!("{}", encode_atlas(&refs, args.format, args.compact)?);
            } else {
                let atlas_model = atlas::build(projects, &args.path, opts);
                print!("{}", encode_atlas(&atlas_model, args.format, args.compact)?);
            }
        }
        Some(dir) => {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            let ext = match args.format {
                AtlasFormat::Json => "json",
                AtlasFormat::Yaml => "yaml",
            };
            let classes_snapshot = classes::build(&projects, &args.path);
            let metrics_snapshot = metrics::build(&projects, &args.path);
            let references_snapshot = if args.references {
                Some(references::build(&projects, &args.path))
            } else {
                None
            };
            let atlas_model = atlas::build(projects, &args.path, opts);

            write_artifact(
                &dir.join(format!("atlas.{ext}")),
                &encode_atlas(&atlas_model, args.format, args.compact)?,
            )?;
            write_artifact(
                &dir.join(format!("classes.{ext}")),
                &encode_atlas(&classes_snapshot, args.format, args.compact)?,
            )?;
            write_artifact(
                &dir.join(format!("metrics.{ext}")),
                &encode_atlas(&metrics_snapshot, args.format, args.compact)?,
            )?;
            if args.check {
                write_artifact(
                    &dir.join(format!("checks.{ext}")),
                    &encode_atlas(&atlas_model.findings, args.format, args.compact)?,
                )?;
            }
            if let Some(refs) = &references_snapshot {
                write_artifact(
                    &dir.join(format!("references.{ext}")),
                    &encode_atlas(refs, args.format, args.compact)?,
                )?;
            }
        }
    }
    Ok(())
}

fn encode_atlas<T: serde::Serialize>(v: &T, format: AtlasFormat, compact: bool) -> Result<String> {
    Ok(match format {
        AtlasFormat::Json if compact => serde_json::to_string(v)?,
        AtlasFormat::Json => serde_json::to_string_pretty(v)?,
        AtlasFormat::Yaml => serde_yaml::to_string(v)?,
    })
}

fn write_artifact(path: &std::path::Path, body: &str) -> Result<()> {
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    eprintln!("wrote {}", path.display());
    Ok(())
}

#[derive(Debug, Parser)]
pub struct TsDumpArgs {
    pub file: PathBuf,
    /// Print the full S-expression tree (can be large).
    #[arg(long)]
    pub sexp: bool,
}

pub fn run_ts_dump(args: TsDumpArgs) -> Result<()> {
    use tree_sitter::Parser;

    let src = std::fs::read_to_string(&args.file)?;
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_c_sharp::language())
        .map_err(|e| anyhow::anyhow!("set_language: {e}"))?;
    let tree = parser
        .parse(&src, None)
        .ok_or_else(|| anyhow::anyhow!("parse returned None"))?;

    let root = tree.root_node();
    println!("file: {}", args.file.display());
    println!(
        "root: kind={}  named_children={}  bytes={}  has_error={}",
        root.kind(),
        root.named_child_count(),
        root.end_byte(),
        root.has_error(),
    );

    println!("\nusings ({}):", "extracted");
    for u in source_scan::extract_usings(&src)? {
        println!("  {u}");
    }

    println!("\ntop-level named children:");
    let mut tc = root.walk();
    for child in root.named_children(&mut tc) {
        let snippet = child
            .utf8_text(src.as_bytes())
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(80)
            .collect::<String>();
        println!(
            "  [{}..{}] {:<28} {snippet}",
            child.start_position().row + 1,
            child.end_position().row + 1,
            child.kind(),
        );
    }

    if args.sexp {
        println!(
            "\nS-expression (with leaf text):\n{}",
            sexp_with_text(root, src.as_bytes(), 0)
        );
    }

    Ok(())
}

/// Render a tree-sitter node as an indented S-expression where leaf tokens also
/// carry their source text — e.g. `(identifier "GetAsync")` rather than bare
/// `(identifier)`. `to_sexp()` doesn't include leaf text.
fn sexp_with_text(node: tree_sitter::Node<'_>, src: &[u8], depth: usize) -> String {
    let mut out = String::new();
    let indent = "  ".repeat(depth);
    out.push_str(&indent);
    out.push('(');
    out.push_str(node.kind());

    let named_count = node.named_child_count();
    if named_count == 0 {
        // Leaf — attach the source text so you can actually see the symbol.
        if let Ok(text) = node.utf8_text(src) {
            let one_line: String = text
                .chars()
                .take(80)
                .map(|c| if c == '\n' { ' ' } else { c })
                .collect();
            let escaped = one_line.replace('\\', "\\\\").replace('"', "\\\"");
            out.push_str(&format!(" \"{escaped}\""));
        }
        out.push(')');
        return out;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        out.push('\n');
        // Prefix field name if there is one.
        if let Some(field) = node.field_name_for_child(child_index(node, child)) {
            out.push_str(&"  ".repeat(depth + 1));
            out.push_str(field);
            out.push_str(":\n");
            // The child itself on its own line:
            let inner = sexp_with_text(child, src, depth + 1);
            out.push_str(&inner);
        } else {
            out.push_str(&sexp_with_text(child, src, depth + 1));
        }
    }
    out.push(')');
    out
}

fn child_index(parent: tree_sitter::Node<'_>, target: tree_sitter::Node<'_>) -> u32 {
    let mut cursor = parent.walk();
    let mut idx = 0u32;
    for c in parent.children(&mut cursor) {
        if c.id() == target.id() {
            return idx;
        }
        idx += 1;
    }
    0
}

#[allow(dead_code)]
fn pretty_sexp(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    let mut depth: usize = 0;
    let mut at_line_start = true;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '(' => {
                if !at_line_start {
                    out.push('\n');
                }
                for _ in 0..depth {
                    out.push_str("  ");
                }
                out.push('(');
                depth += 1;
                at_line_start = false;
            }
            ')' => {
                depth = depth.saturating_sub(1);
                out.push(')');
                at_line_start = false;
            }
            ' ' => {
                // Collapse runs of whitespace.
                while matches!(chars.peek(), Some(' ')) {
                    chars.next();
                }
                out.push(' ');
            }
            other => {
                out.push(other);
                at_line_start = false;
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum GraphFormat {
    Dot,
    Mermaid,
    Json,
    Text,
}

#[derive(Debug, Parser)]
pub struct GraphArgs {
    pub path: PathBuf,
    #[arg(long, value_enum, default_value_t = GraphFormat::Dot)]
    pub format: GraphFormat,
    /// Include NuGet package nodes. Off by default — inter-project edges are the primary signal.
    #[arg(long)]
    pub packages: bool,
}

pub fn run_graph(args: GraphArgs) -> Result<()> {
    let projects = load_projects(&args.path)?;
    let g = if args.packages {
        ProjectGraph::build_with_packages(projects)
    } else {
        ProjectGraph::build(projects)
    };
    let out = match args.format {
        GraphFormat::Dot => g.to_dot(),
        GraphFormat::Mermaid => g.to_mermaid(),
        GraphFormat::Json => g.to_json()?,
        GraphFormat::Text => report::graph_text(&g),
    };
    println!("{out}");
    Ok(())
}

#[derive(Debug, Parser)]
pub struct ScanArgs {
    /// Repository root, a `.sln`, or a `.csproj` file.
    pub path: PathBuf,
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
    /// Run tree-sitter pass over `.cs` sources to populate `usings`,
    /// `declared_namespaces`, and `declared_types` on each project.
    #[arg(long)]
    pub source_scan: bool,
}

pub fn run_scan(args: ScanArgs) -> Result<()> {
    let mut projects = load_projects(&args.path)?;
    if args.source_scan {
        apply_source_scan(&mut projects)?;
    }
    let out = match args.format {
        OutputFormat::Text => report::scan_text(&projects),
        OutputFormat::Json => report::scan_json(&projects)?,
    };
    println!("{out}");
    Ok(())
}

/// Collect unique projects reachable from the given path.
pub fn load_projects(root: &std::path::Path) -> Result<Vec<Project>> {
    let mut csproj_paths: Vec<PathBuf> = Vec::new();

    if root.is_file() {
        match root.extension().and_then(|e| e.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("sln") => {
                for p in sln::parse(root)? {
                    csproj_paths.push(p.path);
                }
            }
            Some(ext) if ext.eq_ignore_ascii_case("csproj") => {
                csproj_paths.push(root.to_path_buf());
            }
            _ => anyhow::bail!("unsupported file: {}", root.display()),
        }
    } else {
        let found = discovery::discover(root)?;
        if !found.solutions.is_empty() {
            for sln_path in &found.solutions {
                for p in sln::parse(sln_path)? {
                    csproj_paths.push(p.path);
                }
            }
            // Fall back to any additional csproj found on disk but not in a sln.
            for p in found.projects {
                csproj_paths.push(p);
            }
        } else {
            csproj_paths.extend(found.projects);
        }
    }

    // Deduplicate via canonical path.
    let mut seen = std::collections::BTreeSet::new();
    let mut projects = Vec::new();
    for p in csproj_paths {
        let canon = csproj::canonicalize(&p);
        if !seen.insert(canon.clone()) {
            continue;
        }
        if !canon.exists() {
            tracing::warn!("referenced csproj not found: {}", canon.display());
            continue;
        }
        match csproj::parse(&canon) {
            Ok(mut project) => {
                apply_cpm(&mut project)?;
                projects.push(project);
            }
            Err(e) => {
                tracing::warn!("skipping {}: {e:#}", canon.display());
            }
        }
    }
    projects.sort_by(|a, b| a.name.cmp(&b.name));
    resolve_assembly_refs(&mut projects);
    Ok(projects)
}

/// Resolve bare `<Reference Include="X"/>` assembly refs against sibling projects
/// in the same load. Legacy .NET Framework csprojs depend on sibling projects
/// this way (no HintPath means "same output directory"). When the simple
/// assembly name matches another project's name / AssemblyName, promote the
/// ref to a real project-ref so the graph sees the edge.
///
/// Unresolved entries stay in `assembly_refs` as external refs.
pub fn resolve_assembly_refs(projects: &mut [Project]) {
    use std::collections::HashMap;
    let mut by_name: HashMap<String, PathBuf> = HashMap::new();
    for p in projects.iter() {
        by_name.insert(p.name.to_lowercase(), p.path.clone());
    }
    for p in projects.iter_mut() {
        let mut resolved: Vec<PathBuf> = Vec::new();
        let mut external: Vec<String> = Vec::new();
        for asm in p.assembly_refs.drain(..) {
            let simple = asm.split(',').next().unwrap_or(&asm).trim();
            if let Some(path) = by_name.get(&simple.to_lowercase()) {
                if *path != p.path {
                    resolved.push(path.clone());
                    continue;
                }
            }
            external.push(asm);
        }
        p.assembly_refs = external;
        let existing: std::collections::HashSet<PathBuf> = p
            .project_refs
            .iter()
            .map(|r| csproj::canonicalize(r))
            .collect();
        for path in resolved {
            if !existing.contains(&path) {
                p.project_refs.push(path);
            }
        }
    }
}

/// Run source scan (tree-sitter) over each project and attach the discovered usings.
pub fn apply_source_scan(projects: &mut [Project]) -> Result<()> {
    let scans = source_scan::scan_projects(projects)?;
    for (p, s) in projects.iter_mut().zip(scans) {
        p.usings = s.usings;
        p.declared_namespaces = s.declared_namespaces;
        p.declared_types = s.declared_types;
        p.type_metrics = s.type_metrics;
        p.referenced_types = s.referenced_types;
    }
    Ok(())
}

/// Fill in missing PackageReference versions from the nearest Directory.Packages.props.
fn apply_cpm(project: &mut Project) -> Result<()> {
    let needs_cpm = project.package_refs.iter().any(|p| p.version.is_none());
    if !needs_cpm {
        return Ok(());
    }
    let Some(cpm) = cpm::find_for(&project.path)? else {
        return Ok(());
    };
    for pkg in &mut project.package_refs {
        if pkg.version.is_none() {
            if let Some(v) = cpm.versions.get(&pkg.name) {
                pkg.version = Some(v.clone());
            }
        }
    }
    Ok(())
}
