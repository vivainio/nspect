---
name: nspect
description: Query the structure of a C# codebase (types, methods, references, dependencies) without reading source files. Use when answering "where is X declared?", "what calls Y?", "what's in this file?", "what are the subclasses of Z?", "how complex is this class?", or any question about a large .NET solution where reading individual .cs files would be expensive. Requires `nspect init` to have been run at the repo root.
---

# nspect

`nspect` is a CLI that pre-indexes a C# solution into YAML/JSON artifacts under `.nspect/gen/`. Once indexed, you can answer structural questions in milliseconds without grepping or reading `.cs` files.

## When to reach for `nspect lookup`

Prefer `nspect lookup` over `Read`/`Grep` whenever the question is structural:

- **"Where is `Foo` declared?"** — `nspect lookup Foo` returns the project, namespace, file, and line range. Handles ambiguity if the simple name exists in multiple projects.
- **"What's in `Foo`?"** — the same call lists members, method signatures (with line ranges), base types, and complexity.
- **"Who calls / uses `Foo`?"** — `referenced_by` lists the projects that reference this type's declaring project for it. (Project-level granularity, not call-site.)
- **"What subclasses `Foo`?"** — `subclasses` lists FQNs whose `bases` include `Foo`.
- **"What's in `Customer.cs`?"** — `nspect lookup --file Customer.cs` resolves by file path (suffix match works).
- **"Is this an HTTP endpoint?"** — controller/Minimal-API endpoints surface under `endpoint`.

Skip `nspect lookup` only when you genuinely need the source body (e.g. to edit it). Even then, lookup gives you the precise file:line to jump to, so use it first to avoid grepping.

## Common commands

```bash
# One-time per repo. Walks .sln/.csproj, parses *.cs via tree-sitter, writes
# .nspect/gen/{atlas,classes,metrics,references,endpoints,checks,tips}.yaml
# plus build-deps.json and build-plan.json. Re-run after large changes.
nspect init

# Single type (simple name or FQN); auto-discovers .nspect/gen by walking up.
nspect lookup Customer
nspect lookup Acme.Domain.Customer

# Multiple names + files in one batch (one YAML doc out).
nspect lookup OrderService InvoiceService --file Customer.cs

# Skip method-signature re-parse when the source tree has drifted.
nspect lookup Foo --no-sig

# Minimal output: bare method names, line ranges only (no signatures, no
# loc=/cx= per method, no endpoint `users:`). Type-level metrics still ship.
nspect lookup Foo --min

# Project dependency neighborhood (text/dot/mermaid/json).
nspect focus . MyProject --up 2 --down 1

# Whole-repo metrics summary.
nspect metrics .

# Project-to-project graph for the whole solution.
nspect graph .

# Standalone check for app.config/web.config <bindingRedirect> entries
# (inverted / inconsistent / duplicate). No init needed, skips the
# source scan — fast even on large repos.
nspect check-bindings .
```

## How the artifacts fit together

- `atlas.yaml` — projects, areas, edges, layers.
- `classes.yaml` — declared types per project.
- `metrics.yaml` — bodies for each type: members, methods (name + line range), bases, complexity, endpoint metadata.
- `references.yaml` — cross-project references: which projects reference which, plus ambiguity buckets.
- `endpoints.yaml` — HTTP endpoints surfaced from controllers / Minimal API.
- `build-deps.json`, `build-plan.json` — reverse transitive rebuild lists and parallel build waves.

`nspect lookup` joins across these so you almost never have to read them directly. If you do need raw data, it's plain YAML/JSON — open the file.

## Lookup output shape

Each match's `methods:` is a list of single-line strings. To keep output tight, the file path is hoisted out when every method shares one file (the common case for interfaces and non-partial classes):

- **Shared file**: a sibling key `methods_file: <path>` is set, and each method line is `Name  L<start>-<end>  loc=N  cx=N`.
- **Mixed files** (partial classes whose methods cross files): no `methods_file`, each line carries its own `path:start-end`.

With `--min`: signatures are skipped, the trailing `  loc=N  cx=N` is dropped, and any attached `endpoint.users:` map is stripped. Lines become `Name  L<start>-<end>`. Use it for cheap directory/overview queries; drop the flag when you need full signatures or call-site users.

## Troubleshooting

- **"no `.nspect/gen` found"** — run `nspect init` at the repo root, or pass `--atlas-dir <path>`.
- **Stale results** — re-run `nspect init`. The source-scan cache is incremental, so it's usually fast.
- **Method signatures look wrong** — the source may have changed since `init`. Re-run init, or use `--no-sig` to fall back to line ranges only.
- **Type not found** — try the FQN. If still missing, the declaring project may have failed to parse (check stderr from the most recent `nspect init`).
