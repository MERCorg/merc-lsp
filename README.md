# Overview

`merc-lsp` is a Language Server (built on `async-lsp`) for mCRL2 specifications, plus a thin
VS Code client that spawns it.

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
