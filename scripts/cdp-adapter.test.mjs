import assert from 'node:assert/strict';
import { test } from 'node:test';
import { readFileSync } from 'node:fs';
import { importCdpProtocol, importCdpDomains, rustPackage, snakeCase } from './cdp-adapter.mjs';
import { generate, verifyFallbacks } from './generate-cdp.mjs';

const CODEGEN = 'x-linkrpc-codegen';
const documents = ['browser', 'js'].map((name) => JSON.parse(readFileSync(
	new URL(`../node_modules/devtools-protocol/json/${name}_protocol.json`, import.meta.url), 'utf8')));
const imported = importCdpProtocol(...documents);
const domains = importCdpDomains(...documents);
const generated = generate(rustPackage(domains));

test('domain interfaces have local members and shared component identity', () => {
	const runtime = domains.find((d) => d.name === 'Runtime' && d.kind === 'commands');
	const debuggerEvents = domains.find((d) => d.name === 'Debugger' && d.kind === 'events');
	assert.equal(runtime.prefix, 'Runtime.');
	assert.equal(runtime.schema.id, 'cdp.Runtime');
	assert.ok(runtime.schema.methods.evaluate);
	assert.ok(debuggerEvents.schema.methods.scriptParsed);
	for (const domain of domains) assert.ok(Object.keys(domain.schema.methods).every((name) => !name.includes('.')));
	assert.deepEqual(runtime.schema.components.schemas['Runtime.RemoteObject'], debuggerEvents.schema.components.schemas['Runtime.RemoteObject']);
	assert.equal(runtime.schema.components.schemas['Debugger.Location'], undefined);
	assert.equal(runtime.schema.methods.evaluate.params.$ref, '#/components/schemas/Runtime.evaluateParams');
	assert.equal(debuggerEvents.schema.methods.scriptParsed[CODEGEN].kind, 'notification');
	for (const hash of Object.values(generated.interfaceHashes)) assert.match(hash, /^[0-9a-f]{16}$/);
});

test('directional contracts partition every upstream method without changing wire names', () => {
	const wires = new Set();
	for (const domain of domains) {
		assert.equal(domain.prefix, `${domain.name}.`);
		assert.ok(Object.keys(domain.schema.methods).length);
		for (const [member, method] of Object.entries(domain.schema.methods)) {
			const wire = `${domain.prefix}${member}`;
			assert.ok(!wires.has(wire));
			wires.add(wire);
			assert.equal('result' in method, 'result' in imported.methods[wire]);
			assert.equal('result' in method, domain.kind === 'commands');
			assert.equal(method[CODEGEN].wireMethod, wire);
		}
	}
	assert.deepEqual([...wires].sort(), Object.keys(imported.methods).sort());
});

test('command changes do not change the event contract hash', () => {
	const protocol = { domains: [{ domain: 'Example', commands: [{ name: 'enable' }], events: [{ name: 'changed' }] }] };
	const changed = structuredClone(protocol);
	changed.domains[0].commands[0].parameters = [{ name: 'enabled', type: 'boolean' }];
	const before = generate(rustPackage(importCdpDomains(protocol))).interfaceHashes;
	const after = generate(rustPackage(importCdpDomains(changed))).interfaceHashes;
	assert.notEqual(before.example, after.example);
	assert.equal(before.example_events, after.example_events);
	assert.notEqual(before.example, before.example_events);
});

test('imports the complete protocol into one interface document', () => {
	assert.equal(Object.keys(imported.methods).length, 896);
	assert.equal(Object.keys(imported.components.schemas).length, 607);
	assert.ok(imported.methods['Debugger.enable']);
	assert.ok(imported.methods['Debugger.scriptParsed']);
	assert.equal(imported.methods['Debugger.scriptParsed'].result, undefined);
});

test('generic LinkRPC computes the legacy complete interface hash', () => {
	const output = generate({ modules: [{ name: 'protocol', schema: imported }] });
	assert.equal(output.interfaceHashes.protocol, '140c9802834490c9');
});

test('interface retains hash-invisible codegen extensions', () => {
	assert.equal(imported[CODEGEN].profile, 'cdp');
	assert.equal(imported.methods['Debugger.scriptParsed'][CODEGEN].kind, 'notification');
});

test('preserves recursive references and CDP codegen metadata', () => {
	const node = imported.components.schemas['DOM.Node'];
	assert.equal(node.title, 'Node');
	assert.equal(node.properties.children.items.$ref, '#/components/schemas/DOM.Node');
	assert.equal(node.properties.children.items[CODEGEN].originalRef, 'Node');
});

test('converts CDP optionality to JSON Schema required', () => {
	const params = imported.methods['Debugger.setBreakpointByUrl'].params;
	assert.deepEqual(params.required, ['lineNumber']);
	assert.equal(params.properties.url[CODEGEN].optional, true);
});

test('tolerates buildId missing from older script events', () => {
	for (const method of ['Debugger.scriptParsed', 'Debugger.scriptFailedToParse']) {
		const params = imported.methods[method].params;
		assert.ok(!params.required.includes('buildId'));
		assert.equal(params.properties.buildId[CODEGEN].optional, true);
	}
});

test('adds the dbgjs auto-attach compatibility parameter', () => {
	const params = imported.methods['Target.attachToTarget'].params;
	assert.equal(params.properties.__dbgjsAutoAttach.type, 'boolean');
	assert.equal(params.properties.__dbgjsAutoAttach[CODEGEN].optional, true);
	assert.ok(!params.required.includes('__dbgjsAutoAttach'));
});

test('accepts Node-specific debugger pause reasons', () => {
	const reason = imported.methods['Debugger.paused'].params.properties.reason;
	assert.equal(reason.type, 'string');
	assert.equal(reason.enum, undefined);
});

test('accepts V8 internal remote object subtypes', () => {
	const subtypes = imported.components.schemas['Runtime.RemoteObject'].properties.subtype.enum;
	for (const subtype of ['internal#location', 'internal#scope', 'internal#scopeList', 'internal#entry']) assert.ok(subtypes.includes(subtype));
});

test('preserves open and closed object semantics', () => {
	assert.equal(imported.components.schemas['Network.Headers'].additionalProperties, true);
	assert.equal(imported.components.schemas['DOM.Node'].additionalProperties, false);
});

test('rich codegen extensions do not change the interface hash', () => {
	const schema = { id: 'example', hash: '', methods: { enable: { params: { type: 'boolean' }, [CODEGEN]: { profile: 'cdp' } } } };
	const before = generate({ modules: [{ name: 'example', schema }] }).interfaceHashes.example;
	schema.methods.enable[CODEGEN].generatorHint = 'more-specific-type';
	schema[CODEGEN] = { recordingHint: 'new' };
	assert.equal(generate({ modules: [{ name: 'example', schema }] }).interfaceHashes.example, before);
});

test('rejects duplicate protocol entries instead of overwriting', () => {
	const domain = { domain: 'Duplicate', commands: [{ name: 'method' }] };
	assert.throws(() => importCdpProtocol({ domains: [domain, domain] }), /duplicate CDP schema entry: Duplicate/);
	assert.throws(() => importCdpProtocol({}), /expected protocol domains array/);
	assert.throws(() => importCdpProtocol({ domains: [{}] }), /expected domain/);
	assert.throws(() => importCdpProtocol({ domains: [{ domain: 'Example', commands: [{ name: 'same' }], events: [{ name: 'same' }] }] }), /duplicate CDP schema entry: Example.same/);
});

test('all generated outputs match the legacy generator byte-for-byte and repeat deterministically', () => {
	verifyFallbacks(generated.unsupported);
	assert.deepEqual(generate(rustPackage(domains)), generated);
	for (const [name, code] of Object.entries(generated.files)) {
		assert.equal(code, readFileSync(new URL(`../packages/cdp-protocol/src/generated/${name}`, import.meta.url), 'utf8'), name);
	}
	assert.equal(Object.keys(generated.files).length, 101);
});

test('unexpected or duplicate fallbacks are not silently accepted', () => {
	assert.throws(() => verifyFallbacks([]), /unexpected CDP Rust generation fallbacks/);
	assert.throws(() => verifyFallbacks([...generated.unsupported, generated.unsupported[0]]), /unexpected CDP Rust generation fallbacks/);
});

test('domain naming and definition emission are deterministic and independent of Rust lowering', () => {
	for (const [input, output] of [['DOMDebugger', 'dom_debugger'], ['CSS', 'css'], ['WebMCP', 'web_mcp'], ['DOMStorage', 'dom_storage']]) assert.equal(snakeCase(input), output);
	assert.deepEqual(rustPackage(importCdpDomains(...documents)), rustPackage(domains));
});
