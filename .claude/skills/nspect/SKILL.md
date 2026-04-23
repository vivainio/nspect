---
name: nspect
description: Query the structure of a C# codebase (types, methods, references, dependencies) without reading source files. Use when answering "where is X declared?", "what calls Y?", "what's in this file?", "what are the subclasses of Z?", "how complex is this class?", or any question about a large .NET solution where reading individual .cs files would be expensive. Requires `nspect init` to have been run at the repo root.
---

# nspect

Structural index for C# codebases. Answers questions from a pre-built atlas instead of reading source files.

## One-time setup per repo

```
nspect init
```

- Creates `.nspect/atlas/` at the repo root.
- Adds `/.nspect/` to `.gitignore`.
- Takes ~35s on a 788-csproj monolith. Regenerate after significant source changes.
- Five artifacts are written: `atlas.yaml` (projects/edges/layers), `classes.yaml` (types per project), `metrics.yaml` (types + methods with **file paths and line ranges**), `checks.yaml` (cycles, orphans, unresolved refs, unused/undeclared packages, version conflicts), `references.yaml` (cross-project type usage, including `ambiguous` type names).

If you're asked a question that depends on atlas data and the dir doesn't exist, suggest `nspect init` — don't try to answer from raw source.

## Primary tool: `nspect lookup`

Auto-discovers `.nspect/atlas/` by walking up from the current directory. Output is always a wrapped YAML with `types:` and/or `files:` arrays — even for a single query — so one parser handles every call.

**Batch in one call** — always prefer this to running `lookup` multiple times. The YAML load and per-file tree-sitter re-parses are shared across queries, so batching is much cheaper than N serial invocations:
```
nspect lookup Customer Invoice Order --file Program.cs --file Startup.cs
```

**By type name** (simple or fully-qualified):
```
nspect lookup Customer
nspect lookup Acme.Domain.Customer
```
Returns every match with: declaring project, namespace, **`at:` file:line range(s)** (one per partial), per-type metrics (loc/members/complexity), base list, **per-method signatures with file:line**, and cross-project callers. Each method line looks like:
```
protected override void Dispose(bool disposing)  Src/.../Form1.Designer.cs:14-21  loc=8  cx=1
```
The signature is re-parsed from source via tree-sitter on demand — so it reflects current source, and it gracefully falls back to the bare method name if the file has drifted or failed to parse.

**By source file** (suffix match, repeatable):
```
nspect lookup --file Customer.cs
nspect lookup --file Src/Domain/Customer.cs
```
Returns every atlas-declared type whose span lives in that file, with line ranges. Use this to enumerate "what's in this file" without opening it.

**Flags:**
- `--no-sig` — skip the tree-sitter re-parse. Faster, produces name-only method lines. Use when source has drifted or you only need line ranges.
- `--atlas-dir <path>` — override auto-discovery. Rarely needed.

## Interpreting output

- **`at:`** is a list — length > 1 means a partial class split across files.
- **`methods:`** are single-line strings — greppable. Format: `<signature>  <path>:<start>-<end>  loc=N  cx=N`.
- **`referenced_by:`** lists caller projects (cross-project only; intra-project refs aren't tracked).
- **`subclasses:`** (bottom of output) — types whose base list names the query, grouped by declaring project.
- **`ambiguous_in:`** — projects where the query's simple name is ambiguous (declared in multiple referenced projects).

File paths are repo-root-relative; `path:line` is clickable in Claude Code.

## Other commands (less common, but present)

- `nspect atlas <path> --full --output-dir <dir>` — write all 5 artifacts. `init` wraps this for the common case.
- `nspect metrics <path>` — textual summary of LOC/complexity per project, plus top-N complex methods. Takes `--project <name>` to narrow.
- `nspect focus <path> <project> [--up N] [--down N]` — visualize the dependency neighborhood of one project. `--format mermaid|dot|text|json`.
- `nspect graph <path>` — full project dependency graph. `--format dot|mermaid|json|text`.
- `nspect scan <path>` — list discovered projects with target frameworks and package refs.
- `nspect ts-dump <file.cs>` — dump the tree-sitter parse of a single file (debugging helper).

## When NOT to use nspect

- Questions about line-level implementation details (what does this method *do*) — `lookup` gives you a signature + file:line, then read that slice with `Read`.
- Very fresh edits — regenerate `.nspect/atlas/` if source has moved significantly since init.
- Non-C# code — nspect only understands `.cs` sources and `.csproj`/`.sln` files.

## Raw YAML notes (for when lookup isn't enough)

In `metrics.yaml`, spans are serialized compactly:
- `source_files:` is a mapping of `parent_dir -> [basename, ...]`. Flat index (the `f<id>` number) is the position in the sorted-by-dir, then by-basename enumeration of that mapping.
- `spans: - f55:16-195` — file_id 55, lines 16-195.
- `methods: - Validate L22-40 loc=19 cx=5` — name, line range, metrics. A trailing `f=<id>` appears only for methods of partial classes whose file differs from the type's primary span.

Don't grep `metrics.yaml` directly for `file:line` — use `lookup`, which resolves file_ids to full paths.
