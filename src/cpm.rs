//! Central Package Management (`Directory.Packages.props`) support.
//!
//! CPM declares `<PackageVersion Include="Foo" Version="1.2.3" />` entries in a
//! `Directory.Packages.props` file at (or above) each project. Projects then
//! list `<PackageReference Include="Foo" />` without a version.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use quick_xml::events::Event;
use quick_xml::Reader;

#[derive(Debug, Clone, Default)]
pub struct CpmFile {
    pub path: PathBuf,
    pub versions: HashMap<String, String>,
}

/// Walk up from `start` looking for `Directory.Packages.props`.
/// Returns the first one found (closest wins) and its parsed version map.
pub fn find_for(start: &Path) -> Result<Option<CpmFile>> {
    let mut cur = if start.is_file() {
        start.parent().map(Path::to_path_buf)
    } else {
        Some(start.to_path_buf())
    };
    while let Some(dir) = cur {
        let candidate = dir.join("Directory.Packages.props");
        if candidate.is_file() {
            let text = std::fs::read_to_string(&candidate)?;
            let versions = parse_str(&text)?;
            return Ok(Some(CpmFile {
                path: candidate,
                versions,
            }));
        }
        cur = dir.parent().map(Path::to_path_buf);
    }
    Ok(None)
}

pub fn parse_str(xml: &str) -> Result<HashMap<String, String>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out = HashMap::new();

    let mut collect = |attrs: quick_xml::events::attributes::Attributes| {
        let mut name: Option<String> = None;
        let mut version: Option<String> = None;
        for a in attrs.flatten() {
            let key = a.key.as_ref();
            let val = String::from_utf8_lossy(&a.value).into_owned();
            if key.eq_ignore_ascii_case(b"Include") {
                name = Some(val);
            } else if key.eq_ignore_ascii_case(b"Version") {
                version = Some(val);
            }
        }
        if let (Some(n), Some(v)) = (name, version) {
            out.insert(n, v);
        }
    };

    loop {
        match reader.read_event_into(&mut buf) {
            Err(e) => return Err(anyhow::anyhow!("xml error: {e}")),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e) | Event::Empty(e)) => {
                if e.name()
                    .as_ref()
                    .eq_ignore_ascii_case(b"PackageVersion")
                {
                    collect(e.attributes());
                }
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_package_versions() {
        let xml = r#"<Project>
  <PropertyGroup>
    <ManagePackageVersionsCentrally>true</ManagePackageVersionsCentrally>
  </PropertyGroup>
  <ItemGroup>
    <PackageVersion Include="Serilog" Version="3.1.1" />
    <PackageVersion Include="Newtonsoft.Json" Version="13.0.3" />
  </ItemGroup>
</Project>"#;
        let v = parse_str(xml).unwrap();
        assert_eq!(v.get("Serilog").map(String::as_str), Some("3.1.1"));
        assert_eq!(v.get("Newtonsoft.Json").map(String::as_str), Some("13.0.3"));
    }
}
