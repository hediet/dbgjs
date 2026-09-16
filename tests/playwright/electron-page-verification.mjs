import assert from "node:assert/strict";

export async function verifySelectedElectronPage(command) {
	const tree = await command(["target", "cdp", "Page.getFrameTree"]);
	const frame = tree.frameTree.frame;
	assert.ok(frame.id);
	assert.ok(frame.url);
	const result = await command(["playwright", `
		const session = await page.context().newCDPSession(page);
		const tree = await session.send("Page.getFrameTree");
		const info = await session.send("Target.getTargetInfo");
		const raw = await session.send("Runtime.evaluate", {
			expression: "document.title", returnByValue: true,
		});
		const title = await page.title();
		const titleElement = await page.locator("title").textContent();
		const url = page.url();
		await session.detach();
		const afterDetach = await page.title();
		return {
			targetId: info.targetInfo.targetId,
			frameId: tree.frameTree.frame.id,
			url,
			frameUrl: tree.frameTree.frame.url,
			titleMatches: title === raw.result.value && title === titleElement && title === afterDetach,
			titleNonempty: title.length > 0,
			pageCount: page.context().pages().length,
		};
	`]);
	assert.equal(result.targetId, frame.id, "Playwright target identity must equal the Chromium root frame.");
	assert.equal(result.frameId, frame.id, "Frame IDs must pass through unchanged.");
	assert.equal(result.url, frame.url, "Playwright must initialize the selected page URL.");
	assert.equal(result.frameUrl, frame.url);
	assert.equal(result.titleMatches, true, "Title, utility-world locator, raw evaluation, and post-detach title must agree.");
	assert.equal(result.titleNonempty, true);
	assert.equal(result.pageCount, 1);
	return result;
}
