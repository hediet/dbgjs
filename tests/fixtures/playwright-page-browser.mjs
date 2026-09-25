export const chromium = {
	async connectOverCDP(endpoint) {
		if (endpoint !== "mock:playwright-page") {
			throw new Error(`Unexpected fixture endpoint: ${endpoint}`);
		}
		if (process.env.DBGJS_FIXTURE_PHASE === "connecting") {
			await stall();
		}
		if (process.env.DBGJS_FIXTURE_PHASE === "connection-error") {
			throw new Error(`fixture proxy setup failed at ${endpoint}`);
		}
		if (process.env.DBGJS_FIXTURE_PHASE === "identity-error") {
			throw new Error("Playwright proxy page identity lookup failed at Page.getFrameTree");
		}
		return {
			contexts: () => {
				if (process.env.DBGJS_FIXTURE_PHASE === "initialization-error") {
					throw new Error("fixture frame initialization failed");
				}
				return [{
					pages: () => [{
						locator(selector) {
							if (selector !== "body") throw new Error(`Unexpected selector: ${selector}`);
							return { innerText: async () => "fixture body" };
						},
					}],
				}];
			},
			close: async () => {
				if (process.env.DBGJS_FIXTURE_PHASE === "closing") {
					await stall();
				}
				if (process.env.DBGJS_FIXTURE_PHASE === "closing-error") {
					throw new Error(`fixture closing failed at ${endpoint}`);
				}
			},
		};
	},
};

function stall() {
	return new Promise(() => setInterval(() => {}, 1_000));
}
