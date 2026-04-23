use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;

/// Cheap structural metrics computed per type during the tree-sitter scan.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TypeMetrics {
    /// Source lines spanned by the type declaration (inclusive).
    pub loc: u32,
    /// Direct body members — methods, properties, fields, ctors, events, etc.
    /// Nested types are not counted as members.
    pub members: u32,
    /// McCabe-ish branch count inside the type's subtree: `if`, `while`,
    /// `for`, `foreach`, `do`, `case`, `catch`, ternary, and `when` clauses.
    /// Branches inside nested types count toward their enclosing type too.
    pub complexity: u32,
    /// Per-method breakdown — only direct method/ctor/op members of this
    /// type's body, not members of nested types. Empty for enums/delegates
    /// and types with no method-shaped members.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<MethodMetric>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MethodMetric {
    /// Method, ctor, dtor, or operator name. Overloads share a name.
    pub name: String,
    pub loc: u32,
    pub complexity: u32,
}

/// Kinds of type declarations tracked by the tree-sitter source scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeKind {
    Class,
    Interface,
    Struct,
    Record,
    RecordStruct,
    Enum,
    Delegate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct ProjectId(pub u64);

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub id: ProjectId,
    pub path: PathBuf,
    pub name: String,
    pub sdk_style: bool,
    pub target_frameworks: Vec<String>,
    pub package_refs: Vec<PackageRef>,
    pub project_refs: Vec<PathBuf>,
    pub assembly_refs: Vec<String>,
    #[serde(default)]
    pub usings: Vec<String>,
    /// Namespaces declared in this project's `.cs` sources. Deduped, sorted.
    #[serde(default)]
    pub declared_namespaces: Vec<String>,
    /// Fully-qualified type names declared in this project's `.cs` sources,
    /// bucketed by kind. Per-bucket lists are deduped and sorted.
    #[serde(default)]
    pub declared_types: BTreeMap<TypeKind, Vec<String>>,
    /// Per-type metrics keyed by fully-qualified type name. Populated only
    /// when the source scan ran.
    #[serde(default)]
    pub type_metrics: BTreeMap<String, TypeMetrics>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackageRef {
    pub name: String,
    pub version: Option<String>,
    pub private_assets: Option<String>,
}

impl ProjectId {
    pub fn from_path(path: &std::path::Path) -> Self {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let canonical = path
            .to_string_lossy()
            .to_lowercase()
            .replace('\\', "/");
        let mut h = DefaultHasher::new();
        canonical.hash(&mut h);
        ProjectId(h.finish())
    }
}
