import * as fs from 'fs';
import * as path from 'path';
import { CancellationToken, commands, ExtensionContext, OutputChannel, Uri, window, workspace } from 'vscode';

import {
	LanguageClient,
	LanguageClientOptions,
	RequestType,
	ServerOptions,
	TransportKind
} from 'vscode-languageclient/node';

let client: LanguageClient | undefined;
let output: OutputChannel | undefined;

/**
 * The read-only scheme `merc-lsp` uses for a `Location` into content with no real file behind it.
 */
const VIRTUAL_DOCUMENT_SCHEME = 'merc-builtin';

/**
 * `VirtualDocument` request `{ uri: string }` in, the document's text (or `null` if the server has no entry for it) out.
 */
const virtualDocumentRequest = new RequestType<{ uri: string }, string | null, void>('merc/virtualDocument');

/**
 * `GenerateFullSpec` request `{ uri: string }` in, that process specification's `%import`s
 * resolved and merged, then rendered back to fully-parenthesized (so unambiguous, even against
 * the real mCRL2 parser) mCRL2 source text — or `null` if `uri` isn't an open, successfully
 * parsed process specification. Mirrors `../src/generate.rs`'s `GenerateFullSpec` request.
 */
const generateFullSpecRequest = new RequestType<{ uri: string }, string | null, void>('merc/generateFullSpec');

/**
 * `merc-lsp.generateFullSpec`: writes the active `.mcrl2` editor's merged specification to a
 * sibling `<name>.generated.mcrl2` file and opens it. That has to be a real file on disk, not a
 * `merc-builtin:` virtual document like {@link registerVirtualDocumentProvider}'s — the whole
 * point is for the real mCRL2 toolset (`mcrl22lps` and friends), an external process, to consume
 * it, and only the editor's virtual-document providers can read a virtual URI.
 */
async function generateFullSpec(): Promise<void> {
	const editor = window.activeTextEditor;
	if (!editor || editor.document.languageId !== 'merc' || editor.document.uri.scheme !== 'file') {
		window.showErrorMessage('merc: Generate Full Specification only works on an open, saved .mcrl2 file.');
		return;
	}
	if (!client) {
		window.showErrorMessage('merc: the language server is not running.');
		return;
	}

	const sourcePath = editor.document.uri.fsPath;
	const text = await client.sendRequest(generateFullSpecRequest, { uri: editor.document.uri.toString() });
	if (text === null) {
		window.showErrorMessage(
			'merc: could not generate a full specification — save the file and make sure it parses without errors.'
		);
		return;
	}

	const outputPath = path.join(path.dirname(sourcePath), `${path.basename(sourcePath, path.extname(sourcePath))}.generated.mcrl2`);
	fs.writeFileSync(outputPath, text);

	const document = await workspace.openTextDocument(outputPath);
	await window.showTextDocument(document, { preview: false });
}

/**
 * Registers the `merc-builtin:` read-only content provider.
 */
function registerVirtualDocumentProvider(context: ExtensionContext) {
	context.subscriptions.push(
		workspace.registerTextDocumentContentProvider(VIRTUAL_DOCUMENT_SCHEME, {
			provideTextDocumentContent: async (uri: Uri, _token: CancellationToken) => {
				if (!client) {
					return '';
				}
				const content = await client.sendRequest(virtualDocumentRequest, { uri: uri.toString() });
				return content ?? `% ${uri.toString()} is no longer available from merc-lsp.`;
			}
		})
	);
}

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

/**
 * Builds and starts a fresh {@link LanguageClient}, spawning a new `merc-lsp` process. Used both
 * by {@link activate} and by the `merc-lsp.restartServer` command below — the same steps either
 * way, since restarting *is* "stop the old client if any, then activate a new one".
 */
function startClient(context: ExtensionContext): LanguageClient {
	const command = resolveServerCommand(context);
	output?.appendLine(`Using merc-lsp binary: ${command}`);

	// Both `run` and `debug` launch the same binary the same way: `merc-lsp` speaks LSP over
	// stdio unconditionally, there's no separate debug mode to select (unlike the Node.js
	// language-server template this extension started from).
	const run = { command, transport: TransportKind.stdio };
	const serverOptions: ServerOptions = { run, debug: run };

	const clientOptions: LanguageClientOptions = {
		documentSelector: [
			{ scheme: 'file', language: 'merc' },
			{ scheme: 'file', language: 'merc-pbes' },
			{ scheme: 'file', language: 'merc-pres' },
			{ scheme: 'file', language: 'merc-mcf' }
		],
		synchronize: {
			fileEvents: workspace.createFileSystemWatcher('**/*.{merc,pbes,pres,mcf}')
		}
	};

	const newClient = new LanguageClient(
		'merc-lsp',
		'Merc Language Server',
		serverOptions,
		clientOptions
	);

	newClient.start().then(
		() => output?.appendLine('merc-lsp language server started.'),
		(error: unknown) => {
			const message = `Failed to start merc-lsp (looked for "${command}"): ${error instanceof Error ? error.message : error
				}. Set "merc-lsp.serverPath" if it isn't on your PATH.`;
			output?.appendLine(message);
			window.showErrorMessage(message);
		}
	);

	return newClient;
}

/**
 * Stops the current client (if any) and starts a new one, respawning the `merc-lsp` process from
 * scratch. Bound to the `merc-lsp.restartServer` command (Command Palette: "merc: Restart
 * Language Server") — the extension launches the server binary exactly once, at activation, and
 * has no other way to notice a rebuilt binary or recover from the server process itself having
 * exited; this is the one manual lever for both. Unlike "Developer: Reload Window", it doesn't
 * discard the rest of the editor session to do it.
 */
async function restartClient(context: ExtensionContext): Promise<void> {
	output?.appendLine('Restarting merc-lsp language server...');
	if (client) {
		await client.stop();
	}
	client = startClient(context);
}

export function activate(context: ExtensionContext) {
	output = window.createOutputChannel('merc-lsp');
	context.subscriptions.push(output);

	output.appendLine('merc extension activated.');
	output.show(true);

	context.subscriptions.push(
		commands.registerCommand('merc-lsp.restartServer', () => restartClient(context))
	);
	context.subscriptions.push(
		commands.registerCommand('merc-lsp.generateFullSpec', () => generateFullSpec())
	);

	registerVirtualDocumentProvider(context);

	client = startClient(context);
}

export function deactivate(): Thenable<void> | undefined {
	if (!client) {
		return undefined;
	}
	return client.stop();
}
