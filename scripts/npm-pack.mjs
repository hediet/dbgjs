import { chmod, copyFile, cp, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { platforms } from "../npm/jsdbg/lib/platform.mjs";
import { npm } from "./npm-tools.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const { values } = parseArgs({ options: {
	platform: { type: "string" },
	"bin-dir": { type: "string" },
	output: { type: "string" },
} });
const platform = platforms[values.platform];
if (!platform || !values["bin-dir"] || !values.output) {
	throw new Error("Usage: node scripts/npm-pack.mjs --platform <platform> --bin-dir <binaries> --output <tarballs>");
}
const output = resolve(values.output);
const manifest = JSON.parse(await readFile(join(root, "npm", "jsdbg", "package.json"), "utf8"));
const cargo = await readFile(join(root, "Cargo.toml"), "utf8");
if (!cargo.includes(`version = "${manifest.version}"`)) throw new Error("Cargo and npm package versions must match.");
for (const key of Object.keys(platforms)) {
	if (manifest.optionalDependencies[`@hediet/jsdbg-${key}`] !== manifest.version) {
		throw new Error(`Optional dependency ${key} must use exact version ${manifest.version}.`);
	}
}
const staging = await mkdtemp(join(tmpdir(), "jsdbg-pack-"));
try {
	const native = join(staging, "native");
	const entry = join(staging, "entry");
	await mkdir(join(native, "bin"), { recursive: true });
	await mkdir(output, { recursive: true });
	for (const name of ["jsdbg", "jsdbg-service", "jsdbg-tui"]) {
		const filename = `${name}${platform.os === "win32" ? ".exe" : ""}`;
		await copyFile(join(resolve(values["bin-dir"]), filename), join(native, "bin", filename));
		await chmod(join(native, "bin", filename), 0o755);
	}
	await writeFile(join(native, "package.json"), JSON.stringify({
		name: `@hediet/jsdbg-${values.platform}`,
		version: manifest.version,
		description: `Native binaries for @hediet/jsdbg (${values.platform})`,
		license: manifest.license,
		os: [platform.os],
		cpu: [platform.cpu],
		...(platform.libc ? { libc: [platform.libc] } : {}),
		files: ["bin"],
	}, null, 2) + "\n");
	await cp(join(root, "npm", "jsdbg"), entry, { recursive: true });
	for (const name of ["jsdbg", "jsdbg-tui"]) await chmod(join(entry, "bin", `${name}.mjs`), 0o755);
	for (const directory of [native, entry]) {
		const packed = JSON.parse(npm(["pack", "--json", "--ignore-scripts", "--pack-destination", output], { cwd: directory }));
		if (packed.length !== 1 || !packed[0].filename.endsWith(".tgz")) throw new Error("npm pack did not return exactly one tarball.");
		console.log(join(output, packed[0].filename));
	}
} finally {
	await rm(staging, { recursive: true, force: true });
}
