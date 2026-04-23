# C# Project Analyzer — Plan

A Rust CLI that analyzes the **structure** of C# projects and solutions: dependency graphs, package references, project references, and (optionally) cross-checks between declared dependencies and what the source actually uses.

This is a design doc for handing off to Claude Code. Read it, then ask before making architectural changes.

---

## Goals

- Parse `.sln` and `.csproj` files across a repo.
- Build a project dependency graph (project-to-project and project-to-package).
- Detect: cycles, orphan projects, inconsistent package versions across projects, unused `PackageReference`s, undeclared dependencies referenced in source.
- Output: pretty text report, JSON, and a DOT/Mermaid graph.
- Ship as a single static-ish binary via GitHub Actions for Linux (gnu + musl), macOS (Intel + ARM), and Windows (MSVC).

## Non-goals

- Full C# semantic analysis (no type resolution, no symbol binding).
- MSBuild property expansion fidelity. Flag `$(Foo)` references; don't try to evaluate them.
- Editor integration / LSP. CLI only.
- Replacing Roslyn. If something truly needs Roslyn, that's out of scope.

---

## Architecture

```
  repo root
      │
      ▼
 ┌───────────────┐     ┌──────────────────┐
 │  discovery    │────▶│   csproj parser  │
 │ (walkdir +    │     │  (quick-xml)     │
 │  ignore)      │     └────────┬─────────┘
 └───────────────┘              │
                                ▼
                        ┌──────────────────┐
                        │  project model   │
                        │  (structs)       │
                        └────────┬─────────┘
                                 │
                 ┌───────────────┴───────────────┐
                 ▼                               ▼
        ┌──────────────────┐            ┌──────────────────┐
        │  dep graph       │            │ source scanner   │
        │  (petgraph)      │            │ (tree-sitter)    │
        └────────┬─────────┘            └────────┬─────────┘
                 │                               │
                 └───────────────┬───────────────┘
                                 ▼
                         ┌──────────────────┐
                         │   reporters      │
                         │ text/json/dot    │
                         └──────────────────┘
```

### Modules

- `discovery` — find `.sln` files, fall back to walking for `*.csproj`. Respect `.gitignore` via the `ignore` crate.
- `sln` — parse solution files. These aren't XML; they use a `Project("{guid}") = "name", "relative\path.csproj", "{guid}"` format. Small hand parser or regex is fine.
- `csproj` — parse SDK-style and legacy csproj with `quick-xml`. Extract:
  - `TargetFramework` / `TargetFrameworks`
  - `PackageReference` (name, version, `PrivateAssets`, `IncludeAssets`)
  - `ProjectReference` (resolved absolute path)
  - `<Reference>` assembly refs (legacy)
  - SDK attribute (SDK-style vs legacy detection)
- `msbuild_context` — walk up from each csproj collecting `Directory.Build.props`, `Directory.Build.targets`, `Directory.Packages.props`. Merge for Central Package Management.
- `graph` — build `petgraph::DiGraph<ProjectId, EdgeKind>` where `EdgeKind` ∈ `{ProjectRef, PackageRef}`. Run cycle detection, topo sort, reachability.
- `source_scan` — tree-sitter pass over `.cs` files. Query for `using_directive` and top-level type/namespace references. Purely textual.
- `analysis` — cross-check source names against declared deps. Produce findings.
- `report` — text (via `comfy-table` + `owo-colors`), JSON (serde), DOT, Mermaid.
- `cli` — `clap` derive API.

---

## Crate choices

| Purpose | Crate |
|---|---|
| CLI args | `clap` (derive) |
| XML | `quick-xml` with serde feature |
| FS walking | `walkdir` + `ignore` |
| Graph | `petgraph` |
| Parser | `tree-sitter` + `tree-sitter-c-sharp` |
| Tables | `comfy-table` |
| Color | `owo-colors` |
| JSON | `serde` + `serde_json` |
| Errors | `anyhow` (binary) + `thiserror` (lib modules) |
| Logging | `tracing` + `tracing-subscriber` |
| Testing | `insta` for snapshots on report output |

Avoid adding heavy deps without a reason. Everything above earns its keep.

---

## Data model (sketch)

```rust
pub struct Project {
    pub id: ProjectId,           // stable hash of canonical path
    pub path: PathBuf,           // absolute
    pub name: String,            // from csproj AssemblyName or filename
    pub sdk_style: bool,
    pub target_frameworks: Vec<String>,
    pub package_refs: Vec<PackageRef>,
    pub project_refs: Vec<PathBuf>,   // resolved absolute
    pub assembly_refs: Vec<String>,   // legacy <Reference>
    pub source_files: Vec<PathBuf>,
}

pub struct PackageRef {
    pub name: String,
    pub version: Option<String>,   // None when using CPM
    pub private_assets: Option<String>,
}

pub enum Finding {
    Cycle(Vec<ProjectId>),
    OrphanProject(ProjectId),
    VersionConflict { package: String, versions: Vec<(ProjectId, String)> },
    UnusedPackageRef { project: ProjectId, package: String },
    UndeclaredUsage { project: ProjectId, namespace: String },
    UnresolvedProjectRef { project: ProjectId, target: PathBuf },
}
```

---

## Gotchas (read before coding)

- **Central Package Management.** `Directory.Packages.props` declares versions centrally; `PackageReference` entries in csproj omit `Version`. Merge these before reporting versions.
- **Directory.Build.props/targets.** Walk up the directory tree to the repo root collecting these; they inject properties into every project below. Full fidelity is expensive — for v1, record their presence and flag projects affected by them, don't fully merge.
- **MSBuild property expansion.** `$(Version)`, `$(Configuration)` and friends. Don't evaluate. Record the raw string and move on.
- **Multi-targeting.** `TargetFrameworks` is semicolon-separated. Projects can have framework-conditional references (`Condition="'$(TargetFramework)'=='net8.0'"`). Capture conditions as opaque strings.
- **Legacy csproj.** Pre-SDK csproj is verbose, uses `<ItemGroup>` for source files explicitly, and has no `Sdk=""` attribute. Detect and handle, but prioritize SDK-style.
- **Path case sensitivity.** Windows solutions often have wrong casing. Canonicalize before hashing into `ProjectId`.
- **Raw/verbatim strings and preprocessor directives in source.** Tree-sitter handles these; don't regex C# source.
- **Tree-sitter gives CST, not semantics.** `Foo.Bar.Baz` is nested identifiers. Don't try to resolve; report textual references and let the cross-check be best-effort.
- **Partial classes.** A type can be split across files. For structure analysis we mostly don't care, but don't double-count.

---

## Milestones

1. **M1 — csproj + sln parsing.** `analyzer scan <path>` prints a list of projects with their target frameworks and package refs. No graph yet. Snapshot tests with a fixture repo.
2. **M2 — dependency graph.** Build project-to-project graph with petgraph. Emit DOT. Detect cycles and orphans. `analyzer graph <path> --format dot`.
3. **M3 — package analysis.** Add package nodes. Version conflict detection across projects. CPM support.
4. **M4 — source scan.** Tree-sitter pass. Extract `using` directives per project. Cross-check against declared packages (heuristic: package name often matches root namespace; maintain a small known-mapping table for common mismatches like `Newtonsoft.Json` → `Newtonsoft.Json`).
5. **M5 — reports.** Pretty text report with color, JSON output, Mermaid graph output.
6. **M6 — CI release pipeline.** Matrix build + binary upload on tag.

Ship M1 end-to-end before starting M2. Each milestone gets integration tests against a fixture repo under `tests/fixtures/`.

---

## Testing strategy

- `tests/fixtures/` contains small but real-shaped C# repos: SDK-style solo project, multi-project solution, CPM repo, legacy csproj, one with a cycle, one with a version conflict.
- Unit tests per parser module.
- `insta` snapshots for report output.
- A golden-file test per fixture: run the CLI, compare JSON output to expected.

---

## CI / release

- GitHub Actions, matrix over `ubuntu-latest`, `macos-latest`, `macos-13` (Intel), `windows-latest`.
- `Swatinem/rust-cache@v2` for cargo + target caching.
- Musl build via `x86_64-unknown-linux-musl` with `musl-tools` installed.
- Release automation via `taiki-e/create-gh-release-action` + `taiki-e/upload-rust-binary-action` on tag push.
- Tree-sitter's C build works out of the box on all three runners. No extra setup needed for native builds.
- For ARM Linux cross-compile, use `houseabsolute/actions-rust-cross` so the cross C toolchain is handled.

---

## CLI shape (proposed)

```
analyzer scan <path>                    # list projects, summary
analyzer graph <path> [--format dot|mermaid|json]
analyzer check <path> [--json]          # run all findings, exit non-zero if any
analyzer deps <path> --project <name>   # show one project's transitive deps
```

Global flags: `--no-source-scan`, `--include-legacy`, `-v/--verbose`.

---

## What not to do

- Don't evaluate MSBuild. You will lose.
- Don't hand-roll C# parsing. Use tree-sitter. If tree-sitter becomes a measured bottleneck (it won't), revisit.
- Don't try to resolve types. Textual cross-check only.
- Don't add an LSP mode "while we're at it." Scope creep kills this kind of tool.
- Don't support NuGet restore / package resolution. You're analyzing what's declared, not what resolves.

---

## Open questions for the next session

- Should `analyzer check` respect a config file (`.analyzer.toml`) for suppressing findings? Probably yes in M5.
- Do we want SARIF output for CI integration? Nice-to-have, not M1–M5.
- How do we handle `.slnx` (new XML solution format)? Probably add alongside `.sln` in M2.
