# Heap-based investigation: AI credits as dollars

Date: 2026-09-17.

This is a dependency-ordered walkthrough of the successful heap-based approach,
including the earlier observations it relied on. In particular, a read-only DOM
inspection supplied the text for the heap search; the heap did not independently
discover the model name. The source searches also preceded some heap queries.
This is not a claim that the successful approach was the first attempt.

Commands and outputs come from the investigation; long outputs are excerpted
and escaped JSON strings are decoded for readability. Search hypotheses are
identified separately from discovered facts. Heap IDs, remote object IDs, and
minified symbols are specific to this renderer and capture. Substitute the IDs
returned by your commands when replaying: the recorded remote IDs below are
evidence, not reusable constants.

## Goal

Present AI credit costs as USD throughout the cdp-client VS Code renderer.
The user confirmed truncation at 100 credits per dollar: **279.6 credits becomes
$2.79**, not $2.80.

The implementation changes shared presentation functions, not individual DOM
nodes:

- The shared response view-model `result` getter projects credit-bearing details
  into dollars for existing and future response instances.
- Shared credit and model-price formatters produce USD.
- Original response models, numeric accounting, unrelated result metadata, and
  the original footer rendering function remain untouched.
- Existing views refresh through their normal model-change event.

This is a reversible in-memory function patch, not a disk source edit or V8
script-source live edit. It lasts until the window reloads. No DOM walker,
mutation observer, localization override, or footer wrapper is installed.
No pre-existing transcripts were read during the runtime investigation. This
transcript itself was subsequently read at the user's request to audit and
repair missing discovery steps; no other transcripts were read.

## 1. Attach to the renderer

```powershell
dbgjs --help
dbgjs context list
dbgjs target list
dbgjs process list --vscode --no-cmd-line
dbgjs context create :credit-dollars 'AI credit dollar presentation' --set
dbgjs process attach w:11756/1 --context :credit-dollars --set
```

Relevant output:

```text
VS Code process tree 11756
  w:11756/1  window  cdp-client - Debug agents multi-diff
    p:3124  renderer  [renderer]

Context credit-dollars  rev 1
  Name: AI credit dollar presentation
Attachment: created
Target renderer-1  [running]  credit-dollars/process-tree-11756  gen 1  rev 7
```

A separate context kept this investigation apart from the existing
`bracket-ast-size` context. The workspace's old binding referred to a missing
`renderer-attach-fix` context; creating and selecting `:credit-dollars` replaced
that stale binding.

## 2. Establish the search text, then follow its heap owners

### Where the exact regex came from

The user supplied `279.6 credits`, but did **not** supply `GPT-6 Astra`. I first
looked for credit-related presentation in the live renderer. An initial
all-element text probe included stylesheet text and was too broad; a subsequent
leaf-only probe excluded styles but missed the footer's nested text. This
attribute-based, read-only probe found it:

```powershell
dbgjs target eval 'JSON.stringify({costs:[...document.querySelectorAll("[class*=cost],[class*=credit],[aria-label*=credit],[title*=credit]")].map(e=>({tag:e.tagName,cls:e.className,text:e.textContent?.slice(0,120),label:e.getAttribute("aria-label"),title:e.getAttribute("title")})).slice(0,30)})' --context :credit-dollars --full
```

Relevant decoded result:

```json
{
  "tag": "DIV",
  "cls": "chat-footer-details",
  "text": "12:37 PM995m 0s\u2022GPT-6 Astra \u2022 279.6 credits",
  "label": "Completed 9/17/2026, 12:37 PM, Elapsed time 995m 0s, GPT-6 Astra \u2022 279.6 credits, Response details. Model: GPT-6 Astra. Input tokens: 800320. Cached input tokens: 637138. Output tokens: 2386",
  "title": null
}
```

That output supplied the model name, separator, footer class, and example token
counts used later. From it, I constructed
`^GPT-6 Astra.{1,5}279\.6 credits$`:

- `GPT-6 Astra` and `279.6 credits` came from the observed footer.
- `\.` matches the decimal point literally.
- `.{1,5}` was a manually chosen tolerance for the short separator, not a value
  learned from the heap or a necessary property of the application.
- Anchors exclude longer prompt/code strings mentioning the same amount.

It is a search hypothesis based on the displayed text. A simpler broad heap
search could have started with the user's amount alone, but that was not the
command used. Reading the DOM to obtain a seed is distinct from rewriting it;
the successful patch below performs no live DOM rewrites.

### Follow the exact matching string

The heap investigation followed an earlier presentation experiment. Before
capturing, I restored its original function bindings:

```powershell
dbgjs target eval 'globalThis.__creditDollars ? globalThis.__creditDollars.restore() : "No earlier patch installed"' --context :credit-dollars
```

Output:

```text
Original formatters restored; re-render existing views to restore their labels
```

This restored functions, not every previously produced presentation string.
The captured application state was therefore not a pristine newly launched
renderer. That distinction also explains why the later live-instance check
includes some already-dollar-denominated details.

```powershell
dbgjs heap capture --id credit-functions --context :credit-dollars
dbgjs heap strings --capture credit-functions --regex '^GPT-6 Astra.{1,5}279\.6 credits$' --limit 5
dbgjs heap refs 'credit-functions#282159' --incoming --limit 10
dbgjs heap refs 'credit-functions#4358915' --incoming --limit 10
```

Relevant output:

```text
Captured credit-functions (332.2 MiB) in 14.066s:
  taking 8.664s, retrieving 5.403s.

credit-functions#282159  type:string,
  value:"GPT-6 Astra \u{2022} 279.6 credits"
  credit-functions#4358915 --property "details"--> credit-functions#282159
  credit-functions#6572995 --property "details"--> credit-functions#282159

credit-functions#4358915  type:object, value:"Object"
  credit-functions#3415327 --property "_result"--> credit-functions#4358915
```

The important discovery is that this is **already-formatted text in a response
model**, not a numeric value being formatted by the footer. Merely changing a
numeric formatter cannot fix historical responses.

The string query also reported incomplete reconstructed strings. The exact
matched node above was available, so the investigation followed that concrete
match rather than treating the search as exhaustive.

## 3. Find the shared presentation prototype

I first requested a broad class overview using conceptual search terms:

```powershell
dbgjs heap classes credit-functions --filter 'Chat.*(Model|Widget|Renderer)' --instances --max-lines 30
```

Relevant output:

```text
60 classes, 214 instances, 17.0 KiB shallow size
  contrib/chat/
    browser/  [46 classes in 39 files, 133 instances]
    common/   [8 classes in 4 files, 67 instances]
      model/  [7 classes in 3 files, 66 instances]
```

That bounded tree pruned class names. I then **hypothesized** the candidate names
`ChatResponseModel`, `ChatResponseViewModel`, and `ChatListItemRenderer` from the
response-model/rendering concepts. The exact-name filter below tested those
guesses; its results, not the earlier pruned tree, confirmed their existence and
supplied the instance IDs.

```powershell
dbgjs heap classes credit-functions --filter '^(ChatResponseModel|ChatResponseViewModel|ChatListItemRenderer)$' --instances --max-lines 60
dbgjs heap show 'credit-functions#1987451' --limit 20
dbgjs heap show 'credit-functions#1928973' --all
dbgjs heap show 'credit-functions#3415327' --limit 45
```

Relevant output:

```text
3 classes, 28 instances, 3.6 KiB shallow size
  chat/common/model/chatModel.ts
    ChatResponseModel: 17 instances
    ChatResponseModel@1  id 3415327
  chat/common/model/chatViewModel.ts
    ChatResponseViewModel: 9 instances
    ChatResponseViewModel@1  id 1987451
    ChatResponseViewModel@2  id 3415323
  chat/browser/widget/chatListRenderer.ts
    ChatListItemRenderer: 2 instances

credit-functions#1987451  type:object, value:"yY"
  property "_model"    credit-functions#1987455
  property "__proto__" credit-functions#1928973

credit-functions#1928973
  property "constructor" credit-functions#942323
  property "result"      credit-functions#4414613
  property "get result"  credit-functions#4414615

credit-functions#3415327  type:object, value:"zHe"
  property "_onDidChange" credit-functions#4358883
  property "_result" credit-functions#4358915
```

The class listing supplied `1987451`; showing it supplied prototype ID
`1928973`. Showing that prototype exposed `get result`. Separately, following
the string's incoming edges in section 2 had already supplied historical model
ID `3415327`. These IDs were copied from the preceding outputs, not guessed.

The heap connected the minified class `yY` to the authored
`ChatResponseViewModel` and exposed its shared `result` getter. That is the
presentation boundary: change what views read without changing what models store.

I resolved the retained historical model into a live CDP object:

```powershell
dbgjs target cdp HeapProfiler.getObjectByHeapObjectId --params '{"objectId":"3415327","objectGroup":"credit-investigation"}' --context :credit-dollars
```

```json
{"result":{"type":"object","className":"zHe","objectId":"-536608007814549874.1.70"}}
```

```powershell
dbgjs target cdp Runtime.callFunctionOn --params '{"objectId":"-536608007814549874.1.70","functionDeclaration":"function(){globalThis.__creditExampleModel=this;return {details:this.result?.details,rawDetails:this._result?.details}}","returnByValue":true}' --context :credit-dollars
```

```json
{
  "details": "GPT-6 Astra \u2022 279.6 credits",
  "rawDetails": "GPT-6 Astra \u2022 279.6 credits"
}
```

An attempted materialization of view-model ID `3415323` returned
`Object is not available`. Snapshot membership does not guarantee that an object
is still live. The underlying historical model above was still available; later
validation queried the currently live view models instead.

## 4. Locate the shared source functions

### Discover names before searching for them exactly

`formatCopilotCredits` was not an initial known symbol. These broad searches
were run first, using the user's term `credits` and guessed formatting terms:

```powershell
dbgjs source grep 'credits' --path workbench --ignore-case --max-results 35 --context-lines 2 --context :credit-dollars
dbgjs source grep 'format.*Credits|credits.*format|AI Credits|AI credits' --regex --path workbench --max-results 25 --context-lines 0 --context :credit-dollars
```

The first returned many accounting references. The second included these
authored-source matches (summarized except for the first complete URL):

```text
https://main.vscode-cdn.net/sourcemaps/046944034292b5479b4e9a50ad1a508033ffb64f/src/vs/workbench/contrib/chat/common/chatService/chatService.ts:223:17:
  export function formatCopilotCredits(credits: number): string {
chat/common/chatService/chatService.ts:232  formatCopilotCreditsLabel
chat/browser/widget/input/modelPicker/modelPickerCard.ts:21
  imports formatModelCost and getCreditsPerMillionTokensLabel from './modelPickerDetails.js'
chat/browser/widget/input/modelPicker/modelPickerHover.ts:21
  imports the same helpers from './modelPickerDetails.js'
```

The returned URLs supplied both the source-map base URL and the `/src/vs/`
filter. The latter excluded huge generated bundle lines. Now an exact symbol
search was justified:

```powershell
dbgjs source grep 'formatCopilotCredits' --path /src/vs/ --max-results 40 --context-lines 0 --context :credit-dollars
```

It confirmed the definitions at lines 223 and 232 and callers in subagent labels,
session cost, and progress details.

### Discover the footer function and model-price helpers

I initially tried `chatListRenderer` as a likely VS Code renderer filename, a
naming hypothesis rather than a name learned from an earlier query. The targeted
search below confirmed the file and connected its footer code to credits. The
later heap class listing in section 3 independently identified the same file:

```powershell
dbgjs source grep 'credits' --path /src/vs/workbench/contrib/chat/browser/widget/chatListRenderer --max-results 15 --context-lines 1 --context :credit-dollars
```

Output included footer-related comments at lines 169, 392, and 532. Reading
around line 540 then exposed the `renderChatResponseDetails` declaration at
line 536, including its parameter order. Thus neither the function name nor its
declaration line was assumed.

The model-picker imports above supplied `modelPickerDetails.ts`. An initial
read at line 150 failed with `source line 150 is outside 1..=54`; reading around
the midpoint, line 27, exposed the entire short file and declarations at lines
30 (`formatModelCost`) and 35 (`getCreditsPerMillionTokensLabel`).
That source also showed the `"Unknown"` fallback for nonnumeric prices and the
`"Credits per 1M tokens"` unit label, which supplied the preservation check and
unit replacement used later.

These are the corresponding read/map commands. `$base` abbreviates the
**discovered** URL above; the actual commands used expanded URLs:

```powershell
$base = 'https://main.vscode-cdn.net/sourcemaps/046944034292b5479b4e9a50ad1a508033ffb64f/src/vs/workbench/contrib/chat'

dbgjs source show "$base/common/chatService/chatService.ts" --line 220 --context-lines 20 --context :credit-dollars
dbgjs source map "$base/common/chatService/chatService.ts" 232 17 --context :credit-dollars
dbgjs source show "$base/browser/widget/chatListRenderer.ts" --line 540 --context-lines 35 --context :credit-dollars
dbgjs source map "$base/browser/widget/chatListRenderer.ts" 536 17 --context :credit-dollars
dbgjs source show "$base/browser/widget/input/modelPicker/modelPickerDetails.ts" --line 150 --context-lines 50 --context :credit-dollars
dbgjs source show "$base/browser/widget/input/modelPicker/modelPickerDetails.ts" --line 27 --context-lines 27 --context :credit-dollars
dbgjs source map "$base/browser/widget/input/modelPicker/modelPickerDetails.ts" 30 17 --context :credit-dollars
dbgjs source map "$base/browser/widget/input/modelPicker/modelPickerDetails.ts" 35 17 --context :credit-dollars
```

The read at line 220 was chosen to include the definition discovered at 223.
An earlier attempt to use a relative `src/vs/...` path failed; the complete URL
from the search results resolved correctly.

### Derive the minified bindings from the mappings

The mapping output supplied the generated bundle URL, not just line numbers.
For the shared label, the first returned mapping was:

```text
process-tree-11756 / renderer-1  authored-to-generated
vscode-file://vscode-app/c:/Users/hdieterichs/AppData/Local/Programs/Microsoft%20VS%20Code%20Insiders/0469440342/resources/app/out/vs/workbench/workbench.desktop.main.js:633:21171
[greatest-lower-bound]
```

The footer mapped to 2834:996; model cost and its unit mapped to 2482:2951 and
2482:3017. Those locations guided bounded excerpts of the minified source:

```powershell
$bundle = 'vscode-file://vscode-app/c:/Users/hdieterichs/AppData/Local/Programs/Microsoft%20VS%20Code%20Insiders/0469440342/resources/app/out/vs/workbench/workbench.desktop.main.js'
$s = dbgjs --json source show $bundle --line 633 --context-lines 0 --context :credit-dollars | ConvertFrom-Json
$s.content.Substring(20950,550)
$s = dbgjs --json source show $bundle --line 2834 --context-lines 0 --context :credit-dollars | ConvertFrom-Json
$s.content.Substring(980,1350)
$s = dbgjs --json source show $bundle --line 2482 --context-lines 0 --context :credit-dollars | ConvertFrom-Json
$s.content.Substring(2940,240)
```

The substring offsets were manually chosen just before the mapped columns to
show the function headers and surrounding code; they were not independent facts
about the program. The returned excerpts contained these declarations:

```text
line 633:  function H4n(s) ... function bY(s) ...
line 2834: function uun(s,o,e,t,i,n) ...
line 2482: function Hsi(s) ... function Bsi() ...
```

This established the runtime names before they were used in a heap query:

| Source function | Runtime binding | Evidence |
| --- | --- | --- |
| `ChatResponseViewModel` | `yY` | Class listing plus instance `1987451` in section 3 |
| `formatCopilotCredits` | `H4n` | Numeric helper adjacent to mapped label function |
| `formatCopilotCreditsLabel` | `bY` | Authored mapping and line-633 excerpt |
| `formatModelCost` | `Hsi` | Authored mapping and line-2482 excerpt |
| `getCreditsPerMillionTokensLabel` | `Bsi` | Authored mapping and line-2482 excerpt |
| `renderChatResponseDetails`, left unchanged | `uun` | Authored mapping and line-2834 excerpt |

### Find those known functions in the heap

```powershell
dbgjs heap select credit-functions --type closure --name-regex '^(bY|H4n|uun)$' --limit 10
dbgjs heap show 'credit-functions#947891' --limit 15
dbgjs heap refs 'credit-functions#947891' --incoming --limit 10
```

Relevant results:

```text
credit-functions#947891  type:closure, value:"bY"
credit-functions#955505  type:closure, value:"uun"
credit-functions#963651  type:closure, value:"H4n"
credit-functions#520123 --context "bY"--> credit-functions#947891
```

That confirmed the label function was retained by the shared module context and
supplied footer heap ID `955505`. I materialized that function and retained it
for later identity/rendering verification:

`HeapProfiler.getObjectByHeapObjectId`, `Runtime.callFunctionOn`, and the later
`Runtime.queryObjects` are standard CDP APIs, not application symbols found in
the source. They respectively resolve a heap ID to a live handle, invoke code
with that object as `this`, and find live objects with a given prototype. Each
subsequent command uses the handle returned by the preceding one.

```powershell
dbgjs target cdp HeapProfiler.getObjectByHeapObjectId --params '{"objectId":"955505","objectGroup":"credit-investigation"}' --context :credit-dollars
dbgjs target cdp Runtime.callFunctionOn --params '{"objectId":"-536608007814549874.1.72","functionDeclaration":"function(){globalThis.__creditFooterFunction=this;return this.name}","returnByValue":true}' --context :credit-dollars
```

Output: a function object with remote ID `-536608007814549874.1.72`, followed
by `"uun"`.

## 5. Patch the shared getter and formatters

First install pure formatting helpers:

```powershell
@'
(() => {
  const usd = new Intl.NumberFormat("en-US", {style:"currency",currency:"USD",minimumFractionDigits:2,maximumFractionDigits:2});
  const format = credits => {
    if (typeof credits !== "number" || !Number.isFinite(credits)) throw new TypeError("Expected finite numeric credits");
    const cents = Math.trunc(credits);
    return usd.format(cents === 0 ? 0 : cents / 100);
  };
  const convert = text => typeof text === "string" ? text.replace(/(?<![\w$.,+-])(-?(?:\d{1,3}(?:,\d{3})+|\d+)(?:\.\d+)?)\s+(?:AI\s+)?credits?\b/gi, (_, amount) => format(Number(amount.replaceAll(",", "")))) : text;
  globalThis.__creditSourcePatch = {format, convert};
  return "Presentation helpers installed; source functions still unchanged";
})()
'@ | dbgjs target eval - --context :credit-dollars --full
```

Output:

```text
Presentation helpers installed; source functions still unchanged
```

An integer credit is one cent, so truncating credits before dividing by 100
implements the requested rule. The text adapter handles existing English credit
labels, comma grouping, singular credits, and already-converted strings.

Ordinary global evaluation reported `typeof bY` and `typeof H4n` as `undefined`.
That conclusion came from this probe, after the generated-source excerpts had
supplied the names:

```powershell
dbgjs target eval 'JSON.stringify({label:typeof bY,number:typeof H4n,loader:typeof require})' --context :credit-dollars
```

```json
{"label":"undefined","number":"undefined","loader":"undefined"}
```

The hypothesis was that the functions were module-scoped rather than global.
A separate pause/probe/resume sequence tested access from a renderer frame:

```powershell
dbgjs target cdp Debugger.pause --context :credit-dollars
dbgjs target show --context :credit-dollars
dbgjs target eval 'JSON.stringify({label:typeof bY,number:typeof H4n})' --context :credit-dollars
dbgjs target resume --context :credit-dollars
```

Relevant output:

```text
Target renderer-1 [paused at epoch 1]
  Source: ../../../src/vs/base/common/event.ts - fromDOMEventEmitter
  #0 fromDOMEventEmitter - ../../../src/vs/base/common/event.ts:727:45
{"label":"function","number":"function"}
Target renderer-1 [running]
  Pause: none
```

That established the access technique before the patch used it. The shared
`yY` class name came independently from the heap instance/class association in
section 3; the patch checks its result getter before replacing it. A different
pause may land outside this module, so these probes must be repeated rather
than assuming every paused frame exposes these bindings.

The complete successful function patch:

```powershell
dbgjs target cdp Debugger.pause --context :credit-dollars
try {
@'
(() => {
  if (globalThis.__creditHeapPatch) throw new Error("Heap patch already installed");
  const proto = yY.prototype;
  const descriptor = Object.getOwnPropertyDescriptor(proto, "result");
  if (!descriptor?.get || typeof bY !== "function" || typeof H4n !== "function") throw new Error("Unexpected formatter or view-model prototype");
  const {format, convert} = globalThis.__creditSourcePatch;
  const original = {number:H4n,label:bY,modelCost:Hsi,modelUnit:Bsi};
  const raw = globalThis.__creditExampleModel.result.details;
  Object.defineProperty(proto, "result", {...descriptor, get() {
    const result = descriptor.get.call(this);
    if (!result || typeof result.details !== "string") return result;
    const details = convert(result.details);
    return details === result.details ? result : {...result, details};
  }});
  H4n = format;
  bY = format;
  Hsi = cost => typeof cost === "number" ? format(cost) : original.modelCost(cost);
  Bsi = () => "USD per 1M tokens";
  globalThis.__creditHeapPatch = {
    proto, format, convert,
    label: value => bY(value),
    modelCost: value => Hsi(value),
    modelUnit: () => Bsi(),
    restore() {
      Object.defineProperty(proto, "result", descriptor);
      H4n=original.number; bY=original.label; Hsi=original.modelCost; Bsi=original.modelUnit;
      delete globalThis.__creditHeapPatch;
      return "Original source functions restored";
    }
  };
  const view = Object.create(proto);
  view._model = globalThis.__creditExampleModel;
  return JSON.stringify({prototype:proto.constructor.name,before:raw,after:view.result.details,stored:globalThis.__creditExampleModel.result.details,sharedLabel:bY(279.6),footerUnchanged:uun===globalThis.__creditFooterFunction});
})()
'@ | dbgjs target eval - --context :credit-dollars --full
} finally {
  dbgjs target resume --context :credit-dollars
}
```

Decoded output:

```json
{
  "prototype": "yY",
  "before": "GPT-6 Astra \u2022 279.6 credits",
  "after": "GPT-6 Astra \u2022 $2.79",
  "stored": "GPT-6 Astra \u2022 279.6 credits",
  "sharedLabel": "$2.79",
  "footerUnchanged": true
}
```

```text
Target renderer-1  [running]
  Pause: none
```

Unlike a DOM patch, this changes the function every response view uses to obtain
its presentation data. A shallow projected result preserves metadata references;
when no conversion is needed, the original result object is returned unchanged.

## 6. Validate all live response views and refresh normally

```powershell
dbgjs target cdp Runtime.evaluate --params '{"expression":"globalThis.__creditHeapPatch.proto","objectGroup":"credit-verify"}' --context :credit-dollars
dbgjs target cdp Runtime.queryObjects --params '{"prototypeObjectId":"-536608007814549874.1.12","objectGroup":"credit-verify"}' --context :credit-dollars
```

Output: prototype remote ID `-536608007814549874.1.12`, then `Array(9)` with
remote ID `-536608007814549874.1.13`.

The historical model's heap properties in section 3 included `_onDidChange`.
I searched its authored class, whose file path came from the heap class listing,
to learn how the application actually uses that emitter:

```powershell
dbgjs source grep '_onDidChange =|_onDidChange.fire' --regex --path /src/vs/workbench/contrib/chat/common/model/chatModel.ts --max-results 8 --context-lines 0 --context :credit-dollars
```

Relevant output, summarized:

```text
chatModel.ts:1225  _onDidChange is an Emitter<ChatResponseModelChangeReason>
chatModel.ts:1285  this._onDidChange.fire(defaultChatResponseModelChangeReason)
chatModel.ts:1514  response changes fire the same default reason
```

This supplied the previously unknown constant name for the next search:

```powershell
dbgjs source grep 'const defaultChatResponseModelChangeReason' --path /src/vs/workbench/contrib/chat/common/model/chatModel.ts --max-results 1 --context-lines 1 --context :credit-dollars
```

Relevant output: `defaultChatResponseModelChangeReason` is `{ reason: 'other' }`.

```powershell
dbgjs target cdp Runtime.callFunctionOn --params '{"objectId":"-536608007814549874.1.13","functionDeclaration":"function(){const samples=this.map(v=>({before:v._model.result?.details,after:v.result?.details})).filter(v=>v.before);this.forEach(v=>v._model._onDidChange.fire({reason:\"other\"}));return {instances:this.length,samples,notified:this.length}}","returnByValue":true}' --context :credit-dollars
```

Decoded output:

```json
{
  "instances": 9,
  "samples": [
    {"before":"GPT-6 Astra \u2022 $3.14","after":"GPT-6 Astra \u2022 $3.14"},
    {"before":"GPT-6 Astra \u2022 66 credits","after":"GPT-6 Astra \u2022 $0.66"},
    {"before":"GPT-6 Astra \u2022 41.1 credits","after":"GPT-6 Astra \u2022 $0.41"},
    {"before":"GPT-6 Astra \u2022 $3.03","after":"GPT-6 Astra \u2022 $3.03"}
  ],
  "notified": 9
}
```

These are actual live response instances, not just formatter test strings.
Already-dollar-denominated details are not converted again.

## 7. Verify boundaries, preservation, and the original renderer

```powershell
@'
(() => {
  const p=globalThis.__creditHeapPatch;
  const cases=[[0,"$0.00"],[0.9,"$0.00"],[1,"$0.01"],[99.99,"$0.99"],[100,"$1.00"],[279.6,"$2.79"],[123456.7,"$1,234.56"],[-0.9,"$0.00"],[-279.6,"-$2.79"]];
  for(const [input,expected] of cases) if(p.label(input)!==expected) throw new Error("Numeric case: "+input);
  const raw={details:"GPT-6 Astra - 279.6 credits",metadata:{tokens:800320}};
  const view=Object.create(p.proto);view._model={result:raw};
  const shown=view.result;
  if(shown.details!=="GPT-6 Astra - $2.79"||raw.details!=="GPT-6 Astra - 279.6 credits"||shown.metadata!==raw.metadata) throw new Error("View projection changed source data");
  for(const result of [undefined,{details:undefined},{details:"GPT-6 Astra"},{details:"GPT-6 Astra - $2.79"}]) {
    view._model={result};if(view.result!==result) throw new Error("Unchanged result identity regression");
  }
  const historical=Object.create(p.proto);historical._model=globalThis.__creditExampleModel;
  const element=document.createElement("div");
  globalThis.__creditFooterFunction(element,historical.result.details,undefined,undefined,false,"Input tokens: 800320");
  if(!element.textContent.endsWith("$2.79")||!element.ariaLabel.includes("$2.79, Input tokens: 800320")) throw new Error("Original footer did not render projected details");
  if(p.modelCost(279.6)!=="$2.79"||p.modelCost(undefined)!=="Unknown"||p.modelUnit()!=="USD per 1M tokens") throw new Error("Model costs regression");
  return JSON.stringify({numericCases:cases.length,unchangedIdentityCases:4,metadataIdentityPreserved:true,historicalRaw:historical._model.result.details,historicalPresented:historical.result.details,unmodifiedFooter:{text:element.textContent,ariaLabel:element.ariaLabel},modelCost:p.modelCost(279.6),unknownCost:p.modelCost(undefined),modelUnit:p.modelUnit()});
})()
'@ | dbgjs target eval - --context :credit-dollars --full
```

Decoded output:

```json
{
  "numericCases": 9,
  "unchangedIdentityCases": 4,
  "metadataIdentityPreserved": true,
  "historicalRaw": "GPT-6 Astra \u2022 279.6 credits",
  "historicalPresented": "GPT-6 Astra \u2022 $2.79",
  "unmodifiedFooter": {
    "text": "GPT-6 Astra \u2022 $2.79",
    "ariaLabel": "GPT-6 Astra \u2022 $2.79, Input tokens: 800320"
  },
  "modelCost": "$2.79",
  "unknownCost": "Unknown",
  "modelUnit": "USD per 1M tokens"
}
```

The test invokes the original footer function on a detached element with the
real historical model's projected details. It does not rewrite a live node or
fabricate a stored response. The currently visible streaming responses had
empty footer text, so the evidence is the live-instance query plus the original
renderer-function test, not a screenshot claim.

## Lifetime and undo

After verification, investigation-only global references and CDP object groups
were released. `__creditLabelFunction` was an extra handle saved during an
earlier formatter identity probe; it is not required by the successful patch.
The deletion is harmless if replaying only the commands above never created it:

```powershell
dbgjs target eval '(() => { delete globalThis.__creditSourcePatch; delete globalThis.__creditExampleModel; delete globalThis.__creditFooterFunction; delete globalThis.__creditLabelFunction; return JSON.stringify({active:!!globalThis.__creditHeapPatch,example:globalThis.__creditHeapPatch.label(279.6),legacyPatch:typeof globalThis.__creditDollars}); })()' --context :credit-dollars
dbgjs target cdp Runtime.releaseObjectGroup --params '{"objectGroup":"credit-investigation"}' --context :credit-dollars
dbgjs target cdp Runtime.releaseObjectGroup --params '{"objectGroup":"credit-verify"}' --context :credit-dollars
dbgjs target show --context :credit-dollars
```

Output: `{"active":true,"example":"$2.79","legacyPatch":"undefined"}`, two
empty CDP results, and `Target renderer-1 [running]`, `Pause: none`.

The active patch is retained as `globalThis.__creditHeapPatch`. Reloading the
window removes it. To restore the original getter and formatters without a
reload, run:

```powershell
dbgjs target eval 'globalThis.__creditHeapPatch.restore()' --context :credit-dollars
```

That undo command was not run after the successful installation. Re-render
existing views afterward to update already-presented labels.

Before replaying on another VS Code build, repeat the heap and source-map
investigation. Do not reuse capture IDs, remote object IDs, or minified bindings
without verifying them.
