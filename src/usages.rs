use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use serde::Deserialize;
use serde_json::Value;
use tree_sitter::Node;

#[derive(Debug, Parser)]
#[command(
    long_about = "Find direct method calls on receivers typed by a library.

Reads compile references from obj/project.assets.json after NuGet restore, plus project Reference/HintPath entries. Use --dll when the assembly is elsewhere. Reports file:line:column, receiver type, and method name.

The scan is best-effort and uses explicit receiver types in source. It does not resolve overloads, inferred types, or chained calls."
)]
pub struct UsagesArgs {
    /// Repository, solution, or project to scan.
    path: PathBuf,
    /// NuGet package ID or assembly name.
    library: String,
    /// Additional reference DLL, applied to all projects. Repeat as needed.
    #[arg(long)]
    dll: Vec<PathBuf>,
}

pub fn run(args: UsagesArgs) -> Result<()> {
    let projects = crate::cli::load_projects(&args.path)?;
    anyhow::ensure!(!projects.is_empty(), "no C# projects found");
    let root = args.path.canonicalize()?;
    let root = if root.is_file() {
        root.parent().unwrap()
    } else {
        &root
    };
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_c_sharp::language())?;
    let mut cache = HashMap::new();
    let mut found_types = false;
    let mut results = BTreeSet::new();

    for project in &projects {
        let dir = project.path.parent().unwrap();
        let mut dlls = compile_references(dir, &args.library)?;
        dlls.extend(hint_paths(&project.path, &args.library)?);
        dlls.extend(args.dll.iter().cloned());
        let mut types = BTreeSet::new();
        for dll in dlls {
            let dll = dll
                .canonicalize()
                .with_context(|| format!("locating {}", dll.display()))?;
            if !cache.contains_key(&dll) {
                cache.insert(dll.clone(), crate::dotnet_dll::defined_types(&dll)?);
            }
            types.extend(cache[&dll].iter().cloned());
        }
        if types.is_empty() {
            continue;
        }
        found_types = true;
        let nested: Vec<_> = projects
            .iter()
            .filter_map(|other| other.path.parent())
            .filter(|other| *other != dir && other.starts_with(dir))
            .collect();
        for file in crate::source_scan::project_source_paths(dir, &nested) {
            let source = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            if let Some(tree) = parser.parse(&source, None) {
                let relative = file.strip_prefix(root).unwrap_or(&file);
                for (line, column, ty, method) in calls(tree.root_node(), &source, &types) {
                    results.insert((relative.to_path_buf(), line, column, ty, method.to_owned()));
                }
            }
        }
    }
    anyhow::ensure!(
        found_types,
        "no types found for {}; restore the projects or supply --dll <path>",
        args.library
    );
    for (file, line, column, ty, method) in results {
        println!("{}:{line}:{column}  {ty}  {method}", file.display());
    }
    Ok(())
}

fn compile_references(dir: &Path, library: &str) -> Result<BTreeSet<PathBuf>> {
    let path = dir.join("obj/project.assets.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let assets: Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    let mut dlls = BTreeSet::new();
    for target in assets["targets"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(_, v)| v)
    {
        for (id, package) in target.as_object().into_iter().flatten() {
            let Some(package_path) = assets["libraries"][id]["path"].as_str() else {
                continue;
            };
            if package["type"] != "package" {
                continue;
            }
            let package_matches = id
                .split('/')
                .next()
                .is_some_and(|name| name.eq_ignore_ascii_case(library));
            for (asset, _) in package["compile"].as_object().into_iter().flatten() {
                let asset_path = Path::new(asset);
                if !asset_path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("dll"))
                    || !(package_matches || assembly_matches(asset_path, library))
                {
                    continue;
                }
                let relative = Path::new(package_path).join(asset_path);
                if let Some(dll) = assets["packageFolders"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(folder, _)| Path::new(folder).join(&relative))
                    .find(|dll| dll.is_file())
                {
                    dlls.insert(dll);
                } else {
                    tracing::warn!(
                        "{} lists missing compile reference {}",
                        path.display(),
                        relative.display()
                    );
                }
            }
        }
    }
    Ok(dlls)
}

fn assembly_matches(path: &Path, library: &str) -> bool {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.eq_ignore_ascii_case(library))
}

#[derive(Deserialize)]
struct ProjectXml {
    #[serde(rename = "$value", default)]
    items: Vec<ProjectItem>,
}

#[derive(Deserialize)]
enum ProjectItem {
    ItemGroup {
        #[serde(rename = "$value", default)]
        items: Vec<GroupItem>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
enum GroupItem {
    Reference(Reference),
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct Reference {
    #[serde(rename = "@Include")]
    include: Option<String>,
    #[serde(rename = "HintPath")]
    hint: Option<String>,
}

fn hint_paths(project: &Path, library: &str) -> Result<Vec<PathBuf>> {
    let source = std::fs::read_to_string(project)?;
    let xml: ProjectXml = quick_xml::de::from_str(&source)
        .with_context(|| format!("reading references in {}", project.display()))?;
    let mut paths = Vec::new();
    let references = xml
        .items
        .into_iter()
        .filter_map(|item| match item {
            ProjectItem::ItemGroup { items } => Some(items),
            ProjectItem::Other => None,
        })
        .flatten();
    for item in references {
        let GroupItem::Reference(reference) = item else {
            continue;
        };
        let Some(hint) = reference.hint else { continue };
        let path = project
            .parent()
            .unwrap()
            .join(hint.trim().replace('\\', "/"));
        if assembly_matches(&path, library)
            || reference
                .include
                .as_deref()
                .and_then(|include| include.split(',').next())
                .is_some_and(|name| name.trim().eq_ignore_ascii_case(library))
        {
            if path.is_file() {
                paths.push(path);
            } else {
                tracing::warn!(
                    "{} lists missing reference {}",
                    project.display(),
                    path.display()
                );
            }
        }
    }
    Ok(paths)
}

struct Binding<'a> {
    name: &'a str,
    ty: &'a str,
    scope: Node<'a>,
    start: usize,
    member: bool,
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

fn ancestors(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    std::iter::successors(node.parent(), |node| node.parent())
}

fn type_scope(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration" | "struct_declaration" | "record_declaration" | "interface_declaration"
    )
}

fn binding<'a>(node: Node<'a>, source: &'a str) -> Option<Binding<'a>> {
    let parent = node.parent()?;
    let (ty, scope, start, member) = match node.kind() {
        "parameter" => (node.child_by_field_name("type"), parent.parent()?, 0, false),
        "property_declaration" => (
            node.child_by_field_name("type"),
            ancestors(node).find(|n| type_scope(n.kind()))?,
            0,
            true,
        ),
        "variable_declarator" => {
            let ty = parent.child_by_field_name("type");
            if parent.parent()?.kind() == "field_declaration" {
                (ty, ancestors(node).find(|n| type_scope(n.kind()))?, 0, true)
            } else {
                let scope = ancestors(node).find(|n| {
                    matches!(
                        n.kind(),
                        "block"
                            | "for_statement"
                            | "using_statement"
                            | "switch_section"
                            | "compilation_unit"
                    )
                })?;
                (ty, scope, node.end_byte(), false)
            }
        }
        _ => return None,
    };
    Some(Binding {
        name: text(node.child_by_field_name("name")?, source),
        ty: ty.map_or("", |ty| text(ty, source)),
        scope,
        start,
        member,
    })
}

fn calls<'a>(
    root: Node<'a>,
    source: &'a str,
    types: &BTreeSet<String>,
) -> Vec<(usize, usize, String, &'a str)> {
    let mut nodes = vec![root];
    let mut imports = Vec::new();
    let mut bindings: HashMap<&str, Vec<Binding<'_>>> = HashMap::new();
    let mut invocations = Vec::new();
    while let Some(node) = nodes.pop() {
        let mut cursor = node.walk();
        nodes.extend(node.named_children(&mut cursor));
        match node.kind() {
            "using_directive" => {
                let directive = text(node, source).trim_end_matches(';');
                let name = directive
                    .trim_start_matches("global ")
                    .trim_start_matches("using")
                    .trim();
                if !name.starts_with("static ") {
                    imports.push((node.parent().unwrap_or(root), name));
                }
            }
            "invocation_expression" => invocations.push(node),
            _ => {
                if let Some(binding) = binding(node, source) {
                    bindings.entry(binding.name).or_default().push(binding);
                }
            }
        }
    }

    let mut results = Vec::new();
    for invocation in invocations {
        let Some(access) = invocation
            .child_by_field_name("function")
            .filter(|node| node.kind() == "member_access_expression")
        else {
            continue;
        };
        let (Some(receiver), Some(method)) = (
            access.child_by_field_name("expression"),
            access.child_by_field_name("name"),
        ) else {
            continue;
        };
        let receiver_text = text(receiver, source);
        let Some(ty) = (match receiver.kind() {
            "identifier" => receiver_type(receiver_text, false, invocation, &bindings),
            "member_access_expression" => receiver_text
                .strip_prefix("this.")
                .and_then(|name| receiver_type(name, true, invocation, &bindings)),
            "object_creation_expression" => receiver
                .child_by_field_name("type")
                .map(|ty| text(ty, source)),
            _ => None,
        }) else {
            continue;
        };
        let ty = ty.trim_end_matches('?').trim_start_matches("global::");
        let mut matches = BTreeSet::new();
        if let Some(known) = types.get(ty) {
            matches.insert(known);
        }
        for (scope, import) in &imports {
            if !contains(*scope, invocation) {
                continue;
            }
            let candidate = match import.split_once('=') {
                Some((alias, target)) if alias.trim() == ty => {
                    target.trim().trim_start_matches("global::").to_owned()
                }
                Some(_) => continue,
                None => format!("{}.{ty}", import.trim_start_matches("global::")),
            };
            matches.extend(types.get(&candidate));
        }
        if matches.len() == 1 {
            let pos = method.start_position();
            results.push((
                pos.row + 1,
                pos.column + 1,
                (*matches.first().unwrap()).clone(),
                text(method, source),
            ));
        }
    }
    results
}

fn contains(scope: Node<'_>, node: Node<'_>) -> bool {
    scope.start_byte() <= node.start_byte() && scope.end_byte() >= node.end_byte()
}

fn receiver_type<'a>(
    name: &str,
    member_only: bool,
    invocation: Node<'_>,
    bindings: &HashMap<&str, Vec<Binding<'a>>>,
) -> Option<&'a str> {
    let enclosing_type = ancestors(invocation).find(|node| type_scope(node.kind()));
    bindings
        .get(name)?
        .iter()
        .filter(|binding| {
            (!member_only || binding.member)
                && (!binding.member || Some(binding.scope) == enclosing_type)
                && binding.start <= invocation.start_byte()
                && contains(binding.scope, invocation)
        })
        .min_by_key(|binding| {
            (
                binding.scope.end_byte() - binding.scope.start_byte(),
                std::cmp::Reverse(binding.start),
            )
        })
        .map(|binding| binding.ty)
}
