import { createInterface } from 'node:readline';
import { runInNewContext } from 'node:vm';

const targets = [
	{ targetId: 'browser/renderer/target/frame', title: 'Duplicate title', openerId: 'renderer/target/frame' },
	{ targetId: 'renderer/target/frame', title: 'Duplicate title' },
	{ targetId: 'decoy', title: 'browser/browser/renderer/target/frame@1 browser/browser/renderer/target/frame@2' },
].map(target => ({ ...target, type: 'page', url: 'https://fixture.test/', attached: false, canAccessOpener: false }));
const sessions = new Map();
const objects = new Map();
const remote = value => {
	if (value !== null && typeof value === 'object') {
		const objectId = `object-${objects.size + 1}`;
		objects.set(objectId, value);
		return { type: 'object', objectId };
	}
	return { type: typeof value, value };
};
let nextSession = 0;
const send = message => process.stdout.write(`${JSON.stringify(message)}\n`);
createInterface({ input: process.stdin }).on('line', line => {
	const message = JSON.parse(line);
	let result = {};
	switch (message.method) {
		case 'Browser.getVersion':
			result = {
				protocolVersion: '1.3', product: 'SelectorFixture/1.0',
				revision: '1', userAgent: 'SelectorFixture', jsVersion: '1',
			};
			break;
		case 'Target.getTargets':
			result = { targetInfos: targets };
			break;
		case 'Target.attachToTarget':
			result = { sessionId: `session-${++nextSession}` };
			sessions.set(result.sessionId, message.params.targetId);
			break;
		case 'Debugger.enable':
			result = { debuggerId: message.sessionId };
			break;
		case 'Runtime.evaluate':
			result = { result: remote(runInNewContext(message.params.expression, {
				identity: sessions.get(message.sessionId),
			})) };
			break;
		case 'Runtime.callFunctionOn':
			result = { result: remote(runInNewContext(`(${message.params.functionDeclaration})`)
				.call(objects.get(message.params.objectId))) };
			break;
		case 'Runtime.getProperties':
			result = {
				result: Object.entries(objects.get(message.params.objectId))
					.map(([name, value]) => ({
						name, value: remote(value), configurable: true, enumerable: true, writable: true,
					})),
			};
			break;
	}
	send({ id: message.id, sessionId: message.sessionId, result });
	if (message.method === 'Runtime.evaluate') {
		send({
			sessionId: message.sessionId,
			method: 'Runtime.consoleAPICalled',
			params: {
				type: 'log',
				args: [{ type: 'string', value: sessions.get(message.sessionId) }],
				executionContextId: 1,
				timestamp: Date.now(),
			},
		});
	}
});
