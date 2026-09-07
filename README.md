# Overview

`merc-lsp` is a Language Server (built on `async-lsp`) for mCRL2 specifications, plus a thin
VS Code client that spawns it. It understands four document kinds, picked by file extension: plain
process specifications (`.mcrl2`), PBES (`.pbes`), PRES (`.pres`), and modal (mu-calculus) state
formulas (`.mcf`).

## Current status

- **Diagnostics** — syntax errors for all four document kinds, plus whole-specification type
  errors for each: data, actions, processes, and `init` for `.mcrl2`; `glob`, propositional-variable
  equations, and `init` for `.pbes`/`.pres`; and `act` declarations plus the formula itself
  (actions, fixpoint variables, and every `forall`/`exists`/`inf`/`sup`/`sum` binder) for `.mcf`.
  Communication sort-compatibility isn't checked yet, so "no diagnostics" isn't a full
  well-typedness guarantee.
- **Document outline** (`textDocument/documentSymbol`) — for all four document kinds; a `.mcf`
  document's outline nests every `mu`/`nu` fixpoint variable under whichever one encloses it, the
  same structure the formula itself has.
- **Semantic tokens** — AST-driven highlighting for all four document kinds that resolves mCRL2's
  structural ambiguities (e.g. `a(f)` as function application vs. action vs. process
  instantiation); not yet scope-aware (a bound variable reads the same as a free one).
- **Hover** and **go-to-definition** — for all four document kinds, driven by the checked
  specification's typing info: mapping/constructor/action/process/propositional-variable/state-variable
  uses, and bound/global variables, each resolving to their declaration site. Requires the whole
  document to currently type check.
- **Inlay hints** (`textDocument/inlayHint`) — for all four document kinds: a `name:` prefix on a
  call argument when the callee (a process, a struct constructor, or a fixpoint variable) names
  that position, a `: Sort` suffix otherwise (an action argument, a mapping argument, an equation's
  `eqn` LHS pattern variable), covering process instantiations/action instances,
  PBES/PRES/modal-formula propositional- and state-variable instantiations, and `val(...)`
  expressions alike.
- **Completion** (`textDocument/completion`) — for all four document kinds: every
  sort/constructor/mapping/action/process (or propositional-/state-variable)/variable the document
  declares, plus mCRL2's reserved keywords and built-in sort names. Works off the raw parse, not a
  checked specification, so it keeps working while a document is transiently ill-typed or mid-edit.
  Deliberately unscoped — a flat list, not real lexical scoping; an editor's own
  fuzzy-match/prefix filtering narrows it down.

## Building the server

```sh
cargo build --release
# binary at target/release/merc-lsp (merc-lsp.exe on Windows)
```

## VS Code extension

The client (`vscode-client/`) is a standard `vscode-languageclient` wrapper: it resolves a path
to the `merc-lsp` executable and speaks LSP to it over stdio. Resolution order
(`vscode-client/src/extension.ts`):

1. The `merc-lsp.serverPath` setting, if the user set one.
2. A binary bundled with the extension at `server/<platform>-<arch>/merc-lsp[.exe]` (see
   packaging below) — `process.platform`/`process.arch`, e.g. `server/linux-x64/merc-lsp`.
3. Plain `merc-lsp` resolved from `PATH` (e.g. after `cargo install --path .`).

### Local development

```sh
cd vscode-client
npm install
npm run compile        # or: npm run watch
```

Then open `vscode-client/` in VS Code and press F5 (Run Extension) to launch an Extension
Development Host. Make sure a `merc-lsp` binary is reachable via one of the three routes above —
easiest during development is `cargo install --path ..` so it's on `PATH`, or set
`merc-lsp.serverPath` in the dev host's settings to `target/debug/merc-lsp`.

### Packaging (VSIX)

Packaging is via [`@vscode/vsce`](https://github.com/microsoft/vscode-vsce), already a
`devDependency`:

```sh
cd vscode-client
npm install
npm run package     # runs `vsce package`, produces a .vsix
```

This produces a VSIX that does **not** bundle a server binary — it relies on routes 1/3 above
(a configured path, or `merc-lsp` on `PATH`). That's the simplest option and is fine for anyone
who builds/installs the Rust binary themselves.

To ship a **self-contained** extension that works out of the box, bundle prebuilt server binaries
per target platform, using VS Code's [target-specific VSIX](https://code.visualstudio.com/api/working-with-extensions/publishing-extension#platformspecific-extensions)
mechanism:

1. Cross-compile `merc-lsp` for each platform you want to support, e.g. via a CI matrix or
   [`cross`](https://github.com/cross-rs/cross): `x86_64-unknown-linux-gnu`,
   `aarch64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`, `x86_64-apple-darwin`,
   `aarch64-apple-darwin`.
2. For each target, copy the resulting binary into
   `vscode-client/server/<node-platform>-<node-arch>/merc-lsp[.exe]` (the `platformDir()` naming
   `extension.ts` looks for — e.g. `linux-x64`, `win32-x64`, `darwin-arm64`).
3. Run `vsce package --target <vscode-target>` once per platform (e.g. `linux-x64`, `win32-x64`,
   `darwin-arm64`, `linux-arm64`) with only that platform's binary present under `server/`, so
   each VSIX embeds just its own binary and stays small.
4. Publish all the resulting VSIXs — the Marketplace serves the right one per user automatically
   (`vsce publish --target ...` per platform, or upload each VSIX manually).

A GitHub Actions matrix job (one runner per OS/arch, `cargo build --release`, then invoke the
steps above) is the natural way to automate this; not set up yet in this repo.

### Settings

- `merc-lsp.serverPath` — explicit path to the `merc-lsp` executable, overriding auto-detection.
- `merc-lsp.trace.server` — `off` / `messages` / `verbose`, LSP wire tracing for debugging.

### Commands

- **mCRL2: Restart Language Server** (`merc-lsp.restartServer`) — stops and relaunches the server
  process without reloading the whole VS Code window. Useful after swapping in a rebuilt server
  binary, or as manual recovery if the server ever stops responding (a genuine, reproducible bug
  at that point, not a transient fault it retries around — see `PLAN.md`).
