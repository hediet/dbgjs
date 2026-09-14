import { chmod, copyFile, cp, mkdir, mkdtemp, readFile, rename, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { platforms } from "../npm/dbgjs/lib/platform.mjs";
import { npm } from "./npm-tools.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const { values } = parseArgs({ options: {
	platform: { type: "string" },
	"bin-dir": { type: "string" },
	output: { type: "string" },
	candidate: { type: "boolean", default: false },
} });
const platform = platforms[values.platform];
if (!platform || !values["bin-dir"] || !values.output) {
	throw new Error("Usage: node scripts/npm-pack.mjs --platform <platform> --bin-dir <binaries> --output <tarballs>");
}
const output = resolve(values.output);
const manifest = JSON.parse(await readFile(join(root, "npm", "dbgjs", "package.json"), "utf8"));
const cargo = await readFile(join(root, "Cargo.toml"), "utf8");
if (!cargo.includes(`version = "${manifest.version}"`)) throw new Error("Cargo and npm package versions must match.");
for (const key of Object.keys(platforms)) {
	if (manifest.optionalDependencies[`@hediet/dbgjs-${key}`] !== manifest.version) {
		throw new Error(`Optional dependency ${key} must use exact version ${manifest.version}.`);
	}
}
const staging = await mkdtemp(join(tmpdir(), "dbgjs-pack-"));
try {
	const native = join(staging, "native");
	const entry = join(staging, "entry");
	await mkdir(join(native, "bin"), { recursive: true });
	await mkdir(output, { recursive: true });
	for (const name of ["dbgjs", "dbgjs-service", "dbgjs-tui"]) {
		const filename = `${name}${platform.os === "win32" ? ".exe" : ""}`;
		await copyFile(join(resolve(values["bin-dir"]), filename), join(native, "bin", filename));
		await chmod(join(native, "bin", filename), 0o755);
	}
	await writeFile(join(native, "package.json"), JSON.stringify({
		name: `@hediet/dbgjs-${values.platform}`,
		version: manifest.version,
		...(values.candidate ? { private: true } : {}),
		description: `Native binaries for @hediet/dbgjs (${values.platform})`,
		license: manifest.license,
		os: [platform.os],
		cpu: [platform.cpu],
		...(platform.libc ? { libc: [platform.libc] } : {}),
		files: ["bin"],
	}, null, 2) + "\n");
	await cp(join(root, "npm", "dbgjs"), entry, { recursive: true });
	if (values.candidate) {
		await writeFile(join(entry, "package.json"), JSON.stringify({ ...manifest, private: true }, null, 2) + "\n");
	}
	for (const name of ["dbgjs", "dbgjs-tui"]) await chmod(join(entry, "bin", `${name}.mjs`), 0o755);
	for (const directory of [native, entry]) {
		const packed = JSON.parse(npm(["pack", "--json", "--ignore-scripts", "--pack-destination", output], { cwd: directory }));
		if (packed.length !== 1 || !packed[0].filename.endsWith(".tgz")) throw new Error("npm pack did not return exactly one tarball.");
		const filename = packed[0].filename;
		if (values.candidate) {
			const candidate = filename.replace(/\.tgz$/, ".tar.gz");
			await rename(join(output, filename), join(output, candidate));
			console.log(join(output, candidate));
		} else {
			console.log(join(output, filename));
		}
	}
} finally {
	await rm(staging, { recursive: true, force: true });
}
