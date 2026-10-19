# Kotlin Language Server — Feature Catalog

A full inventory of what a Kotlin language server should provide, grouped by LSP
capability. Each item is a checkbox so this file can double as a roadmap.

Priority tags: **[P0]** must-have for a usable server · **[P1]** expected by most
users · **[P2]** nice-to-have / IntelliJ parity.

Status: `[x]` implemented · `[~]` partially implemented · `[ ]` not started.

---

## 1. Core Infrastructure

### 1.1 Protocol & lifecycle
- [x] [P0] `initialize` / `initialized` / `shutdown` / `exit`
- [x] [P0] Capability negotiation (client caps → server caps)
- [x] [P0] Text document sync: `didOpen`, `didChange` (incremental), `didClose`, `didSave`
- [ ] [P1] `willSave` / `willSaveWaitUntil` (format/organize-imports on save)
- [x] [P0] Cancellation (`$/cancelRequest`) honored by all long-running requests
- [x] [P1] Progress reporting (`window/workDoneProgress`, `$/progress`) for indexing/builds
- [~] [P1] `window/showMessage`, `window/logMessage`, `window/showDocument` — *partial: showMessage + logMessage; showDocument not yet*
- [x] [P1] `workspace/configuration` + `workspace/didChangeConfiguration`
- [x] [P1] `workspace/didChangeWatchedFiles` (build files, generated sources, jars)
- [x] [P1] `workspace/didChangeWorkspaceFolders` (multi-root)
- [ ] [P2] `workspace/didCreateFiles` / `didRenameFiles` / `didDeleteFiles` (+ `will*` variants that return edits)
- [x] [P2] Position encoding negotiation (UTF-16 default, UTF-8 if supported)
- [ ] [P2] `$/setTrace`, `$/logTrace`

### 1.2 Parsing & syntax
- [~] [P0] Error-tolerant incremental parser for `.kt` and `.kts` — *partial: error tolerant; whole-file reparse per edit (~20 MB/s)*
- [x] [P0] Lossless CST (trivia, comments, KDoc preserved)
- [x] [P0] String templates (`$x`, `${expr}`), raw strings, multi-dollar interpolation (`$$"..."`)
- [x] [P0] Annotations with use-site targets (`@field:`, `@get:`, `@file:` …)
- [x] [P1] Context parameters / context receivers syntax
- [x] [P1] Guard conditions in `when` (`is X if cond ->`)
- [x] [P1] Non-local `break`/`continue`, underscore type args, other recent language features
- [ ] [P2] Kotlin version–aware feature gating (`-language-version`, `-api-version`)

### 1.3 Semantic analysis
- [x] [P0] Symbol table: packages, imports (incl. aliases, star imports, default imports)
- [~] [P0] Name resolution with Kotlin scope rules (locals, members, extensions, implicit receivers, companion, `this@label`) — *partial: imports and type references (type params, local/nested/inherited classifiers, imports, default imports); expressions not yet*
- [ ] [P0] Type inference (locals, lambdas, generic calls, builder inference, smart casts)
- [ ] [P0] Overload resolution (incl. extension vs member priority, SAM conversion, default/named args, varargs)
- [ ] [P1] Flow analysis: smart casts, definite assignment, reachability, `Nothing` propagation, contracts
- [~] [P1] Nullability: platform types, `@Nullable/@NotNull` (JSR-305, JSpecify, AndroidX) — *partial: modelled for library signatures; not yet checked*
- [~] [P1] Generics: variance (declaration & use-site), star projections, reified, type bounds, definitely-non-null `T & Any` — *partial: represented in symbols; not yet checked*
- [ ] [P1] Delegation (`by lazy`, `by Delegates.observable`, class delegation, `provideDelegate`)
- [ ] [P1] Coroutines: `suspend` checking, suspend lambdas, `restrictsSuspension`
- [ ] [P1] Sealed / enum exhaustiveness
- [ ] [P1] Inline / value classes, `@JvmInline`
- [ ] [P2] Kotlin compiler plugins semantics: `kotlinx.serialization`, `all-open`, `no-arg`, `kapt`/KSP-generated symbols, Compose, Parcelize, Power-assert

### 1.4 Project model & indexing
- [x] [P0] Gradle (Kotlin DSL + Groovy) project import: source sets, dependencies, JVM target, compiler options
- [x] [P1] Maven project import
- [~] [P1] Standalone files / scripts without a build system — *partial: syntax checks only*
- [~] [P1] Kotlin Multiplatform: `commonMain`/`jvmMain`/`iosMain`…, `expect`/`actual` resolution, per-target diagnostics — *partial: source sets + JVM-target dependency resolution; no per-target analysis*
- [~] [P1] Android projects (AGP variants, R class, generated sources) — *partial: android.jar + AAR classes; variants only via Gradle import*
- [x] [P0] Classpath indexing: read `.class` files / Kotlin metadata from jars
- [ ] [P1] `.klib` reading (Native/JS/Wasm/common)
- [~] [P1] Java source interop: parse & resolve `.java` files in the same module — *partial: declarations are indexed and visible to Kotlin*
- [ ] [P1] Source jar / attached sources lookup
- [~] [P1] Persistent on-disk index cache, invalidation on dependency change — *partial: resolved project model is cached on disk; indexes are in memory*
- [x] [P1] Background, cancellable, incremental re-analysis
- [ ] [P2] Bazel / Amper / custom build-server (BSP) support
- [ ] [P2] `.kts` script definitions (Gradle scripts, `*.main.kts` with `@file:DependsOn`)

---

## 2. Navigation

- [~] [P0] `textDocument/definition` — incl. into library sources/decompiled stubs, Java sources, `expect`↔`actual` — *partial: sources (local or downloaded `-sources.jar`) or generated stubs; no `expect`↔`actual` jumps*
- [ ] [P1] `textDocument/declaration`
- [ ] [P1] `textDocument/typeDefinition`
- [ ] [P1] `textDocument/implementation` — overrides, subclasses, `actual` declarations, SAM implementations
- [~] [P0] `textDocument/references` — incl. operator conventions (`plus`, `get`, `invoke`, `component1`), property accessors, delegated properties, KDoc links, Java usages — *partial: name references in Kotlin sources (incl. imports, types, constructor calls, the whole override family incl. `super` calls and anonymous objects, and operator-convention call sites: arithmetic/compound assignment, unary, `++`/`--`, ranges, `in`/`!in`, indexing, `==`, comparisons, `invoke`, destructuring `componentN`, `for` iterators, `by` delegates); not KDoc links or Java files*
- [ ] [P1] `textDocument/documentHighlight` — read vs write occurrences, exit points (`return`), loop `break` targets
- [x] [P0] `textDocument/documentSymbol` — hierarchical outline (classes, objects, functions, properties, typealiases, enum entries)
- [~] [P0] `workspace/symbol` — fuzzy/camel-hump search across project + libraries — *partial: project sources only*
- [ ] [P2] `workspaceSymbol/resolve`
- [ ] [P1] Call hierarchy: `prepareCallHierarchy`, `incomingCalls`, `outgoingCalls`
- [ ] [P1] Type hierarchy: `prepareTypeHierarchy`, `supertypes`, `subtypes`
- [ ] [P1] `textDocument/documentLink` — URLs in comments, `@see`, file paths in strings
- [ ] [P2] Go to test / go to tested subject (custom command)
- [ ] [P2] Go to super method (custom command or via `declaration`)
- [ ] [P2] Decompile/show Kotlin bytecode & Java equivalent (custom command)

---

## 3. Information Display

### 3.1 Hover
- [x] [P0] Declaration signature with resolved types and modifiers
- [x] [P0] Rendered KDoc (Markdown, `@param`, `@return`, `@throws`, `@sample`, `[links]`)
- [x] [P1] Javadoc rendering for Java symbols
- [~] [P1] Inferred type of expressions / smart-cast type — *partial: locals, `it`, generic substitution; no smart casts*
- [ ] [P1] Constant values (`const val`, literal folding)
- [ ] [P2] Deprecation info with replacement
- [x] [P2] Source library / module of the symbol

### 3.2 Signature help
- [ ] [P0] `textDocument/signatureHelp` — active parameter, overloads, named arguments, trailing lambda awareness, vararg
- [ ] [P1] Generic type parameters shown in instantiated form

### 3.3 Inlay hints (`textDocument/inlayHint`, `inlayHint/resolve`)
- [ ] [P1] Local variable / property inferred types
- [ ] [P1] Parameter names at call sites (configurable: literals only, all, off)
- [ ] [P1] Lambda implicit `it` / `this` receiver type and return value hints
- [ ] [P1] Chained call (fluent) intermediate types
- [ ] [P2] Function return type hints for expression bodies
- [ ] [P2] Ranges (`0 ≤ i < 10` for `0 until 10`)
- [ ] [P2] Suspend call markers, implicit receiver hints, value-parameter hints for `Function` types

### 3.4 Semantic tokens (`textDocument/semanticTokens/full`, `/delta`, `/range`)
- [ ] [P0] Token types: namespace, class, interface, enum, enumMember, typeParameter, function, method, property, parameter, variable, keyword, modifier, annotation, string, number, comment, operator, label
- [ ] [P1] Modifiers: declaration, readonly (`val`), mutable (`var`), static/companion, deprecated, abstract, suspend, extension, backing field, smart-cast, documentation
- [ ] [P1] Soft keywords highlighted contextually (`data`, `value`, `sealed`, `by`, `get`, `set`…)
- [ ] [P2] String template injections, regex/SQL/JSON language injection highlighting

### 3.5 Other display
- [ ] [P1] `textDocument/codeLens` — run/debug `main` & tests, references count, implementations count, overrides
- [ ] [P1] `textDocument/foldingRange` — imports, blocks, KDoc, multiline comments, `when` branches, region comments
- [ ] [P1] `textDocument/selectionRange` — expand/shrink selection syntactically
- [ ] [P2] `textDocument/inlineValue` (debugger)
- [ ] [P2] `textDocument/documentColor` / `colorPresentation` (Compose `Color(...)`, Android colors)
- [ ] [P2] `textDocument/linkedEditingRange` (e.g. label `foo@` ↔ `@foo`)
- [ ] [P2] `textDocument/moniker`

---

## 4. Completion (`textDocument/completion`, `completionItem/resolve`)

- [ ] [P0] Scope members: locals, parameters, members, top-levels, imported symbols
- [ ] [P0] Member completion after `.` and `?.` (incl. extensions applicable to receiver type)
- [ ] [P0] Keyword completion (context-aware: `override`, `return@label`, `when` branches…)
- [ ] [P0] Auto-import: unimported classes, top-level functions, extensions (adds `import` via `additionalTextEdits`)
- [ ] [P1] Smart/type-aware ranking (expected type first, recently used, same module)
- [ ] [P1] Named argument completion (`param =`)
- [ ] [P1] Override/implement member completion (generates full signature + body)
- [ ] [P1] `when` exhaustive branch completion (sealed/enum)
- [ ] [P1] Snippets / postfix templates: `.if`, `.not`, `.for`, `.val`, `.var`, `.return`, `.let`, `.notnull`, `.null`, `.try`, `.par`, `.sout`, `.when`, `.arg`, `.spread`
- [ ] [P1] Live templates: `main`, `fun0/1/2`, `sout`, `iter`, `ifn`, `inn`, `void`, `closure`, `object`, `interface`, `exfun`, `exval`
- [ ] [P1] Lambda parameter completion (`{ a, b -> }` from expected function type)
- [ ] [P1] Completion in string templates (`$` / `${`)
- [ ] [P1] Completion in KDoc (`[Symbol]`, tags)
- [ ] [P1] Annotation completion (incl. use-site targets)
- [ ] [P1] Package / import statement completion
- [ ] [P2] Static member completion through companion without qualifier, enum entry completion without class prefix
- [ ] [P2] Callable references (`::fn`, `Type::member`)
- [ ] [P2] Variable name suggestions from type (`val user: User` → `user`)
- [ ] [P2] Chained completion (`users.first().` → suggests via expected-type chains)
- [ ] [P2] Gradle Kotlin DSL completion (dependencies, plugin IDs, task names)
- [ ] [P2] Commit characters, `insertReplace` ranges, label details, deprecation strike-through
- [ ] [P2] Snippet tab-stops for function arguments

---

## 5. Diagnostics

Delivery: [P0] `textDocument/publishDiagnostics` (push) **and/or** [P1] `textDocument/diagnostic` + `workspace/diagnostic` (pull). *(implemented: push, and document pull)* Include `code`, `codeDescription`, `relatedInformation`, `tags` (`Unnecessary`, `Deprecated`).

### 5.1 Compiler errors (subset of the K2 compiler diagnostic set)
- [x] [P0] Syntax errors (expecting X, unexpected token, unclosed string/comment)
- [~] [P0] `UNRESOLVED_REFERENCE`, `UNRESOLVED_IMPORT` — *partial: imports and type references*
- [ ] [P0] `TYPE_MISMATCH`, `ARGUMENT_TYPE_MISMATCH`, `RETURN_TYPE_MISMATCH`, `INITIALIZER_TYPE_MISMATCH`
- [ ] [P0] `NONE_APPLICABLE`, `OVERLOAD_RESOLUTION_AMBIGUITY`, `TOO_MANY_ARGUMENTS`, `NO_VALUE_FOR_PARAMETER`, `NAMED_PARAMETER_NOT_FOUND`
- [ ] [P0] Null safety: `UNSAFE_CALL`, `UNSAFE_OPERATOR_CALL`, `UNSAFE_IMPLICIT_INVOKE_CALL`, `NULL_FOR_NONNULL_TYPE`
- [ ] [P0] `VAL_REASSIGNMENT`, `UNINITIALIZED_VARIABLE`, `MUST_BE_INITIALIZED`, `MUST_BE_INITIALIZED_OR_BE_ABSTRACT`
- [ ] [P0] `NO_RETURN_IN_FUNCTION_WITH_BLOCK_BODY`, `RETURN_NOT_ALLOWED`
- [ ] [P0] `ABSTRACT_MEMBER_NOT_IMPLEMENTED`, `ABSTRACT_CLASS_MEMBER_NOT_IMPLEMENTED`, `MANY_IMPL_MEMBER_NOT_IMPLEMENTED`
- [ ] [P0] `NOTHING_TO_OVERRIDE`, `VIRTUAL_MEMBER_HIDDEN` (missing `override`), `OVERRIDING_FINAL_MEMBER`, `FINAL_SUPERTYPE`
- [ ] [P0] Visibility: `INVISIBLE_REFERENCE`, `INVISIBLE_MEMBER`, `EXPOSED_*` (exposed visibility)
- [ ] [P0] `NO_ELSE_IN_WHEN` / `WHEN_ON_SEALED_*` non-exhaustive
- [ ] [P0] `ILLEGAL_SUSPEND_FUNCTION_CALL`, `NON_LOCAL_SUSPENSION_POINT`
- [~] [P0] `CONFLICTING_OVERLOADS`, `REDECLARATION`, `PACKAGE_OR_CLASSIFIER_REDECLARATION` — *partial: classifier redeclarations*
- [ ] [P1] Generics: `UPPER_BOUND_VIOLATED`, `WRONG_NUMBER_OF_TYPE_ARGUMENTS`, `TYPE_VARIANCE_CONFLICT`, `CANNOT_INFER_PARAMETER_TYPE`, `NEW_INFERENCE_NO_INFORMATION_FOR_PARAMETER`
- [ ] [P1] Modifiers: `INCOMPATIBLE_MODIFIERS`, `REDUNDANT_MODIFIER`, `WRONG_MODIFIER_TARGET`, `REPEATED_MODIFIER`
- [ ] [P1] Annotations: `WRONG_ANNOTATION_TARGET`, `ANNOTATION_ARGUMENT_MUST_BE_CONST`, opt-in (`OPT_IN_USAGE`, `OPT_IN_USAGE_ERROR`)
- [ ] [P1] Data/value/sealed/enum class rule violations (`DATA_CLASS_WITHOUT_PARAMETERS`, `VALUE_CLASS_*`, `SEALED_SUPERTYPE*`)
- [ ] [P1] `expect`/`actual`: `NO_ACTUAL_FOR_EXPECT`, `ACTUAL_WITHOUT_EXPECT`, `ACTUAL_MISSING`, incompatible signatures
- [ ] [P1] `CONST_VAL_*`, `LATEINIT_*`, `DELEGATE_SPECIAL_FUNCTION_*` (missing `getValue`/`setValue`)
- [ ] [P1] `OPERATOR_MODIFIER_REQUIRED`, `INFIX_MODIFIER_REQUIRED`
- [ ] [P1] `SUPERTYPE_NOT_INITIALIZED`, `SUPERTYPE_INITIALIZED_IN_INTERFACE`, `CYCLIC_INHERITANCE_HIERARCHY`
- [ ] [P1] `DEPRECATION_ERROR`, `API_NOT_AVAILABLE` (api-version)
- [ ] [P1] Platform-specific: `JVM_*` clashes (`CONFLICTING_JVM_DECLARATIONS`, `ACCIDENTAL_OVERRIDE`), JS/Native target errors
- [ ] [P2] Compiler plugin diagnostics (serialization: `SERIALIZER_NOT_FOUND`, Compose: `COMPOSABLE_INVOCATION`…)

### 5.2 Compiler warnings
- [ ] [P0] `UNUSED_VARIABLE`, `UNUSED_PARAMETER`, `UNUSED_EXPRESSION`, `UNUSED_VALUE`, `ASSIGNED_BUT_NEVER_ACCESSED_VARIABLE` (tag `Unnecessary`)
- [ ] [P0] `UNNECESSARY_SAFE_CALL`, `UNNECESSARY_NOT_NULL_ASSERTION`, `USELESS_ELVIS`, `SENSELESS_COMPARISON`
- [ ] [P0] `DEPRECATION` (tag `Deprecated`)
- [ ] [P1] `USELESS_CAST`, `USELESS_IS_CHECK`, `CAST_NEVER_SUCCEEDS`, `UNCHECKED_CAST`
- [ ] [P1] `UNREACHABLE_CODE`, `REDUNDANT_ELSE_IN_WHEN`, `DUPLICATE_LABEL_IN_WHEN`
- [ ] [P1] `NAME_SHADOWING`, `VARIABLE_WITH_REDUNDANT_INITIALIZER`
- [ ] [P1] `PLATFORM_CLASS_MAPPED_TO_KOTLIN`, `EXTENSION_SHADOWED_BY_MEMBER`
- [ ] [P1] `NOTHING_TO_INLINE`, `REDUNDANT_PROJECTION`, `FINAL_UPPER_BOUND`
- [ ] [P2] `-Werror` / `-Xsuppress-warning` / `@Suppress` awareness

### 5.3 Inspections (IntelliJ-style, beyond compiler; each configurable severity + `@Suppress("Id")`)

**Redundancy**
- [ ] [P1] `RedundantSemicolon`
- [ ] [P1] `RemoveRedundantQualifierName`
- [ ] [P1] `RedundantExplicitType` / `RedundantUnitReturnType`
- [ ] [P1] `RedundantVisibilityModifier` (`public` default)
- [ ] [P1] `RedundantModalityModifier` (`final` default, `open` in interface)
- [ ] [P1] `RedundantUnitExpression`
- [ ] [P1] `RedundantIf` (`if (x) true else false`)
- [ ] [P1] `RedundantLambdaArrow`, `RedundantLambdaOrAnonymousFunction`
- [ ] [P1] `RedundantSuspendModifier`
- [ ] [P1] `RemoveRedundantBackticks`
- [ ] [P1] `RemoveEmptyParenthesesFromLambdaCall`, `RemoveEmptyClassBody`, `RemoveEmptyPrimaryConstructor`, `RemoveEmptySecondaryConstructorBody`
- [ ] [P1] `RemoveCurlyBracesFromTemplate` (`${x}` → `$x`), `RemoveSingleExpressionStringTemplate`
- [ ] [P1] `RedundantCompanionReference`, `RedundantObjectTypeCheck`
- [ ] [P1] `RedundantGetter` / `RedundantSetter`
- [ ] [P1] `RedundantInnerClassModifier`, `RedundantEmptyInitializerBlock`
- [ ] [P1] `RedundantNullableReturnType`, `RedundantRequireNotNullCall`
- [ ] [P1] `RedundantWith`, `RedundantAsSequence`, `RedundantSamConstructor`
- [ ] [P1] `RedundantElvisReturnNull`, `RedundantRunCatching`
- [ ] [P1] `RedundantConstructorKeyword`, `RedundantEnumConstructorInvocation`
- [ ] [P1] `RedundantLabelMigration`, `RedundantValueArgument` (arg equals default)
- [ ] [P1] `RedundantExplicitTypeArguments` (`listOf<Int>(1)`)
- [ ] [P1] `UnusedImport`, `UnusedSymbol` (unused private/internal declarations), `UnusedReceiverParameter`, `UnusedLambdaExpressionBody`, `UnusedUnaryOperator`
- [ ] [P2] `RedundantOverride` (only calls super), `RedundantInterpolationPrefix`

**Idiomatic Kotlin / style**
- [ ] [P1] `CanBeVal` (`var` never reassigned)
- [ ] [P1] `CanBeParameter` (constructor `val` used only in init)
- [ ] [P1] `CanBePrimaryConstructorProperty`
- [ ] [P1] `ConvertToStringTemplate` (string concatenation)
- [ ] [P1] `LiftReturnOrAssignment` (out of `if`/`when`/`try`)
- [ ] [P1] `CascadeIf` → `when`, `IntroduceWhenSubject`
- [ ] [P1] `ReplaceGetOrSet` (`a.get(i)` → `a[i]`), `ReplaceCallWithBinaryOperator` (`a.plus(b)` → `a + b`)
- [ ] [P1] `ReplaceRangeToWithUntil` / `ReplaceUntilWithRangeUntil` (`..<`)
- [ ] [P1] `ReplaceSizeCheckWithIsNotEmpty`, `ReplaceSizeZeroCheckWithIsEmpty`, `ReplaceIsEmptyWithIfEmpty`
- [ ] [P1] `UseExpressionBody` (single `return` block body)
- [ ] [P1] `IfThenToSafeAccess`, `IfThenToElvis`, `FoldInitializerAndIfToElvis`
- [ ] [P1] `ReplaceNegatedIsEmptyWithIsNotEmpty`, `SimplifyNegatedBinaryExpression`, `SimplifyBooleanWithConstants`
- [ ] [P1] `ConvertTwoComparisonsToRangeCheck`, `ConvertNaNEquality`
- [ ] [P1] `ReplaceJavaStaticMethodWithKotlinAnalog` (`Math.max`, `System.out.println`, `Arrays.asList`…)
- [ ] [P1] `ReplaceToStringWithStringTemplate`, `ReplaceSubstringWithTake/Drop/SubstringBefore/After`
- [ ] [P1] `ReplaceManualRangeWithIndicesCalls` (`0 until list.size` → `list.indices`)
- [ ] [P1] `ReplaceWithOperatorAssignment` (`x = x + 1` → `x += 1`)
- [ ] [P1] `ReplaceArrayEqualityOpWithArraysEquals`, `ReplaceArrayOfWithLiteral` (annotations)
- [ ] [P1] `ReplacePutWithAssignment` (`map.put(k, v)` → `map[k] = v`)
- [ ] [P1] `ReplaceAssociateFunction`, `ReplaceMapIndexedWithListGenerator`
- [ ] [P1] `JoinDeclarationAndAssignment`, `MoveVariableDeclarationIntoWhen`
- [ ] [P1] `MayBeConstant` (`val` → `const val`)
- [ ] [P1] `ObjectLiteralToLambda`, `ConvertLambdaToReference`, `ConvertReferenceToLambda`
- [ ] [P1] `RemoveExplicitTypeArguments`, `RemoveToStringInStringTemplate`
- [ ] [P1] `SelfAssignment`, `SuspiciousEqualsCombination`, `SuspiciousCallableReferenceInLambda`
- [ ] [P1] `ReplaceNotNullAssertionWithElvisReturn`, `ReplaceGuardClauseWithFunctionCall` (`require`/`check`)
- [ ] [P1] `ReplaceWithEnumMap`, `ReplaceWithIgnoreCaseEquals`
- [ ] [P1] `ReplaceStringFormatWithLiteral`, `ReplaceReadLineWithReadln`, `ReplacePrintlnWithLogging`
- [ ] [P1] `ScopeFunctionConversion` (`let` ↔ `also` ↔ `run` ↔ `apply`), `ComplexRedundantLet`, `SimpleRedundantLet`
- [ ] [P1] `UnnecessaryVariable`, `UnnecessaryOptInAnnotation`
- [ ] [P1] `KotlinConstantConditions` (always true/false)
- [ ] [P1] `ConvertSecondaryConstructorToPrimary`, `ConvertTryFinallyToUseCall`
- [ ] [P1] `ControlFlowWithEmptyBody`, `EmptyRange`
- [ ] [P2] `DestructuringWrongName`, `UnusedDataClassCopyResult`
- [ ] [P2] `ReplaceCollectionCountWithSize`, `PlatformExtensionReceiverOfInline`

**Collections & sequences**
- [ ] [P1] `SimplifiableCallChain` (`filter{}.first()` → `first{}`, `map{}.filterNotNull()` → `mapNotNull{}`, `sortedBy{}.first()` → `minBy{}`…)
- [ ] [P1] `SimplifiableCall` (`filter { it is T }` → `filterIsInstance<T>()`)
- [ ] [P1] `ConvertCallChainIntoSequence` (long chains on large collections)
- [ ] [P1] `UselessCallOnCollection` / `UselessCallOnNotNull` (`orEmpty()` on non-null, `filterNotNull()` on non-null elements)
- [ ] [P1] `ReplaceManualForEachWithMap` / loop-to-stdlib suggestions (`for` loop → `map/filter/sum/any/all/count`)
- [ ] [P1] `ReplaceWithCount`, `RedundantAsSequence`
- [ ] [P2] `JavaCollectionsStaticMethod`, `JavaMapForEach`

**Naming & formatting conventions**
- [ ] [P1] `ClassName`, `FunctionName`, `PropertyName`, `ConstPropertyName`, `PrivatePropertyName`, `ObjectPropertyName`, `LocalVariableName`, `EnumEntryName`, `PackageName`, `TestFunctionName`
- [ ] [P1] `PackageDirectoryMismatch`
- [ ] [P1] `MayBeConstant`, `TrailingComma` (missing/redundant per code style)
- [ ] [P2] `KDocUnresolvedReference`, `KDocMissingDocumentation` (public API)
- [ ] [P2] `FileNameMatchesClass` (single top-level class)

**Probable bugs**
- [ ] [P1] `EqualsOrHashCode` (one without the other)
- [ ] [P1] `RecursivePropertyAccessor` / `RecursiveEqualsCall`
- [ ] [P1] `ArrayInDataClass` (needs custom `equals`/`hashCode`)
- [ ] [P1] `LeakingThis` (calling open member / passing `this` in constructor)
- [ ] [P1] `SuspiciousVarProperty`, `SuspiciousCollectionReassignment` (`+=` on `var` read-only list)
- [ ] [P1] `ImplicitNullableNothingType`, `ImplicitThis` (configurable)
- [ ] [P1] `DelegationToVarProperty`
- [ ] [P1] `WrapUnaryOperator` (`-1.inc()` precedence)
- [ ] [P1] `NestedLambdaShadowedImplicitParameter` (nested `it`)
- [ ] [P1] `ReplaceArrayEqualityOpWithArraysEquals`, `CopyWithoutNamedArguments`
- [ ] [P1] `UnusedResultOfAsync` / `DeferredResultUnused`, `SuspendFunctionOnCoroutineScope`, `RedundantRunBlocking`
- [ ] [P1] `RunBlockingInSuspendFunction`, `BlockingMethodInNonBlockingContext` (`Thread.sleep` in coroutine)
- [ ] [P1] `FlowCollectWithoutLaunch`, `GlobalCoroutineUsage` (`GlobalScope`)
- [ ] [P1] `WarningOnMainUnusedParameterMigration`, `MainFunctionReturnUnit`
- [ ] [P1] `OverridingDeprecatedMember`, `DeprecatedCallableAddReplaceWith`
- [ ] [P1] `ConvertNaNEquality` (`x == Double.NaN`)
- [ ] [P1] `ThrowableNotThrown` (`IllegalStateException()` without `throw`)
- [ ] [P1] `KotlinThrowsInspection` / `@Throws` mismatch for Java callers
- [ ] [P1] `PublicApiImplicitType` (explicit API mode: `-Xexplicit-api`)
- [ ] [P2] `SuspiciousCascadingIf`, `ProtectedInFinal`, `NonExhaustiveWhenStatementMigration`
- [ ] [P2] `IncompleteDestructuring`, `UnsafeCastFromDynamic` (JS)
- [ ] [P2] `SerializationAnnotation`/`kotlinx.serialization` misuse, Compose-specific rules (modifier param, remember, stable)

**Java interop**
- [ ] [P1] `UsePropertyAccessSyntax` (`getFoo()` → `foo`)
- [ ] [P1] `PlatformExtensionReceiverOfInline`, `HasPlatformType` (public API exposes platform type)
- [ ] [P1] `JavaIoSerializableObjectMustHaveReadResolve`
- [ ] [P2] `FunctionWithLambdaExpressionBody`, `JvmStaticWithoutCompanion`

**Build scripts**
- [ ] [P2] Gradle `.kts`: deprecated configurations (`compile` → `implementation`), dependency version catalog suggestions, newer library version available

---

## 6. Code Actions (`textDocument/codeAction`, `codeAction/resolve`)

Code action kinds: `quickfix`, `refactor`, `refactor.extract`, `refactor.inline`, `refactor.rewrite`, `source`, `source.organizeImports`, `source.fixAll`.

### 6.1 Quick fixes (bound to diagnostics)
- [ ] [P0] **Import** unresolved symbol (with chooser when ambiguous; class, function, extension, operator, delegate accessor)
- [ ] [P0] **Add `?.`** / **add `!!`** / **surround with null check** / **add elvis** for unsafe calls
- [ ] [P0] **Change type** of variable/parameter/return to match
- [ ] [P0] **Add explicit cast** (`as T`) / **change to safe cast** (`as?`)
- [ ] [P0] **Implement members** / **make class abstract**
- [ ] [P0] **Add `override` modifier**, **remove `override`**, **make super member `open`**
- [ ] [P0] **Add missing `when` branches** / **add `else` branch**
- [ ] [P0] **Remove unused** variable/parameter/import/declaration
- [ ] [P0] **Change `val` to `var`**
- [ ] [P0] **Initialize property** / **make `lateinit`** / **make `abstract`** / **move to constructor**
- [ ] [P0] **Add `suspend` modifier** to containing function / wrap in `runBlocking`/`launch`
- [ ] [P0] **Create from usage**: function, extension function, property, local variable, parameter, class, interface, enum, object, enum entry, constructor, type parameter, annotation, `component`/operator functions
- [ ] [P0] **Add missing return** / **add `return` with expression**
- [ ] [P1] **Add/remove argument**, **add named argument**, **change function signature** to match call, **add parameter to function**
- [ ] [P1] **Add type arguments**, **specify type explicitly**
- [ ] [P1] **Make visible** (change visibility of referenced declaration), **make `public`/`internal`/`private`/`protected`**
- [ ] [P1] **Add `open`** to class/member, **remove `final`**
- [ ] [P1] **Add `operator`/`infix`/`inline`/`reified`/`crossinline`/`noinline`/`tailrec`/`data`/`const` modifier**
- [ ] [P1] **Remove incompatible / redundant modifier**
- [ ] [P1] **Opt-in**: add `@OptIn(X::class)` to statement/function/class/file, or propagate `@X` marker, or add compiler arg
- [ ] [P1] **Replace deprecated usage** (`@Deprecated(replaceWith = ReplaceWith(...))`) — single and whole project
- [ ] [P1] **Suppress warning** for statement/function/class/file (`@Suppress("...")`)
- [ ] [P1] **Add `@Throws`**, **add `@JvmStatic`/`@JvmOverloads`/`@JvmField`/`@JvmName`**
- [ ] [P1] **Create `actual`** declarations for `expect` (per target), **create `expect`** for `actual`
- [ ] [P1] **Convert to block body** to allow multiple statements
- [ ] [P1] **Wrap with `listOf/arrayOf/setOf`**, **add `.toList()/.toInt()/.toString()`** conversions for type mismatch
- [ ] [P1] **Add `else` / `Unit`** for `if` used as expression
- [ ] [P1] **Rename to match expected** (e.g. parameter name in override, `PackageDirectoryMismatch` → move file / change package)
- [ ] [P1] **Add `const`**, **remove `const`**
- [ ] [P1] **Add `init` block / initializer**, **add constructor parameters / supertype constructor call**
- [ ] [P1] **Add missing `getValue`/`setValue`/`provideDelegate`** for delegates
- [ ] [P1] **Add Gradle/Maven dependency** for unresolved well-known library symbol
- [ ] [P1] **Apply inspection fix** for every inspection in §5.3 (one-to-one)
- [ ] [P2] **Fix all of this type in file / project** (`source.fixAll` + per-kind batch)
- [ ] [P2] **Upgrade language/API version** / **enable language feature** in build file

### 6.2 Source actions
- [ ] [P0] `source.organizeImports` — remove unused, sort per code style, collapse to `*` per threshold, handle aliases
- [ ] [P1] `source.fixAll` — apply all safe fixes
- [ ] [P1] **Generate**: `equals()`/`hashCode()`, `toString()`, secondary constructor, KDoc stub, test class/method, `serialVersionUID`
- [ ] [P1] **Implement members…** / **Override members…** (with selection)
- [ ] [P1] **Add missing imports** for whole file (on paste / on save)
- [ ] [P2] **Optimize imports on the fly**, **convert star imports to explicit**

### 6.3 Intentions / refactor.rewrite (cursor-context actions)
**Declarations**
- [ ] [P1] Convert to expression body ↔ block body
- [ ] [P1] Specify type explicitly ↔ remove explicit type
- [ ] [P1] Convert property to function ↔ function to property
- [ ] [P1] Convert property initializer to getter ↔ getter to initializer
- [ ] [P1] Convert parameter to property (`val`/`var` in constructor) ↔ property to parameter
- [ ] [P1] Move property to constructor ↔ move to class body
- [ ] [P1] Convert to secondary constructor ↔ convert to primary constructor
- [ ] [P1] Convert class to `data`/`sealed`/`value`/`enum`/`object`, `sealed class` ↔ `sealed interface`
- [ ] [P1] Convert `object` to `class`, `companion object` extraction
- [ ] [P1] Add/remove `@JvmStatic`, `@JvmOverloads`
- [ ] [P1] Convert member to extension ↔ extension to member
- [ ] [P1] Convert receiver to parameter ↔ parameter to receiver
- [ ] [P1] Convert function type parameter to receiver (`(T) -> R` ↔ `T.() -> R`)
- [ ] [P1] Make `private`/`internal`/`public`/`protected`
- [ ] [P1] Add/remove explicit visibility (explicit API mode)
- [ ] [P1] Convert `lazy` property ↔ eager, introduce backing property
- [ ] [P1] Convert to `typealias`, inline `typealias`
- [ ] [P2] Convert to `fun interface`, SAM ↔ lambda
- [ ] [P2] Add KDoc / generate `@param` tags

**Expressions & control flow**
- [ ] [P1] `if` ↔ `when`; `when` with subject ↔ without subject; flatten / merge `when` branches
- [ ] [P1] `if` → elvis / safe access, elvis → `if`, `!!` → `if` check / elvis
- [ ] [P1] Invert `if` condition; split `if` (`&&`/`||`) ↔ merge nested `if`s
- [ ] [P1] Add/remove braces
- [ ] [P1] Introduce / remove `when` subject; add `else` / remaining branches
- [ ] [P1] `for` loop ↔ `forEach`; loop → collection operations (`map/filter/sumOf…`); `forEach` → `for`
- [ ] [P1] `for (i in 0 until n)` ↔ `repeat(n)`, `indices`, `withIndex()`
- [ ] [P1] `try/finally` → `use {}`
- [ ] [P1] Lambda ↔ anonymous function ↔ callable reference
- [ ] [P1] Move lambda argument out of / into parentheses
- [ ] [P1] Add/remove explicit lambda parameter (`it` ↔ named), specify explicit lambda signature
- [ ] [P1] Scope functions: convert between `let`/`run`/`also`/`apply`/`with`
- [ ] [P1] Add/remove named arguments (single / all)
- [ ] [P1] Add/remove trailing comma, put arguments/parameters on separate lines ↔ one line
- [ ] [P1] Swap binary expression operands, flip `equals`, De Morgan's law
- [ ] [P1] Replace `a.compareTo(b) < 0` ↔ `a < b`, `contains` ↔ `in`, `invoke()` ↔ `()`
- [ ] [P1] Infix call ↔ dot call
- [ ] [P1] Range: `..` ↔ `until`/`..<` ↔ `downTo`, `rangeTo` ↔ operator
- [ ] [P1] Destructure variable / introduce destructuring declaration from `it.first/it.second` or data class
- [ ] [P1] `!is` / `is` simplify; `as` → `as?` with elvis
- [ ] [P1] Add `return@label`, add labeled `this`
- [ ] [P1] Join declaration and assignment ↔ split declaration and assignment
- [ ] [P1] Add/remove `const`, `inline`, `suspend`
- [ ] [P2] Convert assignment to `apply`/`also` chain; introduce `require`/`check`
- [ ] [P2] `listOf().map {}` → `List(n) {}` builder; `buildList`/`buildString` conversions

**Strings**
- [ ] [P1] Concatenation ↔ string template
- [ ] [P1] Convert to raw string (`"""…"""`) ↔ escaped string; add `trimIndent()`/`trimMargin()`
- [ ] [P1] Split / join string literals; convert to `buildString {}`
- [ ] [P1] Add/remove curly braces in template, add/remove interpolation prefix (`$$`)
- [ ] [P2] Convert to `String.format` ↔ template; inject language (regex/JSON/SQL)

**Literals & numbers**
- [ ] [P2] Add/remove underscores in numeric literals; convert hex ↔ decimal ↔ binary; add `L`/`u`/`f` suffix

**Imports & names**
- [ ] [P1] Add import for qualified name / remove qualifier (shorten reference); import all members (`import X.*`)
- [ ] [P1] Introduce import alias / remove alias
- [ ] [P2] Add/remove backticks; rename file to match class

---

## 7. Refactorings (`refactor`, `refactor.extract`, `refactor.inline`, plus rename)

### 7.1 Rename (`textDocument/prepareRename`, `textDocument/rename`)
- [ ] [P0] Locals, parameters, properties, functions, classes, type params, labels
- [ ] [P0] Cross-file and cross-module (incl. Java usages, `@JvmName` aware)
- [ ] [P1] Rename overriding/overridden members together; rename `expect` + all `actual`s
- [ ] [P1] Rename class → rename file; rename package → move directory (`workspace/willRenameFiles`)
- [ ] [P1] Rename named-argument usages and `component`/`getX` Java accessors
- [ ] [P1] Rename in KDoc references, string literals/comments (opt-in), `@Suppress` IDs
- [ ] [P1] Conflict detection (shadowing, redeclaration, visibility)
- [ ] [P2] Rename backing field / accessor pair, rename test along with subject

### 7.2 Extract (`refactor.extract`)
- [ ] [P0] Extract variable (introduce local `val`; with "replace all occurrences")
- [ ] [P0] Extract function (local, member, top-level, extension; infers params/return, handles multiple outputs via data class/pair)
- [ ] [P1] Extract property (member / top-level), extract constant (`const val`)
- [ ] [P1] Extract parameter (introduce parameter, with default value), extract lambda parameter (functional parameter)
- [ ] [P1] Extract interface / superclass
- [ ] [P1] Extract `typealias`
- [ ] [P2] Extract class from members (move fields + methods), extract sealed class hierarchy
- [ ] [P2] Introduce parameter object

### 7.3 Inline (`refactor.inline`)
- [ ] [P0] Inline local variable
- [ ] [P1] Inline function (single call / all calls), inline property, inline `typealias`, inline constant
- [ ] [P1] Inline super class / interface
- [ ] [P2] Inline lambda / scope function (`x.let { it.foo() }` → `x.foo()`)

### 7.4 Move & structural
- [ ] [P1] Move top-level declarations to another file / package (update imports)
- [ ] [P1] Move member to companion ↔ companion to class; move nested class to top level ↔ top level into class
- [ ] [P1] Change signature (reorder, add, remove, rename params; change return type; update all call sites & overrides; add/remove receiver)
- [ ] [P1] Pull members up / push members down
- [ ] [P1] Safe delete (with usages check)
- [ ] [P2] Convert Java file to Kotlin (J2K)
- [ ] [P2] Convert anonymous object to named class / lambda
- [ ] [P2] Encapsulate field (introduce getter/setter or backing property)
- [ ] [P2] Copy class / file

---

## 8. Formatting

- [ ] [P0] `textDocument/formatting` — full-document format per Kotlin coding conventions
- [ ] [P1] `textDocument/rangeFormatting` (+ `rangesFormatting`)
- [ ] [P1] `textDocument/onTypeFormatting` — auto-indent on `\n`, `}`, `)`; KDoc `*` continuation
- [ ] [P1] Respect `.editorconfig` (`ktlint_*`, `ij_kotlin_*` keys: indent, max line length, trailing commas, import layout, wildcard thresholds)
- [ ] [P1] Code style presets: Kotlin official, ktlint (standard / Android), Android Studio
- [ ] [P2] Pluggable external formatters: ktfmt (Google/Kotlinlang/Meta styles), ktlint
- [ ] [P2] Line wrapping of call chains, parameters, arguments, binary expressions; blank-line rules; import ordering

---

## 9. Execution, Testing & Tooling Integration

- [~] [P1] `workspace/executeCommand` commands: run main, run test, build module, reload project, clear caches, show bytecode, decompile — *partial: `kotlin.reloadProject`, `kotlin.projectModel`*
- [ ] [P1] Test discovery: JUnit 4/5, kotlin.test, Kotest specs, Spek, TestNG (for code lenses / test explorer)
- [ ] [P1] Debug adapter integration hooks (launch config generation; DAP is separate)
- [ ] [P2] Linter integration: detekt rules surfaced as diagnostics with fixes, ktlint as diagnostics
- [ ] [P2] Gradle task listing / running from editor
- [ ] [P2] Dependency vulnerability / newer version diagnostics in build files
- [ ] [P2] Coverage overlay (`textDocument/…` via custom extension)
- [ ] [P2] REPL / scratch file evaluation (`*.kts` worksheets)
- [ ] [P2] Notebook support (`notebookDocument/*`) for Kotlin Jupyter

---

## 10. Editor-assist Extras

- [ ] [P1] Paste handling: auto-import on paste, convert pasted Java to Kotlin (custom request)
- [ ] [P1] Auto-closing: generate KDoc skeleton on `/**`, close string templates
- [ ] [P1] Smart enter: complete statement (add braces, parens) — custom command
- [ ] [P2] Join lines (custom) — merges declaration + assignment, string literals
- [ ] [P2] Surround with: `if`, `if/else`, `try/catch`, `try/finally`, `when`, `run {}`, `with {}`, parentheses, string template, `!(…)`
- [ ] [P2] Unwrap / remove: `if`, `else`, `try`, `catch`, lambda, braces
- [ ] [P2] Move statement/declaration up/down (structure-aware)
- [ ] [P2] Spell checking in identifiers/comments/strings (opt-in)
- [ ] [P2] TODO / FIXME collection (`workspace/symbol` or custom)
- [ ] [P2] Show recursive call / suspend call / smart cast gutter markers (via code lens or inlay hints)

---

## 11. Non-functional Requirements

- [ ] [P0] Responsiveness: completion < 100 ms p95, diagnostics on keystroke < 300 ms for open files
- [ ] [P0] Never crash on malformed input; isolate panics per request
- [ ] [P1] Bounded memory with LRU eviction of analysis results; lazy library indexing
- [ ] [P1] Parallel analysis across files/modules; work-stealing scheduler with priority for the active file
- [ ] [P1] Deterministic results independent of request order (salsa-style incremental queries)
- [ ] [P1] Configurable: per-inspection severity, inspection profiles (shared `.editorconfig`/project file), feature toggles
- [ ] [P1] Telemetry-free by default; structured logging with levels
- [ ] [P2] Test corpus: golden tests vs. `kotlinc` diagnostics, fuzzing the parser, LSP conformance tests
