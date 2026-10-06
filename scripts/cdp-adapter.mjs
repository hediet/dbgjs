// CDP JSON -> LinkRPC definitions. Rust lowering and source layout belong to LinkRPC.
const CODEGEN = 'x-linkrpc-codegen';
const refPrefix = '#/components/schemas/';
const clone = (value) => structuredClone(value);

function insert(object, key, value) {
	if (Object.hasOwn(object, key)) throw new Error(`duplicate CDP schema entry: ${key}`);
	object[key] = value;
}

function required(value, key) {
	if (typeof value[key] !== 'string') throw new Error(`expected ${key}`);
	return value[key];
}

function fields(raw = [], domain, optional = []) {
	const properties = {};
	const required = [];
	for (const field of raw) {
		if (typeof field.name !== 'string') continue;
		properties[field.name] = convert(optional.includes(field.name) ? { ...field, optional: true } : field, domain);
		if (!field.optional && !optional.includes(field.name)) required.push(field.name);
	}
	return { type: 'object', properties, additionalProperties: false, ...(required.length ? { required } : {}) };
}

function convert(raw, domain) {
	let schema = {};
	if (raw.$ref) {
		schema.$ref = `${refPrefix}${raw.$ref.includes('.') ? raw.$ref : `${domain}.${raw.$ref}`}`;
	} else if (raw.type === 'object') {
		schema = raw.properties ? fields(raw.properties, domain) : { type: 'object', additionalProperties: true };
	} else if (raw.type === 'array') {
		schema = { type: 'array', items: convert(raw.items ?? {}, domain) };
	} else if (raw.type && raw.type !== 'any') {
		schema.type = raw.type;
	}
	if (raw.id) schema.title = raw.id;
	for (const key of ['description', 'enum']) if (key in raw) schema[key] = clone(raw[key]);
	schema[CODEGEN] = {
		cdpType: raw.type ?? null, originalRef: raw.$ref ?? null,
		optional: raw.optional ?? false, experimental: raw.experimental ?? false,
		deprecated: raw.deprecated ?? false,
	};
	return schema;
}

function method(raw, domain, kind) {
	const wireMethod = `${domain}.${required(raw, 'name')}`;
	const params = fields(raw.parameters, domain,
		['Debugger.scriptParsed', 'Debugger.scriptFailedToParse'].includes(wireMethod) ? ['buildId'] : []);
	if (wireMethod === 'Target.attachToTarget') params.properties.__dbgjsAutoAttach = convert({ type: 'boolean', optional: true }, domain);
	if (wireMethod === 'Debugger.paused') delete params.properties.reason.enum;
	return {
		params, ...(kind === 'request' ? { result: fields(raw.returns, domain) } : {}),
		...Object.fromEntries(['description', 'deprecated'].filter((key) => key in raw).map((key) => [key, raw[key]])),
		[CODEGEN]: { profile: 'cdp', kind, wireMethod, domain, experimental: raw.experimental ?? false, redirect: raw.redirect ?? null },
	};
}

export function importCdpProtocol(...documents) {
	const methods = {}, schemas = {}, domains = {};
	for (const document of documents) {
		if (!Array.isArray(document.domains)) throw new Error('expected protocol domains array');
		for (const domain of document.domains) {
			const name = required(domain, 'domain');
			insert(domains, name, Object.fromEntries(['description', 'experimental', 'deprecated', 'dependencies']
				.filter((key) => key in domain).map((key) => [key, clone(domain[key])])));
			for (const type of domain.types ?? []) {
				const id = required(type, 'id');
				const schema = convert(type, name);
				if (name === 'Runtime' && id === 'RemoteObject' && schema.properties?.subtype?.enum) {
					for (const subtype of ['internal#location', 'internal#scope', 'internal#scopeList', 'internal#entry']) {
						if (!schema.properties.subtype.enum.includes(subtype)) schema.properties.subtype.enum.push(subtype);
					}
				}
				insert(schemas, `${name}.${id}`, schema);
			}
			for (const [key, kind] of [['commands', 'request'], ['events', 'notification']]) {
				for (const raw of domain[key] ?? []) insert(methods, `${name}.${required(raw, 'name')}`, method(raw, name, kind));
			}
		}
	}
	return {
		id: 'cdp.protocol', hash: '',
		// Normative legacy wording preserves the wire identity.
		description: 'Chrome DevTools Protocol imported as one root-addressed HubRPC compatibility interface.',
		methods, components: { schemas },
		[CODEGEN]: { profile: 'cdp', wireAddressing: 'root', sessionMultiplexing: 'transport', domains, sourceVersions: documents.map((d) => d.version ?? null) },
	};
}

function references(value, result = new Set()) {
	if (value && typeof value === 'object') {
		if (typeof value.$ref === 'string' && value.$ref.startsWith(refPrefix)) result.add(value.$ref.slice(refPrefix.length));
		for (const child of Object.values(value)) references(child, result);
	}
	return result;
}

export function importCdpDomains(...documents) {
	const imported = importCdpProtocol(...documents);
	const components = clone(imported.components.schemas);
	const domains = new Map();
	for (const [wire, original] of Object.entries(imported.methods)) {
		const [name, member] = wire.split('.');
		const kind = 'result' in original ? 'commands' : 'events';
		const method = clone(original);
		for (const [field, suffix] of [['params', 'Params'], ['result', 'Result']]) {
			if (!(field in method)) continue;
			const component = `${wire}${suffix}`;
			insert(components, component, method[field]);
			method[field] = { $ref: `${refPrefix}${component}` };
		}
		const key = `${name}:${kind}`;
		if (!domains.has(key)) domains.set(key, { name, kind, methods: {} });
		domains.get(key).methods[member] = method;
	}
	return [...domains.entries()].sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0).map(([, { name, kind, methods }]) => {
		const reachable = references(methods);
		for (const component of reachable) references(components[component], reachable);
		const prefix = `${name}.`;
		return { name, kind, prefix, schema: {
			id: `cdp.${name}${kind === 'events' ? '.events' : ''}`, hash: '', methods,
			components: { schemas: Object.fromEntries(Object.entries(components).filter(([key]) => reachable.has(key))) },
			[CODEGEN]: { profile: 'cdp', wireAddressing: 'root', wirePrefix: prefix, sessionMultiplexing: 'transport' },
			...(imported[CODEGEN].domains[name].description ? { description: imported[CODEGEN].domains[name].description } : {}),
		} };
	});
}

// Equivalent to heck's acronym-aware domain conversion; naming is adapter policy.
export function snakeCase(name) {
	return name.replace(/([A-Z]+)([A-Z][a-z])/g, '$1_$2').replace(/([a-z0-9])([A-Z])/g, '$1_$2').toLowerCase();
}

export function rustPackage(domains) {
	const facades = [
		{ name: 'CdpClient', catalog: 'command_interfaces', description: 'Command contracts offered by a CDP endpoint. Register separately from event contracts.', members: [] },
		{ name: 'CdpEventsClient', catalog: 'event_interfaces', description: 'Notification contracts implemented by a CDP consumer, using the same CDP wire prefixes.', members: [] },
	];
	const modules = domains.map(({ name, kind, prefix, schema }) => {
		const accessor = snakeCase(name);
		const contractName = `${name}${kind === 'events' ? 'Events' : ''}`;
		const module = `${accessor}${kind === 'events' ? '_events' : ''}`;
		facades[kind === 'events' ? 1 : 0].members.push({ name: accessor, module, binding: 'DOMAIN' });
		return { name: module, schema, options: {
			clientName: `${contractName}Client`, methodTypePrefix: contractName,
			generateServer: true, defaultServerMethods: true,
			bindings: [{ name: 'DOMAIN', address: { kind: 'bare', value: prefix } }],
		} };
	});
	return { modules, facades, header: '// @generated by npm run generate:cdp. DO NOT EDIT.\n' };
}
