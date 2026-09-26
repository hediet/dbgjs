# Idea: source discovery and reconstruction

Status: **exploratory and deferred; not approved for implementation**. This
document does not define committed CLI syntax or an implementation plan.

## Motivation

A runtime can provide generated JavaScript and a valid source map while still
not providing the authored JavaScript or TypeScript content.

For example, a map can contain:

```text
generated mcp.js 126:43
  -> ../../../src/server/mcp.ts 214:43
```

without containing `sourcesContent`. In that case the source map tells us the
logical authored path and the position inside that source, but not the bytes of
the source itself.

We do not need to guess what source path the map refers to. Resolving
`sourceRoot`, the map URL, and the `sources` entry gives us a logical source
identity. We may, however, need to discover or reconstruct content for that
identity and establish how confidently it corresponds to the running code.

The debugger should remain useful throughout this process:

- Generated JavaScript is always a usable fallback.
- Formatting generated JavaScript should not imply that original source was
  recovered.
- A plausible file found in a workspace, package, or repository should not be
  treated as authentic without evidence.
- AI-generated readable source should not be presented as original source.
- Partial evidence should remain partial rather than being promoted to a
  whole-file claim.

## Missing-source warnings

Missing authored content should be a visible, nonfatal warning.

There are two useful warning moments:

1. When a source map is loaded and one of its logical sources has no content
   candidate.
2. When a requested frame, breakpoint, or source lookup reaches such a source.

The first warning explains the underlying source-view limitation once. The
second explains why a particular operation falls back to generated JavaScript.
Warnings should be deduplicated by map and logical source so a paused stack
does not print the same warning for every frame.

A structured diagnostic could contain:

```text
code: authored-source-content-unavailable
generatedUrl: ...
sourceMapUrl: ...
sourceRoot: ...
sourceEntry: ../../../src/server/mcp.ts
logicalSourceUrl: ...
mappingQuality: exact | greatest-lower-bound
contentAttempts:
  - sourcesContent: absent
  - workspace: not-found
  - filesystem: not-tried
  - network: disabled
fallback: generated-source
```

Human output should be concise:

```text
warning: source map maps this frame to ../../../src/server/mcp.ts:214:43,
but authored source content is unavailable; using generated mcp.js:126:43
```

Machine-readable output should retain the complete diagnostic and its stable
diagnostic ID. A later source-resolution attempt can update the diagnostic
without rewriting debugger history.

This is a warning, not a source-map failure. The map answered the positional
question; content resolution failed afterward. Evaluation, stepping, resume,
and raw protocol access must remain available.

## Capabilities

The design should support these workflows:

- Display generated JavaScript when no authored content exists.
- Pretty-print generated JavaScript with exact navigation back to runtime
  positions.
- Resolve a logical source path from embedded content, a workspace, a package,
  a repository, or a user-provided resolver.
- Rank several possible source files without silently choosing an ambiguous
  candidate.
- Reject a candidate that contradicts reliable source-map evidence.
- Certify that two source artifacts differ only by formatting or only by
  binding-aware identifier renaming.
- Represent weaker transformations without calling them proven.
- Reconstruct only the function or range needed for the current pause.
- Use AI to propose readable source while separately checking which projection
  properties can be established statically.
- Cache discoveries and certificates reproducibly by content hash.
- Operate without network or AI access.

## Concepts

The concepts should remain independent:

### Source artifact

An immutable, content-addressed byte sequence plus language and logical
identity metadata. Examples include runtime JavaScript, authored TypeScript,
formatted JavaScript, and a reconstructed function.

### Provenance

How an artifact was obtained:

- runtime script;
- source-map `sourcesContent`;
- workspace file;
- installed package;
- package registry artifact;
- repository URL and exact commit;
- deterministic formatter;
- heuristic reconstruction;
- AI reconstruction;
- user-provided content.

Provenance is not confidence. A file from GitHub can still be the wrong commit,
and AI output can be internally consistent without being original source.

### Projection

A mapping from positions or ranges in artifact `A` to artifact `B`.
Source maps, formatter edit maps, AST correspondences, and manually supplied
range mappings are different projection implementations with the same role.

### Resolver

A producer of candidate artifacts for a logical source identity. A resolver
does not decide that its candidates are correct.

### Matcher

An observer that compares runtime source, source-map evidence, and a candidate.
It emits evidence, contradictions, coverage, and a rank.

### Reconstruction

A transform that produces a new artifact. Formatting, deminification,
identifier synthesis, and AI rewriting are reconstruction strategies with
different claims.

### Transformation certificate

A machine-checkable description of which relation between two artifacts was
verified, over which ranges, by which verifier, and with which limitations.

Keeping these concepts separate prevents a source finder, formatter, or AI
model from becoming an implicit authenticity oracle.

## Primitive basis

A small basis could be:

```text
resolve(logical source, context) -> candidate artifacts
match(runtime artifact, source map, candidate) -> source match
transform(input artifact, strategy, range?) -> output artifact + projection
verify(input artifact, output artifact, claimed relation, range?)
  -> transformation certificate
select(candidates, policy) -> selected candidate | ambiguity
render(artifact, projection, diagnostics) -> view
```

Repository search, package lookup, formatting, AI reconstruction, and partial
reconstruction compose from these primitives. They should not each introduce a
second source model.

## Candidate discovery

Candidate discovery should be an ordered, configurable resolver chain:

1. `sourcesContent`.
2. Already known content with the same content hash.
3. Workspace and explicitly configured source roots.
4. Paths relative to the map and generated script.
5. Installed package contents and package metadata.
6. Package registry artifacts for an exact package version.
7. Repository metadata embedded in a package, map, or build manifest.
8. A repository at an exact commit or release.
9. User-provided resolver plugins.
10. Reconstruction when no authentic candidate can be found.

Each result should include its exact provenance and all resolution steps. A
repository candidate should record the repository URL, commit, and path rather
than merely saying "found on GitHub."

Network resolvers should be opt-in and policy controlled. Runtime source may be
private; it must not be uploaded to a search service or AI provider without
explicit authorization. License and content-exclusion policies also apply to
retrieved source.

### Dynamically supplied resolvers

A JavaScript resolver could be useful for project-specific path conventions:

```javascript
export async function resolveSource(request, capabilities) {
  // Return zero or more content-addressed candidates with provenance.
}
```

The request might contain map metadata, logical URLs, package metadata, and
content hashes. It should not expose arbitrary debugger capabilities.

Arbitrary resolver JavaScript should not execute inside the debugger service.
Safer options include:

- a separate process with a versioned JSON protocol;
- an explicitly trusted workspace extension host;
- a restricted JavaScript or WebAssembly runtime;
- declarative path and URL rewrite rules for simple cases.

Resolvers produce candidates only. The same matcher and selection policy should
apply regardless of whether a candidate came from a built-in resolver or a
script.

## Source-match function

The motivating function can be written as:

```text
P(runningSource, sourceMap, potentialSource) -> number
```

Calling this value a probability would be misleading unless it is calibrated
against a representative corpus. A more honest initial result is:

```text
match(runningSource, sourceMap, potentialSource) -> {
  compatibility: incompatible | plausible | verified
  score: number
  coverage: SourceRangeSet
  evidence: Evidence[]
  contradictions: Contradiction[]
}
```

`compatibility` is a semantic gate. `score` ranks candidates that passed the
gate. `coverage` prevents a strong match for ten lines from being mistaken for
a strong whole-file match.

### Hard evidence and rejection

Possible hard contradictions include:

- mapped authored positions fall outside the candidate;
- a mapping that explicitly names an identifier lands on a non-identifier;
- a reliable source-map name disagrees with the candidate identifier;
- source-map ordering is impossible for the candidate ranges;
- a known build hash, package version, or commit excludes the candidate;
- rebuilding with a known deterministic toolchain produces a different
  generated artifact or incompatible map.

Identifier checks must be source-map aware. The `names` table is optional and
some producers record names incompletely. Missing name evidence is not a
contradiction. When a mapping does carry a reliable name, however, mapping it
to punctuation or an incompatible token should reject the candidate.

### Ranking evidence

Ranking evidence can include:

- exact content hash or signed build metadata;
- exact repository commit and package version;
- source URL and path agreement;
- percentage of mapped positions that land on compatible token classes;
- source-map name agreement;
- local ordering and span consistency;
- AST-node correspondence near mapped points;
- generated output reproduced by a known compiler or bundler configuration;
- agreement across neighboring files from the same build.

Scores should not be summed blindly. Independent evidence should increase
confidence more than several correlated path-name checks. The result should
retain the evidence vector so a user can understand why one candidate won.

### Selection laws

- A contradiction cannot be overridden by a high heuristic score.
- No candidate is better than an incompatible candidate.
- Equal plausible candidates remain ambiguous.
- Adding an unrelated candidate does not change evidence for existing
  candidates.
- Whole-file confidence cannot exceed the confidence of uncovered regions.
- Exact content identity implies verified compatibility.
- A selected candidate never loses its provenance or discarded alternatives.

If a probabilistic score is added later, it should be calibrated and versioned.
The evidence and compatibility result remain the durable data.

## Formatting and deminification

Formatting generated JavaScript is the safest fallback because it needs no
authored source. A deterministic formatter can return:

```text
runtime JavaScript --format projection--> formatted JavaScript
```

The formatter must produce an exact position projection, including unmappable
inserted whitespace. This is a presentation transform, not source recovery.

"Deminification" should be split into more precise operations:

- pretty-printing and trivia normalization;
- safe parenthesis or block reconstruction;
- binding-aware synthesized identifier names;
- constant-expression presentation;
- control-flow restructuring;
- authored-source recovery.

Pretty-printing can often be certified exactly. Synthesized names may improve
readability but are not the original identifiers. Control-flow restructuring
and authored-source recovery have much weaker claims and should not inherit the
confidence of formatting.

The UI should use labels such as `formatted generated source` or
`reconstructed source`, never `original source`, unless provenance establishes
that claim.

## Transformation classification

For:

```text
A --T--> B
```

the verifier should answer more than a single confidence number. A certificate
could be:

```text
relation:
  identity
  formatting-only
  comments-and-trivia-only
  identifier-renaming-only
  syntax-lowering
  structure-preserving-rewrite
  source-map-consistent
  unknown
coverage: ranges in A and B
projection: position/range mapping
verifier: name and version
evidence: ...
limitations: ...
```

Useful static checks include:

- **Identity:** byte-for-byte equality.
- **Formatting only:** equivalent token stream after removing trivia, with
  identical identifiers and literals.
- **Comments and trivia only:** AST and token equivalence except for comments
  and whitespace.
- **Identifier renaming only:** binding-aware alpha equivalence with a
  consistent bijection per lexical binding.
- **Syntax lowering:** correspondence through a known compiler transform and
  configuration.
- **Source-map consistent:** mapped ranges and names agree even though stronger
  semantic equivalence was not established.
- **Unknown:** no stronger relation was proved.

Identifier-renaming verification must account for dynamic features such as
`eval`, `with`, reflected function text, property keys, serialization, and
string-based lookups. A syntactic rename can be binding-consistent without
being behavior preserving in those cases. The certificate should state which
claim was checked rather than promising general semantic equivalence.

Certificates compose conservatively:

- identity does not weaken another certificate;
- formatting followed by formatting is formatting;
- formatting followed by identifier renaming is at most identifier renaming
  plus formatting;
- range coverage composes through the intersection of mapped ranges;
- any unverified step makes the composed relation unknown outside its verified
  coverage.

This makes projection checking useful for AI output as well as deterministic
transforms.

## AI reconstruction

AI can propose a readable artifact from generated source, source-map names,
nearby package metadata, types, stack values, or a partial authentic source
candidate. The proposal must remain separate from verification:

```text
generated source
  -> AI proposal
  -> deterministic projection and relation checks
  -> certificate + unresolved differences
```

The model can also propose a projection, but it cannot certify its own output.
Static verifiers should independently establish formatting equivalence,
binding-aware identifier correspondence, token compatibility, and source-map
agreement.

The result might therefore say:

```text
provenance: AI reconstruction
verified:
  - source-map-consistent for generated lines 120-145
  - identifier-renaming-only for one function
unknown:
  - reconstructed type annotations
  - recovered comments
  - control-flow simplification
```

AI-generated comments, types, names, and abstractions are hypotheses. They can
be valuable for understanding code without being evidence of original source.
Debugger navigation should use only the certified projection regions.

AI use must be explicitly enabled and respect data-sharing policy. A local
model is a different resolver policy, not a different source representation.

## Partial reconstruction

Partial reconstruction should be first-class rather than a truncated whole
file. A source fragment contains:

- its generated range;
- its logical authored range, when the source map provides one;
- reconstructed content;
- projection coverage;
- provenance and transformation certificate;
- required surrounding context;
- unresolved edges.

Useful scopes include:

- the current generated statement;
- one function;
- one stack frame's mapped authored range;
- a selected line range;
- the transitive local bindings needed to understand an expression.

A partial workflow could be:

```text
select paused frame
  -> derive generated and authored ranges from the map
  -> discover authentic candidate fragments
  -> optionally reconstruct the missing fragment
  -> verify projection for that fragment
  -> render it beside generated source
```

Coverage must be explicit. Breakpoints, stepping, or evaluation should not use
the reconstructed projection outside certified ranges.

## Composition examples

### Missing TypeScript with readable fallback

```text
runtime JS
  -> deterministic format
  -> exact formatter projection
  -> render formatted generated source
```

No source authenticity claim is needed.

### Find the matching source in a repository

```text
logical source identity
  -> repository resolvers
  -> candidate artifacts
  -> match each candidate against runtime JS and source map
  -> select one verified candidate or report ambiguity
```

Repository discovery and authenticity checking remain separate.

### AI reconstruction of the paused function

```text
paused frame
  -> select function range
  -> AI transform
  -> verify formatting/name/map relations
  -> partial certificate
  -> render generated and reconstructed views
```

The same renderer can show exact, plausible, and unknown regions differently.

## Failure and ambiguity

- Resolver failure is diagnostic data, not an empty successful result.
- Network-disabled and network-failed are different outcomes.
- Missing `sourcesContent` is different from a malformed source map.
- A valid map with missing authored content is different from an unmapped
  generated position.
- Candidate ambiguity must be presented to the user.
- Reconstruction failure leaves generated source available.
- A partial certificate must never be interpreted as whole-file verification.
- Stale candidates are invalidated when runtime source or source-map content
  hashes change.

## Persistence and reproducibility

Artifacts, projections, evidence, and certificates should be content-addressed.
A persisted resolution should record:

- runtime source hash;
- source-map hash;
- candidate source hash;
- resolver and verifier versions;
- repository commit or package version;
- policy inputs;
- covered ranges.

This allows a result to survive debugger restart without pretending ephemeral
runtime handles are durable. It also makes confidence changes explainable when
a resolver or verifier is upgraded.

## Conveniences

Possible future commands or UI actions may abbreviate common compositions:

- show formatted generated source;
- find authored source;
- explain source confidence;
- reconstruct current function;
- compare generated and candidate source.

These should remain conveniences over resolution, matching, transformation,
verification, selection, and rendering. Syntax is intentionally deferred.

## Deferred questions

- Which missing-content diagnostics should be emitted eagerly versus only when
  a mapped source is requested?
- Should trusted resolver plugins use a process protocol, WebAssembly, or a
  restricted JavaScript runtime?
- Which source-map producer quirks make identifier evidence unreliable?
- Can known package build systems provide reproducible compiler fingerprints?
- How should confidence be calibrated if the ranking score is eventually
  exposed as a probability?
- Which transformation relations can be verified cheaply enough during a
  paused debugging session?
- How should the UI visualize mixed confidence and partial coverage?
- Should repository and registry lookup be a debugger capability or an
  external resolver service?
- How should user corrections become durable evidence without being mistaken
  for original provenance?
