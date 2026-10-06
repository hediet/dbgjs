import { readFileSync, readdirSync, mkdirSync, writeFileSync, existsSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { join } from 'node:path';
import { importCdpDomains, rustPackage } from './cdp-adapter.mjs';

const root = fileURLToPath(new URL('../', import.meta.url));
const expectedFallbacks = [
	'AccessibilityAxvalueValue', 'DomShapeOutsideInfoMarginShapeItem', 'DomShapeOutsideInfoShapeItem',
	'RuntimeCallArgumentValue', 'RuntimeDeepSerializedValueValue', 'RuntimeRemoteObjectValue',
	'WebMcpToolRespondedParamsOutput',
].sort();

export function codegenBinary() {
	if (process.env.LINKRPC_CODEGEN) return process.env.LINKRPC_CODEGEN;
	const cargo = readFileSync(join(root, 'Cargo.toml'), 'utf8');
	const version = cargo.match(/^linkrpc = "=([^"]+)"$/m)?.[1];
	if (!version) throw new Error('expected an exact published LinkRPC version in Cargo.toml');
	const installRoot = join(root, 'node_modules/.cache/linkrpc-codegen', version);
	const binary = join(installRoot, 'bin', `linkrpc-codegen${process.platform === 'win32' ? '.exe' : ''}`);
	if (!existsSync(binary)) {
		const installed = spawnSync('cargo', ['install', '--locked', '--root', installRoot, '--version', version, 'linkrpc', '--bin', 'linkrpc-codegen'], {
			stdio: 'inherit',
			env: { ...process.env, CARGO_TARGET_DIR: process.env.CARGO_TARGET_DIR ?? join(installRoot, 'target'), CARGO_BUILD_JOBS: '2', CARGO_INCREMENTAL: '0' },
		});
		if (installed.error) throw installed.error;
		if (installed.status !== 0) throw new Error(`installing linkrpc-codegen ${version} failed`);
	}
	return binary;
}

export function generate(packageDefinition, binary = codegenBinary()) {
	const result = spawnSync(binary, [], { input: JSON.stringify(packageDefinition), encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 });
	if (result.error) throw result.error;
	if (result.status !== 0) throw new Error(`LinkRPC Rust generation failed:\n${result.stderr}`);
	return JSON.parse(result.stdout);
}

export function verifyFallbacks(unsupported) {
	const names = unsupported.map((message) => message.match(/^component `([^`]+)`/)?.[1]).sort();
	if (JSON.stringify(names) !== JSON.stringify(expectedFallbacks)) throw new Error(`unexpected CDP Rust generation fallbacks:\n${unsupported.join('\n')}`);
}

function main() {
	const args = process.argv.slice(2);
	const check = args.length === 1 && args[0] === '--check';
	const definitions = args.length === 1 && args[0] === '--definitions';
	if (args.length && !check && !definitions) throw new Error('usage: generate-cdp [--check|--definitions]');
	const documents = ['browser_protocol.json', 'js_protocol.json'].map((name) =>
		JSON.parse(readFileSync(join(root, 'node_modules/devtools-protocol/json', name), 'utf8')));
	const input = rustPackage(importCdpDomains(...documents));
	if (definitions) { process.stdout.write(`${JSON.stringify(input)}\n`); return; }
	const { files, unsupported } = generate(input);
	verifyFallbacks(unsupported);
	const directory = join(root, 'packages/cdp-protocol/src/generated');
	if (existsSync(directory)) for (const name of readdirSync(directory)) if (!(name in files)) throw new Error(`obsolete generated output: ${name}`);
	if (!check) mkdirSync(directory, { recursive: true });
	for (const [name, code] of Object.entries(files)) {
		const path = join(directory, name);
		if (check) {
			if (readFileSync(path, 'utf8') !== code) throw new Error(`${path} is stale; run npm run generate:cdp`);
		} else writeFileSync(path, code);
	}
}

if (process.argv[1] === fileURLToPath(import.meta.url)) main();
