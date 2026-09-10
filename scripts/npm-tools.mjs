import { execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";

export function npm(args, options = {}) {
	if (process.platform !== "win32") {
		return execFileSync("npm", args, { encoding: "utf8", ...options });
	}
	const npmCmd = execFileSync("where.exe", ["npm.cmd"], { encoding: "utf8" }).trim().split(/\r?\n/)[0];
	const cli = process.env.npm_execpath ?? join(dirname(npmCmd), "node_modules", "npm", "bin", "npm-cli.js");
	if (!existsSync(cli)) throw new Error(`Cannot locate npm CLI at ${cli}.`);
	return execFileSync(process.execPath, [cli, ...args], { encoding: "utf8", ...options });
}
