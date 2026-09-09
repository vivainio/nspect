//! .NET assembly (`.dll`/`.exe`) metadata parsing via ECMA-335 CLI
//! metadata, using the `dotnetdll` crate.
//!
//! This reads the `AssemblyRef` table straight out of a PE file's embedded
//! metadata — no CLR, no Mono, no `dotnet` on `PATH`. That table is what
//! actually determines which assembly identities get loaded at runtime,
//! as opposed to what a `.csproj`/`packages.config` merely *declares*.
//! Complements [`crate::binding_redirects`], which is purely a source-level
//! `.config` scan and (by design) never touches a compiled binary: given a
//! directory of build output, [`find_redirect_causes`] joins the two to
//! show which on-disk reference actually needed each `<bindingRedirect>`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use dotnetdll::prelude::{ReadOptions, Resolution};
use serde::Serialize;

use crate::binding_redirects::BindingRedirectEntry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AssemblyVersion {
    pub major: u16,
    pub minor: u16,
    pub build: u16,
    pub revision: u16,
}

impl std::fmt::Display for AssemblyVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}.{}.{}.{}",
            self.major, self.minor, self.build, self.revision
        )
    }
}

/// Serializes as the plain `"major.minor.build.revision"` string (matching
/// how [`crate::binding_redirects`] already represents versions) rather
/// than as a 4-field struct — much less noisy in YAML/JSON output.
impl Serialize for AssemblyVersion {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl From<dotnetdll::resolved::assembly::Version> for AssemblyVersion {
    fn from(v: dotnetdll::resolved::assembly::Version) -> Self {
        AssemblyVersion {
            major: v.major,
            minor: v.minor,
            build: v.build,
            revision: v.revision,
        }
    }
}

/// One entry from a DLL's `AssemblyRef` table — an assembly it was compiled
/// against.
#[derive(Debug, Clone)]
pub struct AssemblyReference {
    pub name: String,
    pub version: AssemblyVersion,
    /// Lowercase hex, same shape as `app.config`'s `publicKeyToken`
    /// attribute. `None` when the reference is unsigned, or in the rare
    /// case where the `AssemblyRef` row embeds a full public key rather
    /// than its 8-byte token (deriving one from the other needs a SHA-1
    /// we don't carry a dependency for; name/version matching still works).
    pub public_key_token: Option<String>,
}

impl std::fmt::Display for AssemblyReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.name, self.version)?;
        if let Some(tok) = &self.public_key_token {
            write!(f, " ({tok})")?;
        }
        Ok(())
    }
}

/// Serializes as a single `"Name Version (token)"` string (matching
/// `DllYaml`'s `- dep.Name dep.Version` convention) rather than a 3-field
/// object — a DLL can carry dozens of these, and one line per reference
/// reads far better than three.
impl Serialize for AssemblyReference {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Parsed identity + dependencies of one `.dll`/`.exe`.
#[derive(Debug, Clone, Serialize)]
pub struct DllInfo {
    pub path: PathBuf,
    /// `None` for a module without an assembly manifest (a linked
    /// netmodule, not a normal build output).
    pub assembly_name: Option<String>,
    pub version: Option<AssemblyVersion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_key_token: Option<String>,
    pub references: Vec<AssemblyReference>,
}

fn token_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Clear every public key token for display purposes — the matching logic
/// in [`find_redirect_causes`] runs before this and is unaffected; this is
/// purely a presentation choice for [`crate::cli::run_dlls`]'s default
/// output, where the token is nearly always one of a handful of well-known
/// Microsoft constants and adds little.
pub fn strip_tokens(dlls: &mut [DllInfo]) {
    for d in dlls {
        d.public_key_token = None;
        for r in &mut d.references {
            r.public_key_token = None;
        }
    }
}

pub fn parse(path: &Path) -> Result<DllInfo> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    parse_bytes(&bytes, path)
}

fn parse_bytes(bytes: &[u8], path: &Path) -> Result<DllInfo> {
    let opts = ReadOptions {
        skip_method_bodies: true,
        ..ReadOptions::default()
    };
    let res = Resolution::parse(bytes, opts)
        .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;

    let (assembly_name, version, public_key_token) = match &res.assembly {
        Some(a) => (
            Some(a.name.to_string()),
            Some(a.version.into()),
            a.public_key
                .as_ref()
                .filter(|k| k.len() == 8)
                .map(|k| token_hex(k)),
        ),
        None => (None, None, None),
    };

    let references = res
        .assembly_references
        .iter()
        .map(|r| AssemblyReference {
            name: r.name.to_string(),
            version: r.version.into(),
            public_key_token: r
                .public_key_or_token
                .as_ref()
                .filter(|_| !r.has_full_public_key)
                .map(|k| token_hex(k)),
        })
        .collect();

    Ok(DllInfo {
        path: path.to_path_buf(),
        assembly_name,
        version,
        public_key_token,
        references,
    })
}

/// Find `.dll`/`.exe` files under `dir`, recursively. Deliberately doesn't
/// respect `.gitignore` (unlike [`crate::discovery::discover`]) — build
/// output directories like `bin/`/`obj/` are almost always gitignored, and
/// that's exactly where the binaries we want to inspect live.
pub fn discover(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        if dir.is_file() {
            return vec![dir.to_path_buf()];
        }
        return out;
    }
    for entry in walkdir::WalkDir::new(dir).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if ext.eq_ignore_ascii_case("dll") || ext.eq_ignore_ascii_case("exe") {
            out.push(path.to_path_buf());
        }
    }
    out.sort();
    out
}

/// Parse every `.dll`/`.exe` found under `dir`. Files that fail to parse
/// (native DLLs, corrupted output, etc.) are skipped with a warning rather
/// than failing the whole scan.
pub fn scan(dir: &Path) -> Vec<DllInfo> {
    let mut out = Vec::new();
    for path in discover(dir) {
        match parse(&path) {
            Ok(info) => out.push(info),
            Err(e) => tracing::warn!("skipping {}: {e:#}", path.display()),
        }
    }
    out
}

/// One real, on-disk `AssemblyRef` that falls inside a binding redirect's
/// `oldVersion` range — i.e. this is (part of) *why* the redirect exists.
#[derive(Debug, Clone, Serialize)]
pub struct RedirectCause {
    pub config_path: PathBuf,
    pub assembly_name: String,
    pub referenced_version: AssemblyVersion,
    pub referencing_dll: PathBuf,
}

/// A `<bindingRedirect>` with no matching on-disk reference found in any
/// scanned DLL — either the redirect is dead (nothing actually needs it),
/// or the referencing binary wasn't in the set that was scanned.
#[derive(Debug, Clone, Serialize)]
pub struct UnmatchedRedirect {
    pub config_path: PathBuf,
    pub assembly_name: String,
    pub old_version: String,
    pub new_version: String,
}

fn version_in_old_range(old_version: &str, v: AssemblyVersion) -> bool {
    let (lo, hi) = crate::binding_redirects::old_version_bounds(old_version);
    let (Some(lo), Some(hi)) = (
        crate::binding_redirects::parse_version(lo),
        crate::binding_redirects::parse_version(hi),
    ) else {
        return false;
    };
    let vt = (
        v.major as u32,
        v.minor as u32,
        v.build as u32,
        v.revision as u32,
    );
    vt >= lo && vt <= hi
}

/// Join `<bindingRedirect>` entries against a set of parsed DLLs: for each
/// redirect, find every on-disk `AssemblyRef` whose name (and, when both
/// sides carry one, public key token) match and whose version falls inside
/// `oldVersion`'s range. Redirects with no such match at all come back in
/// the second list.
pub fn find_redirect_causes(
    entries: &[BindingRedirectEntry],
    dlls: &[DllInfo],
) -> (Vec<RedirectCause>, Vec<UnmatchedRedirect>) {
    let mut causes = Vec::new();
    let mut matched: BTreeSet<usize> = BTreeSet::new();

    for dll in dlls {
        for r in &dll.references {
            for (i, e) in entries.iter().enumerate() {
                if !e.assembly_name.eq_ignore_ascii_case(&r.name) {
                    continue;
                }
                if let (Some(tok_e), Some(tok_r)) = (&e.public_key_token, &r.public_key_token) {
                    if !tok_e.eq_ignore_ascii_case(tok_r) {
                        continue;
                    }
                }
                if !version_in_old_range(&e.old_version, r.version) {
                    continue;
                }
                matched.insert(i);
                causes.push(RedirectCause {
                    config_path: e.config_path.clone(),
                    assembly_name: e.assembly_name.clone(),
                    referenced_version: r.version,
                    referencing_dll: dll.path.clone(),
                });
            }
        }
    }

    let unmatched = entries
        .iter()
        .enumerate()
        .filter(|(i, _)| !matched.contains(i))
        .map(|(_, e)| UnmatchedRedirect {
            config_path: e.config_path.clone(),
            assembly_name: e.assembly_name.clone(),
            old_version: e.old_version.clone(),
            new_version: e.new_version.clone(),
        })
        .collect();

    (causes, unmatched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dotnetdll::prelude::*;

    /// Round-trip: build a minimal assembly in memory with `dotnetdll`'s
    /// writer, referencing one external assembly, then parse the bytes
    /// back with our own `parse_bytes` and check the AssemblyRef came
    /// through. No fixture binary needed.
    fn build_test_dll() -> Vec<u8> {
        let mut res = Resolution::new(Module::new("Test.dll"));
        res.assembly = Some(Assembly::new("Test"));

        let mut ext = ExternalAssemblyReference::new("Newtonsoft.Json");
        ext.version = dotnetdll::resolved::assembly::Version {
            major: 13,
            minor: 0,
            build: 0,
            revision: 0,
        };
        ext.public_key_or_token = Some(vec![0x30, 0xad, 0x4f, 0xe6, 0xb2, 0xa6, 0xae, 0xed].into());
        res.assembly_references.push(ext);

        res.write(WriteOptions::default()).unwrap()
    }

    #[test]
    fn parses_own_identity_and_references() {
        let bytes = build_test_dll();
        let info = parse_bytes(&bytes, Path::new("Test.dll")).unwrap();

        assert_eq!(info.assembly_name.as_deref(), Some("Test"));
        assert_eq!(info.references.len(), 1);
        let r = &info.references[0];
        assert_eq!(r.name, "Newtonsoft.Json");
        assert_eq!(r.version.to_string(), "13.0.0.0");
        assert_eq!(r.public_key_token.as_deref(), Some("30ad4fe6b2a6aeed"));
    }

    fn entry(assembly_name: &str, old_version: &str, new_version: &str) -> BindingRedirectEntry {
        BindingRedirectEntry {
            config_path: PathBuf::from("app.config"),
            assembly_name: assembly_name.into(),
            public_key_token: Some("30ad4fe6b2a6aeed".into()),
            old_version: old_version.into(),
            new_version: new_version.into(),
        }
    }

    #[test]
    fn finds_the_dll_that_caused_a_redirect() {
        let bytes = build_test_dll();
        let dll = parse_bytes(&bytes, Path::new("Test.dll")).unwrap();
        let entries = vec![entry("Newtonsoft.Json", "0.0.0.0-13.0.0.0", "13.0.0.0")];

        let (causes, unmatched) = find_redirect_causes(&entries, &[dll]);
        assert_eq!(causes.len(), 1);
        assert_eq!(causes[0].referencing_dll, PathBuf::from("Test.dll"));
        assert_eq!(causes[0].referenced_version.to_string(), "13.0.0.0");
        assert!(unmatched.is_empty());
    }

    #[test]
    fn flags_a_redirect_with_no_matching_reference() {
        let entries = vec![entry("Newtonsoft.Json", "0.0.0.0-13.0.0.0", "13.0.0.0")];
        let (causes, unmatched) = find_redirect_causes(&entries, &[]);
        assert!(causes.is_empty());
        assert_eq!(unmatched.len(), 1);
        assert_eq!(unmatched[0].assembly_name, "Newtonsoft.Json");
    }

    #[test]
    fn version_outside_range_does_not_match() {
        let bytes = build_test_dll();
        let dll = parse_bytes(&bytes, Path::new("Test.dll")).unwrap();
        // Redirect only covers up to 12.x; the DLL references 13.0.0.0.
        let entries = vec![entry("Newtonsoft.Json", "0.0.0.0-12.0.0.0", "12.0.0.0")];

        let (causes, unmatched) = find_redirect_causes(&entries, &[dll]);
        assert!(causes.is_empty());
        assert_eq!(unmatched.len(), 1);
    }
}
