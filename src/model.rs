use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Serialize, Serializer};

/// Cheap structural metrics computed per type during the tree-sitter scan.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, bincode::Encode, bincode::Decode)]
pub struct TypeMetrics {
    /// Source lines spanned by the type declaration (inclusive). For partial
    /// types this is the sum across all partial fragments.
    pub loc: u32,
    /// Direct body members — methods, properties, fields, ctors, events, etc.
    /// Nested types are not counted as members.
    pub members: u32,
    /// McCabe-ish branch count inside the type's subtree: `if`, `while`,
    /// `for`, `foreach`, `do`, `case`, `catch`, ternary, and `when` clauses.
    /// Branches inside nested types count toward their enclosing type too.
    pub complexity: u32,
    /// Source spans where this type is declared. Length > 1 means a partial
    /// type split across files. The first entry is the "primary" span;
    /// methods default to it unless they carry their own `file_id`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spans: Vec<SourceSpan>,
    /// Per-method breakdown — only direct method/ctor/op members of this
    /// type's body, not members of nested types. Empty for enums/delegates
    /// and types with no method-shaped members.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<MethodMetric>,
    /// Simple names from the type's `base_list` — base class and implemented
    /// interfaces merged, in source order. Empty when the type has no base
    /// list. Names are not disambiguated against the cross-project catalog.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bases: Vec<String>,
    /// Attributes applied directly to the type, in source order. Names have
    /// the trailing `Attribute` stripped (`ServiceContract`, not
    /// `ServiceContractAttribute`). When an attribute carries arguments they
    /// are rendered as `Name(args)` with C# `"` rewritten to `'` for YAML
    /// plain-scalar safety. Empty when the type has no attributes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attributes: Vec<String>,
    /// Simple type names referenced anywhere inside this type's body — base
    /// list, fields, properties, parameters, locals, casts, attributes,
    /// etc. Deduped, sorted, self-references filtered. This powers the
    /// cross-index in `endpoints.yaml`: any consumer of an endpoint type
    /// shows up here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub referenced_types: Vec<String>,
}

/// One declaration fragment of a type. `file_id` indexes the owning project's
/// `source_files` table. Lines are 1-based, inclusive.
///
/// Serialized as a compact single-line string — `"f{file_id}:{start}-{end}"`
/// — to keep metrics.yaml dense. `lookup` parses it back.
#[derive(Debug, Clone, Default, PartialEq, Eq, bincode::Encode, bincode::Decode)]
pub struct SourceSpan {
    pub file_id: u32,
    pub line_start: u32,
    pub line_end: u32,
}

impl Serialize for SourceSpan {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&format_args!(
            "f{}:{}-{}",
            self.file_id, self.line_start, self.line_end
        ))
    }
}

/// Serialized as `"<name> L{start}-{end} loc={loc} cx={cx}"`, with an extra
/// ` f={file_id}` suffix only for partial-class methods whose file differs
/// from the type's primary span.
#[derive(Debug, Clone, Default, PartialEq, Eq, bincode::Encode, bincode::Decode)]
pub struct MethodMetric {
    /// Method, ctor, dtor, or operator name. Overloads share a name.
    pub name: String,
    pub line_start: u32,
    pub line_end: u32,
    pub loc: u32,
    pub complexity: u32,
    /// Only set on methods of partial types where the method lives in a file
    /// other than the type's primary span. Non-partial types leave this
    /// `None`; the method inherits the file from `spans[0]`.
    pub file_id: Option<u32>,
    /// Attributes applied to the method, in source order. Same rendering
    /// rules as `TypeMetrics::attributes`. Appended to the serialized
    /// one-liner as ` [A, B]` after the optional `f=<id>` slot.
    pub attributes: Vec<String>,
    /// Simple type names appearing in this method's signature — every
    /// parameter's declared type plus the return type. Deduped, sorted,
    /// predefined types (`int`, `string`, …) excluded. Drives the `dtos:`
    /// list in `endpoints.yaml`. Not rendered in the method one-liner; sits
    /// alongside it via a separate channel so the YAML stays terse.
    pub signature_types: Vec<String>,
}

impl Serialize for MethodMetric {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let attrs_suffix = if self.attributes.is_empty() {
            String::new()
        } else {
            format!(" [{}]", self.attributes.join(", "))
        };
        match self.file_id {
            Some(f) => s.collect_str(&format_args!(
                "{} L{}-{} loc={} cx={} f={}{}",
                self.name,
                self.line_start,
                self.line_end,
                self.loc,
                self.complexity,
                f,
                attrs_suffix
            )),
            None => s.collect_str(&format_args!(
                "{} L{}-{} loc={} cx={}{}",
                self.name,
                self.line_start,
                self.line_end,
                self.loc,
                self.complexity,
                attrs_suffix
            )),
        }
    }
}

/// Kinds of type declarations tracked by the tree-sitter source scan.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    bincode::Encode,
    bincode::Decode,
)]
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
    /// Simple type names referenced in type-position syntax across this
    /// project's `.cs` sources (base lists, fields, parameters, object
    /// creations, casts, attributes). Deduped, sorted. Predefined types
    /// (`int`, `string`, …) are excluded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub referenced_types: Vec<String>,
    /// `.cs` source files scanned for this project, in index order. Spans in
    /// `type_metrics` reference these by position.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_files: Vec<PathBuf>,
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
        let canonical = path.to_string_lossy().to_lowercase().replace('\\', "/");
        let mut h = DefaultHasher::new();
        canonical.hash(&mut h);
        ProjectId(h.finish())
    }
}
