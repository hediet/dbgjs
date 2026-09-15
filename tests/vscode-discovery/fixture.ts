import { writeFileSync } from "node:fs";

export function compute(value: number): number {
	const doubled = value * 2;
	return doubled + 1;
}

export function activate(): void {
	globalThis.__dbgjsVscodeFixture = { compute, result: null };
	writeFileSync(process.env.DBGJS_VSCODE_FIXTURE_READY, JSON.stringify({ pid: process.pid }));
}
