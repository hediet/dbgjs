export const chromium = {
	async connectOverCDP(endpoint) {
		if (endpoint !== "mock:playwright-page") {
			throw new Error(`Unexpected fixture endpoint: ${endpoint}`);
		}
		return {
			contexts: () => [{
				pages: () => [{
					locator(selector) {
						if (selector !== "body") throw new Error(`Unexpected selector: ${selector}`);
						return { innerText: async () => "fixture body" };
					},
				}],
			}],
			close: async () => {},
		};
	},
};
