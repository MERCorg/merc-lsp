# Overview

`merc-lsp` is a Language Server (built on `async-lsp`) for mCRL2 specifications,
plus a thin VS Code client that spawns it, using the `merc` toolset. Supports
plain process specifications (`.mcrl2`), PBES (`.pbes`), PRES (`.pres`), and
modal (mu-calculus) state formulas (`.mcf`).

## Current status

Supports syntax and type checking error diagnostics, document outline, semantic
tokens, hover and go-to-definition, inlay hints, and text completion.

## Building the server

```sh
cargo build --release
# binary at target/release/merc-lsp (merc-lsp.exe on Windows)
```

### The `lsp-extensions` Cargo feature

Several non-standard protocol extensions in [VS Code
extension](#whats-specific-to-the-vs-code-extension) live behind a Cargo
feature, `lsp-extensions`, enabled by **default**.

```sh
cargo build --release --no-default-features
```

## Keeping diagnostics up to date across files

Plain LSP only reanalyzes a document on `didOpen`/`didSave`. So if `a.mcrl2` `%import`s `b.mcrl2`,
and `b.mcrl2` changes on disk — edited in another tab, by an external tool, or by `git checkout` —
while `a.mcrl2` is open, nothing tells the server to recheck `a.mcrl2`: it never gets a `didSave`
of its own. This is what `workspace/didChangeWatchedFiles` (standard LSP) is for: the client
watches project files itself and forwards create/change/delete events, and `backend::router`'s
handler for it (`reanalyze_stale_documents`, via `Document::is_stale`/
`has_newly_available_import`) reanalyzes every open document that leaves stale. The VS Code client
registers one over `**/*.{mcrl2,pbes,pres,mcf}` (`vscode-client/src/extension.ts`'s
`synchronize.fileEvents`) — `vscode-languageclient` forwards its events as this notification
automatically. Relies on the client both supporting dynamic file-watcher registration and watching
broadly enough to cover every `%import` target; deliberately left to the client/IDE to provide
rather than duplicated with a server-side watcher of its own.

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

- **mCRL2: Restart Language Server** (`merc-lsp.restartServer`). Stops and relaunches the server
  process without reloading the whole VS Code window. Useful after swapping in a rebuilt server
  binary, or as manual recovery if the server ever stops responding.
- **mCRL2: Generate Full Specification** (`merc-lsp.generateFullSpec`). On the
  active `.mcrl2` editor, resolves every `%import` and writes the merged,
  fully-parenthesized result to a sibling `<name>.generated.mcrl2` file, then
  opens it. Meant for feeding to `mcrl22lps` and other spec tools, which don't
  understand `%import` themselves.

## VS Code LSP Extensions

The two server-side extensions below are gated behind the [`lsp-extensions`
Cargo feature](#the-lsp-extensions-cargo-feature), on by default.

- **Built-in declarations as virtual, read-only documents.** Hovering or jumping to the definition of
  a built-in name (`Bool`, `List`, …) resolves into system-defined content that has no real file on
  disk. The server exposes it over a custom `merc/virtualDocument` request (`src/virtual_document.rs`)
  keyed by `merc-builtin:`-scheme URIs (see `src/convert.rs`); the client answers by registering a
  `workspace.registerTextDocumentContentProvider` for that scheme
  (`registerVirtualDocumentProvider` in `vscode-client/src/extension.ts`), which VS Code then treats
  as an ordinary (read-only) open document. A generic client with no custom-scheme content-provider
  mechanism will fail to open the location at all — goto-definition into a built-in effectively
  becomes a dead end.
- **"Generate Full Specification" command.** The `merc-lsp.generateFullSpec` command above is pure
  client-side glue around a custom `merc/generateFullSpec` request (`src/generate.rs`): a plain LSP
  client gets no menu entry, no command, and no automatic way to invoke it — it would need to send
  the request itself and do something with the returned text.
