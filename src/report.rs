use comfy_table::{presets::UTF8_FULL, Cell, Table};
use owo_colors::OwoColorize;

use crate::analysis::{Finding, Severity};
use crate::graph::ProjectGraph;
use crate::model::Project;

pub fn scan_text(projects: &[Project]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{} {} project(s)\n\n",
        "Found".green().bold(),
        projects.len()
    ));

    let mut table = Table::new();
    table.load_preset(UTF8_FULL);
    table.set_header(vec![
        "Project",
        "SDK",
        "TargetFramework(s)",
        "Pkgs",
        "ProjRefs",
    ]);
    for p in projects {
        table.add_row(vec![
            Cell::new(&p.name),
            Cell::new(if p.sdk_style { "sdk" } else { "legacy" }),
            Cell::new(p.target_frameworks.join(", ")),
            Cell::new(p.package_refs.len()),
            Cell::new(p.project_refs.len()),
        ]);
    }
    out.push_str(&table.to_string());
    out.push('\n');

    for p in projects {
        out.push_str(&format!("\n{} {}\n", "■".cyan(), p.name.bold()));
        out.push_str(&format!("  path: {}\n", p.path.display()));
        if !p.package_refs.is_empty() {
            out.push_str("  packages:\n");
            for pkg in &p.package_refs {
                let ver = pkg.version.as_deref().unwrap_or("<cpm>");
                out.push_str(&format!("    - {} {}\n", pkg.name, ver.dimmed()));
            }
        }
        if !p.project_refs.is_empty() {
            out.push_str("  project refs:\n");
            for r in &p.project_refs {
                out.push_str(&format!("    - {}\n", r.display()));
            }
        }
    }
    out
}

pub fn scan_json(projects: &[Project]) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(projects)?)
}

pub fn graph_text(g: &ProjectGraph) -> String {
    use petgraph::visit::EdgeRef;
    let mut out = String::new();
    out.push_str(&format!(
        "{} {} project(s), {} edge(s)\n\n",
        "Graph:".green().bold(),
        g.projects.len(),
        g.graph.edge_count()
    ));

    for (&id, project) in &g.projects {
        out.push_str(&format!("{} {}\n", "■".cyan(), project.name.bold()));
        let idx = g.project_nodes[&id];
        let mut targets: Vec<String> = g
            .graph
            .edges(idx)
            .map(|e| match &g.graph[e.target()] {
                crate::graph::Node::Project(pid) => g.name(*pid).to_string(),
                crate::graph::Node::Package(name) => format!("📦 {name}"),
            })
            .collect();
        targets.sort();
        for t in targets {
            out.push_str(&format!("    → {t}\n"));
        }
    }

    let cycles = g.cycles();
    if !cycles.is_empty() {
        out.push_str(&format!("\n{}\n", "Cycles:".red().bold()));
        for c in cycles {
            let names: Vec<&str> = c.iter().map(|id| g.name(*id)).collect();
            out.push_str(&format!("  - {}\n", names.join(" → ")));
        }
    }

    let orphans = g.orphans();
    if !orphans.is_empty() {
        out.push_str(&format!("\n{}\n", "Orphans:".yellow().bold()));
        for id in orphans {
            out.push_str(&format!("  - {}\n", g.name(id)));
        }
    }

    if !g.unresolved.is_empty() {
        out.push_str(&format!(
            "\n{}\n",
            "Unresolved project refs:".yellow().bold()
        ));
        for u in &g.unresolved {
            out.push_str(&format!(
                "  - {} → {}\n",
                g.name(u.from),
                u.target.display()
            ));
        }
    }

    out
}

pub fn findings_text(findings: &[Finding]) -> String {
    if findings.is_empty() {
        return format!("{} no findings\n", "✓".green().bold());
    }
    let mut out = String::new();
    let mut errors = 0;
    let mut warnings = 0;
    let mut infos = 0;
    for f in findings {
        match f.severity() {
            Severity::Error => errors += 1,
            Severity::Warning => warnings += 1,
            Severity::Info => infos += 1,
        }
    }
    out.push_str(&format!(
        "{} {} error(s), {} warning(s), {} info\n\n",
        "Findings:".bold(),
        errors.to_string().red(),
        warnings.to_string().yellow(),
        infos.to_string().dimmed(),
    ));

    for f in findings {
        let tag = match f.severity() {
            Severity::Error => "ERROR  ".red().to_string(),
            Severity::Warning => "WARN   ".yellow().to_string(),
            Severity::Info => "INFO   ".dimmed().to_string(),
        };
        match f {
            Finding::Cycle { projects } => {
                out.push_str(&format!("{tag} cycle: {}\n", projects.join(" → ")));
            }
            Finding::OrphanProject { project } => {
                out.push_str(&format!("{tag} orphan: {project}\n"));
            }
            Finding::UnresolvedProjectRef { project, target } => {
                out.push_str(&format!(
                    "{tag} unresolved ref: {project} → {}\n",
                    target.display()
                ));
            }
            Finding::VersionConflict { package, versions } => {
                out.push_str(&format!("{tag} version conflict: {package}\n"));
                for (proj, ver) in versions {
                    out.push_str(&format!("           {proj}: {ver}\n"));
                }
            }
            Finding::UnusedPackageRef { project, package } => {
                out.push_str(&format!("{tag} unused package: {project} → {package}\n"));
            }
            Finding::UndeclaredUsage { project, namespace } => {
                out.push_str(&format!(
                    "{tag} undeclared usage: {project} imports `{namespace}`\n"
                ));
            }
            Finding::ForbiddenAreaEdge {
                from_project,
                from_area,
                to_project,
                to_area,
                reason,
            } => {
                out.push_str(&format!(
                    "{tag} forbidden area edge: {from_project} [{from_area}] → {to_project} [{to_area}] — {reason}\n",
                ));
            }
        }
    }
    out
}
