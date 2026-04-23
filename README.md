# nspect

A Rust CLI that analyzes the **structure** of C# projects and solutions: dependency graphs, package references, version conflicts, and a best-effort cross-check between declared packages and what the source actually imports.

Not a compiler, not Roslyn. Fast, read-only, works from the filesystem — no NuGet restore, no MSBuild evaluation.

## Install

```bash
git clone https://github.com/vivainio/nspect
cd nspect
cargo install --path .
```

## Commands

### `nspect scan <path>`

List every project reachable from `<path>` (a repo root, a `.sln`, or a `.csproj`) with its SDK style, target framework(s), and package/project refs.

```
$ nspect scan ./my-repo
Found 12 project(s)

┌──────────────┬─────┬────────────────────────┬──────┬──────────┐
│ Project      │ SDK │ TargetFramework(s)     │ Pkgs │ ProjRefs │
╞══════════════╪═════╪════════════════════════╪══════╪══════════╡
│ Web.Api      │ sdk │ net8.0                 │ 14   │ 3        │
│ Domain       │ sdk │ net8.0, netstandard2.0 │ 2    │ 0        │
...
```

Add `--format json` for machine output.

### `nspect graph <path>`

Emit a project-to-project dependency graph as DOT, Mermaid, JSON, or a text summary.

```bash
nspect graph ./my-repo --format dot      | dot -Tsvg > graph.svg
nspect graph ./my-repo --format mermaid  > graph.mmd
nspect graph ./my-repo --format text
```

Package nodes are **off by default** — on large monoliths they drown out the project structure. Add `--packages` to include them.

### `nspect check <path>`

Run every finding and exit non-zero if any error-level finding is produced. Integrates cleanly with CI.

| Finding | Severity | What it means |
|---|---|---|
| `cycle` | error | A project-to-project reference cycle. |
| `version_conflict` | error | Same package declared with different versions across projects. |
| `unresolved_project_ref` | warning | A `<ProjectReference>` that doesn't resolve on disk. |
| `unused_package_ref` | warning | A `<PackageReference>` whose namespaces never appear in any `using` of the project. Skips test runners, analyzers, and runtime shims by default. |
| `undeclared_usage` | warning | A `using X.Y.Z;` that doesn't match any declared package or project ref. Advisory only — noisy on legacy codebases that rely on transitive DLL discovery. |
| `orphan_project` | info | A project with no incoming or outgoing project refs. |

Flags:

- `--json` — structured output instead of a text report
- `--no-source-scan` — skip the tree-sitter pass (disables `unused_package_ref` + `undeclared_usage`, ~100× faster on big monorepos)

### `nspect ts-dump <file.cs>`

Debug aid. Shows the extracted `using`s, top-level named children of the parse tree with line ranges, and (with `--sexp`) the full tree-sitter S-expression annotated with leaf source text:

```
(class_declaration
  (modifier "public")
  name:
  (identifier "Greeter")
  body:
  (declaration_list
    (method_declaration ...)))
```

Useful for writing new heuristics against the CST.

## What it handles

- **SDK-style csproj** — `<PackageReference>`, `<ProjectReference>`, `TargetFramework(s)`, `AssemblyName`
- **Legacy csproj** — `<Reference>` assembly refs are counted as namespace providers
- **`.sln` files** — project list (the format is not XML; parsed directly)
- **Central Package Management** — walks up for `Directory.Packages.props` and resolves version-less `PackageReference` entries
- **Multi-targeting** — captures `TargetFrameworks="net8.0;netstandard2.0"` as a list
- **Malformed csprojs** — skipped with a warning instead of aborting the scan

## What it doesn't handle

- **MSBuild property evaluation.** `$(Foo)` references are recorded as-is; nothing is expanded. Attempting to evaluate MSBuild correctly is a rabbit hole.
- **`Directory.Build.props/targets`.** Presence is not currently merged into project metadata. Flagged for a future milestone.
- **Transitive DLL discovery via HintPath.** Legacy .NET Framework monoliths rely on `packages/*/lib/*.dll` being found through a chain of HintPaths. `undeclared_usage` does not trace these, which is why it's noisy on legacy codebases.
- **Type resolution.** The source scan is textual. `using Foo.Bar;` produces the string `"Foo.Bar"`; whether that's a namespace or a static type is not determined.
- **NuGet restore.** `nspect` analyzes what's *declared*, not what would *resolve*.

## Performance

On a ~790-csproj monolith:

| Command | Time |
|---|---|
| `nspect scan` (parse all csprojs + CPM) | ~0.3 s |
| `nspect graph` | ~0.3 s |
| `nspect check --no-source-scan` | ~0.3 s |
| `nspect check` (full, tree-sitter across ~all .cs files) | ~28 s |

## License

MIT
