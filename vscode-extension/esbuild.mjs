import { build } from "esbuild";

const shared = {
	bundle: true,
	platform: "node",
	target: "node20",
	sourcemap: true,
	external: ["vscode"],
	logLevel: "info",
};

await Promise.all([
	build({
		...shared,
		entryPoints: ["src/extension.ts"],
		outfile: "dist/extension.cjs",
		format: "cjs",
	}),
	build({
		...shared,
		entryPoints: ["src/test/unit.test.ts"],
		outfile: "dist/test/unit.test.cjs",
		format: "cjs",
	}),
	build({
		...shared,
		entryPoints: ["src/test/suite/index.ts"],
		outfile: "dist/test/suite/index.cjs",
		format: "cjs",
	}),
]);
