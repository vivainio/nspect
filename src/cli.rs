use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

use crate::{
    analysis, cpm, csproj, discovery, graph::ProjectGraph, model::Project, report, sln,
    source_scan,
};

#[derive(Debug, Parser)]
#[command(name = "nspect", version, about = "Analyze the structure of C# projects and solutions")]
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
    /// Run all findings (cycles, orphans, unresolved refs, version conflicts). Exits non-zero if any error finding is produced.
    Check(CheckArgs),
    /// Dump the tree-sitter C# parse of a single `.cs` file (S-expression + extracted `using`s).
    TsDump(TsDumpArgs),
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
        println!("\nS-expression (with leaf text):\n{}", sexp_with_text(root, src.as_bytes(), 0));
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

#[derive(Debug, Parser)]
pub struct CheckArgs {
    pub path: PathBuf,
    #[arg(long)]
    pub json: bool,
    /// Skip tree-sitter pass over `.cs` sources (disables unused/undeclared checks).
    #[arg(long)]
    pub no_source_scan: bool,
}

pub fn run_check(args: CheckArgs) -> Result<()> {
    let mut projects = load_projects(&args.path)?;
    if !args.no_source_scan {
        apply_source_scan(&mut projects)?;
    }
    let g = ProjectGraph::build(projects);
    let findings = analysis::analyze(&g);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&findings)?);
    } else {
        println!("{}", report::findings_text(&findings));
    }

    let any_error = findings
        .iter()
        .any(|f| f.severity() == analysis::Severity::Error);
    if any_error {
        std::process::exit(1);
    }
    Ok(())
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
}

pub fn run_scan(args: ScanArgs) -> Result<()> {
    let projects = load_projects(&args.path)?;
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
    Ok(projects)
}

/// Run source scan (tree-sitter) over each project and attach the discovered usings.
pub fn apply_source_scan(projects: &mut [Project]) -> Result<()> {
    let scans = source_scan::scan_projects(projects)?;
    for (p, s) in projects.iter_mut().zip(scans) {
        p.usings = s.usings;
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
