use std::path::{Path, PathBuf};

use anyhow::Result;
use ignore::WalkBuilder;

#[derive(Debug, Default)]
pub struct Discovered {
    pub solutions: Vec<PathBuf>,
    pub projects: Vec<PathBuf>,
}

pub fn discover(root: &Path) -> Result<Discovered> {
    let mut found = Discovered::default();

    // A file path was passed directly.
    if root.is_file() {
        classify(root, &mut found);
        return Ok(found);
    }

    let walker = WalkBuilder::new(root)
        .follow_links(false)
        .git_ignore(true)
        .git_exclude(true)
        .hidden(false)
        .build();
    for entry in walker.flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        classify(entry.path(), &mut found);
    }
    found.solutions.sort();
    found.projects.sort();
    Ok(found)
}

fn classify(path: &Path, out: &mut Discovered) {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return;
    };
    if ext.eq_ignore_ascii_case("sln") {
        out.solutions.push(path.to_path_buf());
    } else if ext.eq_ignore_ascii_case("csproj") {
        out.projects.push(path.to_path_buf());
    }
}
