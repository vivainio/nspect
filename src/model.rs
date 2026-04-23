use std::path::PathBuf;

use serde::Serialize;

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
