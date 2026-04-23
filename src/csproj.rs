use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::model::{PackageRef, Project, ProjectId};

pub fn parse(path: &Path) -> Result<Project> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let raw = parse_str(&text)?;
    let canonical = canonicalize(path);
    let id = ProjectId::from_path(&canonical);
    let name = raw
        .assembly_name
        .unwrap_or_else(|| stem_of(path).unwrap_or_else(|| "unknown".to_string()));

    let project_refs = raw
        .project_refs
        .into_iter()
        .map(|rel| {
            let rel = rel.replace('\\', std::path::MAIN_SEPARATOR_STR);
            let base = canonical.parent().unwrap_or_else(|| Path::new("."));
            normalize(&base.join(&rel))
        })
        .collect();

    Ok(Project {
        id,
        path: canonical,
        name,
        sdk_style: raw.sdk_style,
        target_frameworks: raw.target_frameworks,
        package_refs: raw.package_refs,
        project_refs,
        assembly_refs: raw.assembly_refs,
        usings: Vec::new(),
        declared_namespaces: Vec::new(),
        declared_types: std::collections::BTreeMap::new(),
    })
}

pub fn canonicalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| normalize(path))
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        use std::path::Component::*;
        match comp {
            ParentDir => {
                out.pop();
            }
            CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn stem_of(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
}

#[derive(Debug, Default)]
struct RawCsproj {
    sdk_style: bool,
    assembly_name: Option<String>,
    target_frameworks: Vec<String>,
    package_refs: Vec<PackageRef>,
    project_refs: Vec<String>,
    assembly_refs: Vec<String>,
}

fn parse_str(xml: &str) -> Result<RawCsproj> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut raw = RawCsproj::default();
    let mut buf = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut text_buf = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Err(e) => return Err(anyhow::anyhow!("xml error: {e}")),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                let name = std::str::from_utf8(e.name().as_ref())?.to_string();
                if path.is_empty() && name.eq_ignore_ascii_case("Project") {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"Sdk" {
                            raw.sdk_style = true;
                        }
                    }
                }
                // Also capture refs declared as non-self-closing elements:
                //   <Reference Include="Foo">...child nodes...</Reference>
                // Legacy .NET Framework csprojs emit these a lot.
                let parent = path.last().cloned().unwrap_or_default();
                let attrs: Vec<_> = e.attributes().flatten().collect();
                handle_item(&mut raw, &parent, &name, &attrs);
                path.push(name);
                text_buf.clear();
            }
            Ok(Event::Empty(e)) => {
                let name = std::str::from_utf8(e.name().as_ref())?.to_string();
                // Self-closing Project (rare) — still capture SDK attr.
                if path.is_empty() && name.eq_ignore_ascii_case("Project") {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"Sdk" {
                            raw.sdk_style = true;
                        }
                    }
                }
                let parent = path.last().cloned().unwrap_or_default();
                let attrs: Vec<_> = e.attributes().flatten().collect();
                handle_item(&mut raw, &parent, &name, &attrs);
            }
            Ok(Event::Text(t)) => {
                if let Ok(s) = t.unescape() {
                    text_buf.push_str(&s);
                }
            }
            Ok(Event::End(e)) => {
                let name = std::str::from_utf8(e.name().as_ref())?.to_string();
                // Close tag: record any accumulated text into fields.
                if let Some(parent) = path.iter().rev().nth(1).cloned() {
                    handle_text(&mut raw, &parent, &name, &text_buf);
                }
                path.pop();
                text_buf.clear();
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(raw)
}

fn handle_item(
    raw: &mut RawCsproj,
    parent: &str,
    name: &str,
    attrs: &[quick_xml::events::attributes::Attribute<'_>],
) {
    if !parent.eq_ignore_ascii_case("ItemGroup") {
        return;
    }
    let find = |key: &[u8]| -> Option<String> {
        attrs
            .iter()
            .find(|a| a.key.as_ref().eq_ignore_ascii_case(key))
            .and_then(|a| String::from_utf8(a.value.to_vec()).ok())
    };
    match name {
        n if n.eq_ignore_ascii_case("PackageReference") => {
            if let Some(pkg) = find(b"Include") {
                raw.package_refs.push(PackageRef {
                    name: pkg,
                    version: find(b"Version"),
                    private_assets: find(b"PrivateAssets"),
                });
            }
        }
        n if n.eq_ignore_ascii_case("ProjectReference") => {
            if let Some(inc) = find(b"Include") {
                raw.project_refs.push(inc);
            }
        }
        n if n.eq_ignore_ascii_case("Reference") => {
            if let Some(inc) = find(b"Include") {
                raw.assembly_refs.push(inc);
            }
        }
        _ => {}
    }
}

fn handle_text(raw: &mut RawCsproj, parent: &str, name: &str, text: &str) {
    if !parent.eq_ignore_ascii_case("PropertyGroup") {
        return;
    }
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    match name {
        n if n.eq_ignore_ascii_case("TargetFramework") => {
            raw.target_frameworks.push(text.to_string());
        }
        n if n.eq_ignore_ascii_case("TargetFrameworks") => {
            for tf in text.split(';') {
                let tf = tf.trim();
                if !tf.is_empty() {
                    raw.target_frameworks.push(tf.to_string());
                }
            }
        }
        n if n.eq_ignore_ascii_case("AssemblyName") => {
            raw.assembly_name = Some(text.to_string());
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sdk_style() {
        let xml = r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <TargetFramework>net8.0</TargetFramework>
    <AssemblyName>MyApp</AssemblyName>
  </PropertyGroup>
  <ItemGroup>
    <PackageReference Include="Serilog" Version="3.1.1" />
    <PackageReference Include="Newtonsoft.Json" />
    <ProjectReference Include="..\Lib\Lib.csproj" />
  </ItemGroup>
</Project>"#;
        let raw = parse_str(xml).unwrap();
        assert!(raw.sdk_style);
        assert_eq!(raw.assembly_name.as_deref(), Some("MyApp"));
        assert_eq!(raw.target_frameworks, vec!["net8.0"]);
        assert_eq!(raw.package_refs.len(), 2);
        assert_eq!(raw.package_refs[0].name, "Serilog");
        assert_eq!(raw.package_refs[0].version.as_deref(), Some("3.1.1"));
        assert_eq!(raw.package_refs[1].version, None);
        assert_eq!(raw.project_refs.len(), 1);
    }

    #[test]
    fn parses_multi_targeting() {
        let xml = r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <TargetFrameworks>net8.0;netstandard2.0</TargetFrameworks>
  </PropertyGroup>
</Project>"#;
        let raw = parse_str(xml).unwrap();
        assert_eq!(raw.target_frameworks, vec!["net8.0", "netstandard2.0"]);
    }

    #[test]
    fn parses_legacy_csproj() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<Project ToolsVersion="15.0" xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
  <PropertyGroup>
    <TargetFrameworkVersion>v4.8</TargetFrameworkVersion>
    <AssemblyName>LegacyLib</AssemblyName>
  </PropertyGroup>
  <ItemGroup>
    <Reference Include="System.Xml" />
  </ItemGroup>
</Project>"#;
        let raw = parse_str(xml).unwrap();
        assert!(!raw.sdk_style);
        assert_eq!(raw.assembly_refs, vec!["System.Xml"]);
    }
}
