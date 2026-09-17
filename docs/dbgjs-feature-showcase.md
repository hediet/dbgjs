# dbgjs Feature Showcase

> Commands and outputs are short excerpts from the repository's demo transcripts,
> CLI walkthrough, and end-to-end tests. Process, target, capture, and object IDs
> are illustrative and change between runs.

- **Process and VS Code window discovery**
  - Lists process trees, windows, renderers, extension hosts, agent hosts, and
    other attachable runtimes.
  - Example:

    ```powershell
    dbgjs process list --vscode --no-cmd-line
    ```

    ```text
    VS Code process tree 11756
    └─ p:11756  Code - Insiders.exe  [vscode-main]
       ├─ w:11756/1  window  ● cdp-client - Untitled-1
       │  ├─ p:3124  renderer  [renderer]
       │  ├─ p:18168  extension-host  [extension-host]
    ```

  - Source: [bracket-pair AST transcript](../demo-transcripts/bracket-pair-ast-size.md#1-discover-and-attach-to-the-correct-renderer)

- **Durable contexts and connection management**
  - Keeps breakpoints, captures, source knowledge, and named connection recipes
    together even while no runtime is connected.
  - Example:

    ```powershell
    dbgjs context create :bracket-ast-size 'Bracket AST size'
    dbgjs process attach w:11756/1 --context :bracket-ast-size --set
    dbgjs connection disconnect --context :bracket-ast-size --connection process-tree-11756
    ```

    ```text
    Attachment: created
    Target renderer-1  [running]  bracket-ast-size/process-tree-11756

    process-tree-11756  [disconnected; generation 1]
      Configuration: process tree rooted at PID 11756
      Targets: none
    ```

  - Source: [bracket-pair AST transcript](../demo-transcripts/bracket-pair-ast-size.md#8-disconnect-without-deleting-the-evidence)

- **Target attachment, evaluation, pause, and resume**
  - Evaluates JavaScript in the selected runtime and exposes authored stack
    locations while paused.
  - Example:

    ```powershell
    dbgjs target cdp Debugger.pause --context :credit-dollars
    dbgjs target show --context :credit-dollars
    dbgjs target eval 'JSON.stringify({label:typeof bY,number:typeof H4n})' --context :credit-dollars
    dbgjs target resume --context :credit-dollars
    ```

    ```text
    Target renderer-1 [paused at epoch 1]
      Source: ../../../src/vs/base/common/event.ts - fromDOMEventEmitter
      #0 fromDOMEventEmitter - ../../../src/vs/base/common/event.ts:727:45
    {"label":"function","number":"function"}
    Target renderer-1 [running]
    ```

  - Source: [AI credit transcript](../demo-transcripts/ai-credit-dollars.md#5-patch-the-shared-getter-and-formatters)

- **Source maps, source viewing, and grep**
  - Searches loaded generated and authored sources, prints focused excerpts, and
    maps positions in either direction.
  - Example:

    ```powershell
    dbgjs source grep 'class CompressedVirtualizedScrollView' --max-results 2 --context-lines 0 --context :graphb-layout
    dbgjs source show <authored-url>/compressedVirtualizedScrollView.ts --line 367 --context-lines 13 --context :graphb-layout
    dbgjs source map <authored-url>/compressedVirtualizedScrollView.ts 372 3 --context :graphb-layout
    ```

    ```text
    compressedVirtualizedScrollView.ts:55:8:
      export class CompressedVirtualizedScrollView<...> extends Disposable

    371 | private _render(...): void {
    372 |     for (let index = 0; index < items.length; index++) {

    authored-to-generated  sessions.desktop.main.js:2529:27377  [exact]
    ```

  - Source: [multi-diff transcript](../demo-transcripts/multi-diff-disposed-models.md#3-find-source-and-capture-the-live-scroll-view)

- **Breakpoints, logpoints, watches, and stepping**
  - Resolves authored locations through source maps, reports pending bindings
    explicitly, and returns the new pause directly when execution stops.
  - Example:

    ```text
    > dbgjs -c frontend breakpoint set src/components/CheckoutButton.tsx:28
    Created bp-1 [verified on browser/page-1]

    > dbgjs -c frontend cdp Runtime.evaluate --params \
        '{"expression":"setTimeout(() => document.querySelector(\"button[data-testid=checkout]\").click(), 0)"}'
    Paused: breakpoint bp-1
    At: src/components/CheckoutButton.tsx:28:28

      27 | export function CheckoutButton({ cart }: Props) {
    > 28 |   const onCheckout = () => submitCheckout(cart);
         |                            ^^^^^^^^^^^^^^^^^^^^^
    ```

  - Source: [CLI website-debugging walkthrough](./cli-design.md#33-debug-a-website-button-that-does-nothing)

- **Playwright page automation**
  - Runs a bounded Playwright program against one already-selected page without
    granting access to unrelated pages.
  - Example:

    ```powershell
    dbgjs playwright 'return { title: await page.title(), scrollY: await page.evaluate(() => scrollY) };' --context :playwright-e2e --connection browser --target <page>
    ```

    ```json
    {"title":"dbgjs Playwright E2E","scrollY":800}
    ```

  - Source: [Playwright page E2E test](../tests/playwright/playwright-page-cli.spec.mjs)

- **Screenshots**
  - Captures a full page or selected element from the attached target.
  - Example:

    ```powershell
    dbgjs screenshot capture --output graphb-before.png --context :graphb-layout --target renderer-6
    ```

    ```text
    Captured 2880x1848 screenshot to ...\graphb-before.png.
    ```

  - Source: [multi-diff transcript](../demo-transcripts/multi-diff-disposed-models.md#2-inspect-the-ui-and-measure-the-blank-space)

- **Heap snapshots, search, references, and dominators**
  - Freezes an immutable heap capture, searches classes and strings, follows
    incoming or outgoing references, measures retained size, and can reconnect a
    still-live snapshot object to JavaScript.
  - Find classes by authored name and list their instances:

    ```powershell
    dbgjs heap classes credit-functions --filter '^(ChatResponseModel|ChatResponseViewModel|ChatListItemRenderer)$' --instances --max-lines 60
    ```

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
    ```

  - Resolve heap object `3415327` back to a live remote object, then use it as
    `this` in a JavaScript expression:

    ```powershell
    dbgjs target cdp HeapProfiler.getObjectByHeapObjectId --params '{"objectId":"3415327","objectGroup":"credit-investigation"}' --context :credit-dollars
    dbgjs target cdp Runtime.callFunctionOn --params '{"objectId":"-536608007814549874.1.70","functionDeclaration":"function(){globalThis.__creditExampleModel=this;return {details:this.result?.details,rawDetails:this._result?.details}}","returnByValue":true}' --context :credit-dollars
    ```

    ```json
    {"result":{"type":"object","className":"zHe","objectId":"-536608007814549874.1.70"}}
    {
      "details": "GPT-6 Astra \u2022 279.6 credits",
      "rawDetails": "GPT-6 Astra \u2022 279.6 credits"
    }
    ```

  - Source: [AI credit transcript](../demo-transcripts/ai-credit-dollars.md#3-find-the-shared-presentation-prototype)

- **Precise code coverage**
  - Starts recording before execution, stores immutable captures, subtracts a
    baseline, and renders source-mapped function or block coverage.
  - Complete real typing workflow:

    ```console
    $ dbgjs coverage start
    Coverage recording started.

    $ dbgjs coverage capture --id background
    Captured background.

    $ dbgjs target key enter
    Pressed enter

    $ dbgjs target key arrowup
    Pressed arrowup

    $ dbgjs coverage stop --exclude background
    Coverage recording stopped. Captured .

    $ dbgjs coverage show . --path src/vs/editor/common/model
    14549 RL (run lines), 4106 HL (hit lines)
    Analysis 0.0s; source-map cache: 0 hit, 0 miss, 0 bypass
    └─ src/vs/editor/common/  [29 files, 4106 HL, 14549 RL]
       ├─ model/  [28 files, 4084 HL, 14527 RL]
       │  ├─ bracketPairsTextModelPart/  [12 files, 1240 HL, 5432 RL]
       │  │  ├─ bracketPairsTree/  [11 files, 1035 HL, 4689 RL]
       │  │  │  ├─ bracketPairsTree.ts  296 HL, 1777 RL [collectBrackets 128 HL, 1024 RL, collectBracketPairs 107 HL, 428 RL]
       │  │  │  │  ├─ collectBrackets  128 HL, 1024 RL
       │  │  │  │  ├─ collectBracketPairs  107 HL, 428 RL
       │  │  │  │  ├─ BracketPairsTree  55 HL, 301 RL
       │  │  │  │  │  ├─ flushQueue  12 HL, 144 RL
       │  │  │  ├─ tokenizer.ts  171 HL, 606 RL
       │  │  │  │  ├─ NonPeekableTextBufferTokenizer  120 HL, 472 RL
       │  │  │  │  │  ├─ read  116 HL, 464 RL
       │  │  │  │  │  └─ constructor  4 HL, 8 RL
       │  │  │  ├─ parser.ts  134 HL, 318 RL [parseDocument 4 HL, 8 RL]
       │  │  │  │  ├─ Parser  130 HL, 310 RL
       │  │  │  │  │  ├─ parseChild  48 HL, 96 RL
       │  │  │  ├─ nodeReader.ts  108 HL, 362 RL [getNextChildIdx 11 HL, 22 RL, lastOrUndefined 3 HL, 30 RL]
       │  │  │  │  ├─ NodeReader  94 HL, 310 RL
       │  │  │  │  │  ├─ readLongestNodeAt  61 HL, 244 RL
       │  │  │  ├─ beforeEditPositionMapper.ts  94 HL, 446 RL
       │  │  │  │  ├─ BeforeEditPositionMapper  64 HL, 404 RL
       │  │  │  │  │  ├─ adjustNextEdit  29 HL, 232 RL
       │  │  │  ├─ combineTextEditInfos.ts  74 HL, 148 RL [combineTextEditInfos 74 HL, 148 RL]
       │  │  │  │  └─ combineTextEditInfos  74 HL, 148 RL
       │  │  │  ├─ length.ts  72 HL, 744 RL
       │  │  │  │  ├─ lengthDiffNonNegative  25 HL, 50 RL
       │  │  │  │  ├─ toLength  10 HL, 450 RL
       │  │  │  ├─ concat23Trees.ts  52 HL, 104 RL [concat23Trees 52 HL, 104 RL]
       │  │  │  │  └─ concat23Trees  52 HL, 104 RL
       │  │  │  ├─ brackets.ts  16 HL, 64 RL  [children pruned]
       │  │  │  ├─ ast.ts  15 HL, 108 RL  [children pruned]
       │  │  │  └─ smallImmutableSet.ts  3 HL, 12 RL  [children pruned]
       │  │  └─ bracketPairsImpl.ts  205 HL, 743 RL
       │  │     └─ BracketPairsTextModelPart  205 HL, 743 RL
       │  │        ├─ findEnclosingBrackets  135 HL, 270 RL
       │  │        ├─ matchBracket  29 HL, 58 RL
       │  │        ├─ updateBracketPairsTree  24 HL, 288 RL
       │  │        ├─ getBracketPairsInRange  5 HL, 20 RL
       │  │        ├─ getBracketsInRange  5 HL, 40 RL
       │  │        ├─ canBuildAST  4 HL, 64 RL
       │  │        └─ handleDidChangeContent  3 HL, 3 RL
       │  ├─ pieceTreeTextBuffer/  [3 files, 950 HL, 4282 RL]
       │  │  ├─ pieceTreeBase.ts  612 HL, 3524 RL [createLineStartsFast 26 HL, 26 RL]
       │  │  │  ├─ PieceTreeBase  532 HL, 3051 RL
       │  │  │  │  ├─ insert  97 HL, 97 RL
       │  │  │  │  ├─ delete  72 HL, 72 RL
       │  │  │  │  ├─ getLineRawContent  71 HL, 2414 RL
       │  │  │  │  ├─ positionInBuffer  47 HL, 188 RL
       │  │  │  │  ├─ findMatchesInNode  40 HL, 43 RL
       │  │  │  │  ├─ appendToNode  39 HL, 39 RL
       │  │  │  │  ├─ nodeAt  34 HL, 34 RL
       │  │  │  │  ├─ adjustCarriageReturnFromNext  31 HL, 31 RL
       │  │  │  │  ├─ getLineFeedCnt  29 HL, 58 RL
       │  │  │  │  ├─ startWithLF  23 HL, 23 RL
       │  │  │  │  ├─ findMatchesLineByLine  19 HL, 19 RL
       │  │  │  │  ├─ computeBufferMetadata  16 HL, 16 RL
       │  │  │  │  ├─ endWithCR  11 HL, 11 RL
       │  │  │  │  └─ shouldCheckCRLF  3 HL, 6 RL
       │  │  │  ├─ PieceTreeSearchCache  47 HL, 440 RL
       │  │  │  │  ├─ validate  23 HL, 23 RL
       │  │  │  │  ├─ get  9 HL, 9 RL
       │  │  │  │  ├─ get2  9 HL, 306 RL
       │  │  │  │  └─ set  6 HL, 102 RL
       │  │  ├─ pieceTreeTextBuffer.ts  306 HL, 390 RL
       │  │  │  └─ PieceTreeTextBuffer  306 HL, 390 RL
       │  │  │     ├─ applyEdits  172 HL, 172 RL
       │  │  │     ├─ _getInverseEditRanges  52 HL, 52 RL
       │  │  │     ├─ _doApplyEdits  40 HL, 40 RL
       │  │  │     ├─ getCharacterCountInRange  30 HL, 60 RL
       │  │  │     ├─ findMatchesLineByLine  3 HL, 3 RL
       │  │  │     ├─ mightContainNonBasicASCII  3 HL, 30 RL
       │  │  │     ├─ mightContainRTL  3 HL, 30 RL
       │  │  │     └─ mightContainUnusualLineTerminators  3 HL, 3 RL
       │  │  └─ rbTreeBase.ts  32 HL, 368 RL [updateTreeMetadata 11 HL, 11 RL]
       │  │     ├─ TreeNode  21 HL, 357 RL
       │  │     └─ updateTreeMetadata  11 HL, 11 RL
       │  ├─ textModel.ts  654 HL, 1255 RL [indentOfLine 11 HL, 22 RL, _normalizeOptions 6 HL, 18 RL]
       │  │  ├─ TextModel  527 HL, 1004 RL
       │  │  │  ├─ _deltaDecorationsImpl  103 HL, 309 RL
       │  │  │  ├─ _doApplyEdits  100 HL, 100 RL
       │  │  │  ├─ _pushEditOperations  89 HL, 89 RL
       │  │  │  ├─ findMatches  37 HL, 37 RL
       │  │  │  ├─ _validateEditOperation  32 HL, 64 RL
       │  │  │  ├─ _emitContentChangedEvent  16 HL, 16 RL
       │  │  │  ├─ _onDidChangeContentOrInjectedText  16 HL, 16 RL
       │  │  │  ├─ getValue  15 HL, 15 RL
       │  │  │  ├─ applyEdits  12 HL, 12 RL
       │  │  │  ├─ handleBeforeFireDecorationsChangedEvent  11 HL, 33 RL
       │  │  │  ├─ pushEditOperations  10 HL, 10 RL
       │  │  │  ├─ getLineLength  8 HL, 64 RL
       │  │  │  ├─ _fireOnDidChangeFont  7 HL, 21 RL
       │  │  │  ├─ _fireOnDidChangeLineHeight  7 HL, 21 RL
       │  │  │  ├─ _validateEditOperations  7 HL, 14 RL
       │  │  │  ├─ getLineInjectedText  7 HL, 14 RL
       │  │  │  ├─ getCustomLineHeightsDecorationsInRange  5 HL, 5 RL
       │  │  │  ├─ _increaseVersionId  4 HL, 4 RL
       │  │  │  ├─ getAlternativeVersionId  4 HL, 12 RL
       │  │  │  ├─ getCharacterCountInRange  4 HL, 8 RL
       │  │  │  ├─ getLineIndentColumn  4 HL, 8 RL
       │  │  │  ├─ findMatchesLineByLine  3 HL, 3 RL
       │  │  │  ├─ getAllMarginDecorations  3 HL, 3 RL
       │  │  │  ├─ getLanguageIdAtPosition  3 HL, 6 RL
       │  │  │  ├─ getWordAtPosition  3 HL, 24 RL
       │  │  │  ├─ isValidRange  3 HL, 9 RL
       │  │  │  ├─ mightContainNonBasicASCII  3 HL, 30 RL
       │  │  │  ├─ mightContainRTL  3 HL, 30 RL
       │  │  │  ├─ mightContainUnusualLineTerminators  3 HL, 3 RL
       │  │  │  ├─ pushStackElement  3 HL, 6 RL
       │  │  │  ├─ bracketPairs  1 HL, 12 RL
       │  │  │  └─ guides  1 HL, 6 RL
       │  │  ├─ DidChangeDecorationsEmitter  35 HL, 86 RL
       │  │  │  ├─ doFire  15 HL, 45 RL
       │  │  │  ├─ checkAffectedAndFire  7 HL, 14 RL
       │  │  │  ├─ tryFire  7 HL, 21 RL
       │  ├─ guidesTextModelPart.ts  361 HL, 1292 RL
       │  │  └─ GuidesTextModelPart  361 HL, 1292 RL
       │  │     ├─ getActiveIndentGuide  232 HL, 696 RL
       │  │     ├─ getLinesIndentGuides  90 HL, 270 RL
       │  │     ├─ _getIndentLevelForWhitespaceLine  26 HL, 182 RL
       │  │     ├─ getLanguageConfiguration  7 HL, 42 RL
       │  │     └─ _computeIndentLevel  6 HL, 102 RL
       │  ├─ tokens/  [5 files, 333 HL, 1043 RL]
       │  │  ├─ tokenizationTextModelPart.ts  110 HL, 661 RL
       │  │  │  └─ TokenizationTextModelPart  110 HL, 661 RL
       │  │  │     ├─ getWordAtPosition  50 HL, 400 RL
       │  │  │     ├─ _findLanguageBoundaries  21 HL, 168 RL
       │  │  │     ├─ handleDidChangeContent  19 HL, 19 RL
       │  │  │     ├─ getLanguageIdAtPosition  5 HL, 10 RL
       │  │  ├─ annotations.ts  91 HL, 91 RL
       │  │  │  └─ AnnotatedString  91 HL, 91 RL
       │  │  ├─ tokenizerSyntaxTokenBackend.ts  68 HL, 212 RL
       │  │  │  └─ TokenizerSyntaxTokenBackend  68 HL, 212 RL
       │  │  │     ├─ refreshRange  23 HL, 23 RL
       │  │  │     ├─ handleDidChangeContent  19 HL, 19 RL
       │  │  │     ├─ setTokens  9 HL, 81 RL
       │  │  ├─ abstractSyntaxTokenBackend.ts  44 HL, 59 RL
       │  │  │  ├─ AttachedViewHandler  17 HL, 17 RL
       │  │  │  │  ├─ handleStateChange  9 HL, 9 RL
       │  │  │  │  ├─ update  7 HL, 7 RL
       │  │  └─ tokenizationFontDecorationsProvider.ts  20 HL, 20 RL
       │  │     └─ TokenizationFontDecorationProvider  20 HL, 20 RL
       │  ├─ intervalTree.ts  267 HL, 741 RL
       │  │  ├─ searchForEditing  71 HL, 213 RL
       │  │  ├─ noOverlapReplace  69 HL, 207 RL
       │  │  ├─ search  63 HL, 126 RL
       │  │  ├─ IntervalNode  30 HL, 90 RL
       │  │  ├─ IntervalTree  28 HL, 84 RL
       │  │  ├─ getNodeIsInGlyphMargin  3 HL, 6 RL
       │  │  └─ recomputeMaxEnd  3 HL, 15 RL
       │  ├─ editStack.ts  155 HL, 265 RL
       │  │  ├─ SingleModelEditStackData  73 HL, 146 RL
       │  │  ├─ EditStack  37 HL, 43 RL
       │  │  ├─ SingleModelEditStackElement  31 HL, 42 RL
       │  │  ├─ getModelEOL  8 HL, 16 RL
       │  │  └─ pG  6 HL, 18 RL  [generated workbench.web.main.internal.js:678:7]
       │  ├─ textModelSearch.ts  97 HL, 113 RL
       │  │  ├─ SearchParams  34 HL, 34 RL
       │  │  │  ├─ parseSearchRequest  28 HL, 28 RL
       │  │  │  └─ constructor  6 HL, 6 RL
       │  │  ├─ Searcher  33 HL, 49 RL
       │  │  │  ├─ next  22 HL, 38 RL
       │  │  │  ├─ constructor  6 HL, 6 RL
       │  │  │  └─ reset  5 HL, 5 RL
       │  │  ├─ rightIsWordBounday  12 HL, 12 RL
       │  │  ├─ leftIsWordBounday  8 HL, 8 RL
       │  │  ├─ isValidMatch  6 HL, 6 RL
       │  │  └─ createFindMatch  4 HL, 4 RL
       │  ├─ prefixSumComputer.ts  16 HL, 16 RL
       │  │  └─ ConstantTimePrefixSumComputer  16 HL, 16 RL
       │  │     ├─ setValue  8 HL, 8 RL
       │  │     ├─ _invalidate  4 HL, 4 RL
       │  │     └─ insertValues  4 HL, 4 RL
       │  ├─ textModelStringEdit.ts  6 HL, 18 RL [offsetEditFromContentChanges 6 HL, 18 RL]
       │  │  └─ offsetEditFromContentChanges  6 HL, 18 RL
       │  └─ textModelPart.ts  5 HL, 70 RL
       │     └─ TextModelPart  5 HL, 70 RL
       └─ model.ts  22 HL, 22 RL
          ├─ ValidAnnotatedEditOperation  8 HL, 8 RL
          ├─ ApplyEditsResult  5 HL, 5 RL
          ├─ SearchData  5 HL, 5 RL
          └─ FindMatch  4 HL, 4 RL
    ```

  - Source: generated by the [real vscode.dev typing coverage E2E test](../tests/playwright/vscode-coverage-cli.spec.mjs)

- **Performance trace / CPU profiling**
  - Records a real target-scoped V8 sampling profile and projects generated
    frames back to authored TypeScript.
  - Real editor-creation and typing profile, trimmed to the hottest entries:

    ```console
    $ dbgjs profile start --sampling-interval 1ms
    CPU profile recording started (1000us sampling interval).

    $ dbgjs target key ctrl+k,n
    Pressed ctrl+k,n

    $ dbgjs target type "<27781 characters>"

    $ dbgjs profile stop --id editor-input
    CPU profile recording stopped. Captured editor-input.

    $ dbgjs profile show editor-input --view functions --sort self --max-lines 60
    ```

    ```text
    CPU profile editor-input: 23.90s elapsed, 23.90s sampled, 16629 samples
    Self       Total      Samples  Function
    5.73s      5.98s         3915  ViewModelEventDispatcher._addOutgoingEvent  ../../../src/vs/editor/common/viewModelEventDispatcher.ts:42:10
    3.64s      9.55s         2517  AutoClosedAction.getAllAutoClosedCharacters  ../../../src/vs/editor/common/cursor/cursor.ts:676:16
    2.86s      2.86s         2014  (garbage collector)  :0:0
    2.49s      4.03s         1726  PieceTreeBase.getPositionAt  ../../../src/vs/editor/common/model/pieceTreeTextBuffer/pieceTreeBase.ts:418:9
    1.40s      1.41s          979  IntervalTree.resolveNode  ../../../src/vs/editor/common/model/intervalTree.ts:291:9
    1.05s      1.17s          728  PieceTreeBase.getOffsetAt  ../../../src/vs/editor/common/model/pieceTreeTextBuffer/pieceTreeBase.ts:395:9
    716.37ms   1.48s          507  ViewModelEventDispatcher._emitOutgoingEvents  ../../../src/vs/editor/common/viewModelEventDispatcher.ts:54:10
    518.40ms   518.40ms       366  ProducerConsumer.consume  ../../../src/vs/base/common/async.ts:2447:2
    370.93ms   370.93ms       187  (program)  :0:0
    362.77ms   362.77ms       252  PieceTreeBase.positionInBuffer  ../../../src/vs/editor/common/model/pieceTreeTextBuffer/pieceTreeBase.ts:1075:10
    322.62ms   737.20ms       223  PieceTreeBase.getValueInRange  ../../../src/vs/editor/common/model/pieceTreeTextBuffer/pieceTreeBase.ts:459:9
    189.26ms   5.91s          131  AutoClosedAction.getAutoClosedCharactersRanges  ../../../src/vs/editor/common/cursor/cursor.ts:700:9
    129.28ms   129.28ms        91  WebWorker.postMessage → callback  ../../../src/vs/platform/webWorker/browser/webWorkerServiceImpl.ts:181:21
    ```

  - Source: generated by the [real vscode.dev CPU-profile E2E test](../tests/playwright/vscode-profile-cli.spec.mjs)

- **Immutable capture catalog and offline analysis**
  - Stores coverage, CPU profiles, and heap snapshots outside the live target;
    named captures remain queryable after disconnect or service restart.
  - Example:

    ```powershell
    dbgjs --json capture show bracket-ast --context :bracket-ast-size
    ```

    ```json
    {
      "name": "bracket-ast",
      "kind": "heapSnapshot",
      "targetId": "renderer-1",
      "connectionGeneration": 1,
      "storageId": "21429294b1af0ef17689cf3239353098"
    }
    ```

  - Source: [bracket-pair AST transcript](../demo-transcripts/bracket-pair-ast-size.md#2-capture-an-immutable-heap-snapshot)

- **Raw CDP escape hatch**
  - Sends any CDP method through the managed target session when a high-level
    command is not yet available.
  - Example:

    ```powershell
    dbgjs target cdp HeapProfiler.getObjectByHeapObjectId --params '{"objectId":"3415327","objectGroup":"credit-investigation"}' --context :credit-dollars
    ```

    ```json
    {"result":{"type":"object","className":"zHe","objectId":"-536608007814549874.1.70"}}
    ```

  - Source: [AI credit transcript](../demo-transcripts/ai-credit-dollars.md#3-find-the-shared-presentation-prototype)
