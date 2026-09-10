export const platforms = {
	"win32-x64": { os: "win32", cpu: "x64" },
	"darwin-x64": { os: "darwin", cpu: "x64" },
	"darwin-arm64": { os: "darwin", cpu: "arm64" },
	"linux-x64-gnu": { os: "linux", cpu: "x64", libc: "glibc" },
	"linux-arm64-gnu": { os: "linux", cpu: "arm64", libc: "glibc" },
};

export function platformKey(os, arch, glibc) {
	const key = `${os}-${arch}${os === "linux" ? "-gnu" : ""}`;
	if (!platforms[key] || (os === "linux" && !glibc)) {
		throw new Error(`Unsupported jsdbg platform: ${os}/${arch}${os === "linux" && !glibc ? " (musl)" : ""}. Supported: ${Object.keys(platforms).join(", ")}.`);
	}
	return key;
}
