use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// A project entry extracted from a `.sln` file.
#[derive(Debug, Clone)]
pub struct SlnProject {
    pub name: String,
    pub path: PathBuf,
}

/// Parse a `.sln` file and return the csproj paths it references.
/// Solutions use lines like:
/// `Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "Name", "rel\path.csproj", "{GUID}"`
pub fn parse(sln_path: &Path) -> Result<Vec<SlnProject>> {
    let text = std::fs::read_to_string(sln_path)
        .with_context(|| format!("reading {}", sln_path.display()))?;
    Ok(parse_str(
        &text,
        sln_path.parent().unwrap_or_else(|| Path::new(".")),
    ))
}

pub fn parse_str(text: &str, base: &Path) -> Vec<SlnProject> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("Project(") {
            continue;
        }
        // Find the "=" and parse the three quoted values after it.
        let Some(eq) = line.find('=') else { continue };
        let rhs = &line[eq + 1..];
        let parts = quoted_fields(rhs);
        if parts.len() < 2 {
            continue;
        }
        let name = parts[0].clone();
        let rel = parts[1].replace('\\', std::path::MAIN_SEPARATOR_STR);
        let rel_path = Path::new(&rel);
        // Solutions list nested solution folders too — filter by extension.
        if rel_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("csproj"))
            .unwrap_or(false)
        {
            let abs = base.join(rel_path);
            out.push(SlnProject { name, path: abs });
        }
    }
    out
}

fn quoted_fields(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c == '"' {
            chars.next();
            let mut buf = String::new();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                buf.push(c);
            }
            out.push(buf);
        } else {
            chars.next();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sln_projects() {
        let sln = r#"
Microsoft Visual Studio Solution File, Format Version 12.00
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "Alpha", "src\Alpha\Alpha.csproj", "{11111111-1111-1111-1111-111111111111}"
EndProject
Project("{2150E333-8FDC-42A3-9474-1A3956D46DE8}") = "SolutionFolder", "SolutionFolder", "{22222222-2222-2222-2222-222222222222}"
EndProject
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "Beta", "src\Beta\Beta.csproj", "{33333333-3333-3333-3333-333333333333}"
EndProject
"#;
        let projects = parse_str(sln, Path::new("/root"));
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].name, "Alpha");
        assert_eq!(projects[1].name, "Beta");
    }
}
