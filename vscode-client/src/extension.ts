/* --------------------------------------------------------------------------------------------
 * Copyright (c) Microsoft Corporation. All rights reserved.
 * Licensed under the MIT License. See License.txt in the project root for license information.
 * ------------------------------------------------------------------------------------------ */

import * as fs from 'fs';
import * as path from 'path';
import { ExtensionContext, OutputChannel, window, workspace } from 'vscode';

import {
	LanguageClient,
	LanguageClientOptions,
	ServerOptions,
	TransportKind
} from 'vscode-languageclient/node';

let client: LanguageClient | undefined;
let output: OutputChannel | undefined;

/** Binary name of the language server, without a platform-specific extension. */
const SERVER_BIN_NAME = 'merc-lsp';

/**
 * `os.platform()`-`os.arch()` folder name used for a bundled binary, e.g. `linux-x64`,
 * `win32-x64`, `darwin-arm64`. Matches the layout produced by the packaging step in
 * `vscode-client/README.md` / CI: `server/<platform>-<arch>/merc-lsp[.exe]`.
 */
function platformDir(): string {
	return `${process.platform}-${process.arch}`;
}

function serverFileName(): string {
	return process.platform === 'win32' ? `${SERVER_BIN_NAME}.exe` : SERVER_BIN_NAME;
}

/**
 * Expands a leading `${workspaceFolder}` in a user-supplied setting value. VS Code only expands
 * this automatically in a handful of contribution points (`tasks.json`, `launch.json`); a plain
 * `workspace.getConfiguration().get()` read, like `serverPath`'s, gets the literal string back —
 * so we resolve it ourselves against the first workspace folder.
 */
function expandWorkspaceFolder(value: string): string {
	const [firstFolder] = workspace.workspaceFolders ?? [];
	if (!firstFolder) {
		return value;
	}
	return value.replace(/\$\{workspaceFolder\}/g, firstFolder.uri.fsPath);
}

/**
 * Resolves the path to the `merc-lsp` executable, in order of preference:
 *
 * 1. The `merc-lsp.serverPath` user/workspace setting, if set — always trusted as-is, even if
 *    the file doesn't (yet) exist, so users can point at a binary they're about to build.
 * 2. A binary bundled with the extension under `server/<platform>-<arch>/`, for the packaged
 *    (VSIX) distribution — see the "Packaging" section of `vscode-client/README.md`.
 * 3. Plain `merc-lsp`, resolved via the user's `PATH` (e.g. `cargo install --path .` during
 *    development) — handed to `child_process.spawn` unresolved and let the OS find it.
 */
function resolveServerCommand(context: ExtensionContext): string {
	const configured = workspace.getConfiguration('merc-lsp').get<string>('serverPath');
	if (configured && configured.trim().length > 0) {
		return expandWorkspaceFolder(configured);
	}

	const bundled = context.asAbsolutePath(
		path.join('server', platformDir(), serverFileName())
	);
	if (fs.existsSync(bundled)) {
		return bundled;
	}

	return SERVER_BIN_NAME;
}

export function activate(context: ExtensionContext) {
	output = window.createOutputChannel('mCRL2-lsp');
	context.subscriptions.push(output);

	output.appendLine('mCRL2 extension activated.');

	const command = resolveServerCommand(context);
	output.appendLine(`Using merc-lsp binary: ${command}`);
	output.show(true);

	// Both `run` and `debug` launch the same binary the same way: `merc-lsp` speaks LSP over
	// stdio unconditionally, there's no separate debug mode to select (unlike the Node.js
	// language-server template this extension started from).
	const run = { command, transport: TransportKind.stdio };
	const serverOptions: ServerOptions = { run, debug: run };

	const clientOptions: LanguageClientOptions = {
		documentSelector: [{ scheme: 'file', language: 'mcrl2' }],
		synchronize: {
			fileEvents: workspace.createFileSystemWatcher('**/*.mcrl2')
		}
	};

	client = new LanguageClient(
		'merc-lsp',
		'mCRL2 Language Server',
		serverOptions,
		clientOptions
	);

	client.start().then(
		() => output?.appendLine('merc-lsp language server started.'),
		(error: unknown) => {
			const message = `Failed to start merc-lsp (looked for "${command}"): ${error instanceof Error ? error.message : error
				}. Set "merc-lsp.serverPath" if it isn't on your PATH.`;
			output?.appendLine(message);
			window.showErrorMessage(message);
		}
	);
}

export function deactivate(): Thenable<void> | undefined {
	if (!client) {
		return undefined;
	}
	return client.stop();
}
