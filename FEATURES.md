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
- [~] [P2] `workspace/didCreateFiles` / `didRenameFiles` / `didDeleteFiles` (+ `will*` variants that return edits) — *partial: `willRenameFiles` for `.kt` files and folders: `package` follows the directory, imports of the moved declarations are updated or added, and the moved file imports what it used from its old package*
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
- [~] [P0] Type inference (locals, lambdas, generic calls, builder inference, smart casts) — *partial: locals, lambda parameters and receivers from the expected function type, type arguments bound from arguments and receivers, smart casts (see flow analysis); no builder inference*
- [~] [P0] Overload resolution (incl. extension vs member priority, SAM conversion, default/named args, varargs) — *partial: arity, then argument types (overloads an argument of known class type cannot be passed to are dropped, explicit type arguments must fit the bounds, and the most specific remaining overload wins: subtype parameters, non-generic before generic, non-vararg before vararg); `@LowPriorityInOverloadResolution` library functions lose to other candidates (and to same-named class constructors); `@Deprecated(level = HIDDEN)` library declarations are not candidates*
- [~] [P1] Flow analysis: smart casts, definite assignment, reachability, `Nothing` propagation, contracts — *partial: smart casts of `val`s, parameters and never-reassigned `var`s from `is`/`!is`/`== null`/`!= null` in `if`/`else`, `&&`/`||`, `when` (with and without subject, earlier entries negated), early-exit guards (`if (…) return/throw/break/continue/error()`), `require`/`check`/`assert`, `requireNotNull`/`checkNotNull`*
- [~] [P1] Nullability: platform types, `@Nullable/@NotNull` (JSR-305, JSpecify, AndroidX) — *partial: modelled for library signatures; not yet checked*
- [~] [P1] Generics: variance (declaration & use-site), star projections, reified, type bounds, definitely-non-null `T & Any` — *partial: represented in symbols; not yet checked*
- [ ] [P1] Delegation (`by lazy`, `by Delegates.observable`, class delegation, `provideDelegate`)
- [ ] [P1] Coroutines: `suspend` checking, suspend lambdas, `restrictsSuspension`
- [x] [P1] Sealed / enum exhaustiveness
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

- [~] [P0] `textDocument/definition` — incl. into library sources/decompiled stubs, Java sources, `expect`↔`actual` — *partial: sources (local or downloaded `-sources.jar`) or generated stubs, KDoc `[links]` and `@param`/`@throws`/`@see` subjects; `expect`↔`actual` via declaration/implementation*
- [x] [P1] `textDocument/declaration` — the member an `override` overrides (also in libraries), the `expect` of an `actual`, otherwise the definition
- [x] [P1] `textDocument/typeDefinition`
- [~] [P1] `textDocument/implementation` — overrides, subclasses, `actual` declarations, SAM implementations — *partial: project subclasses (transitive), overriding members and the `actual`s of an `expect`; not anonymous objects or SAM lambdas*
- [~] [P0] `textDocument/references` — (KDoc `[links]` included) incl. operator conventions (`plus`, `get`, `invoke`, `component1`), property accessors, delegated properties, KDoc links, Java usages — *partial: name references in Kotlin sources (incl. imports, types, constructor calls, the whole override family incl. `super` calls and anonymous objects, and operator-convention call sites: arithmetic/compound assignment, unary, `++`/`--`, ranges, `in`/`!in`, indexing, `==`, comparisons, `invoke`, destructuring `componentN`, `for` iterators, `by` delegates); not KDoc links or Java files*
- [~] [P1] `textDocument/documentHighlight` — read vs write occurrences, exit points (`return`), loop `break` targets — *partial: read/write occurrences (incl. the override family); exit points on `fun`/`return`/`throw` (non-local returns from lambdas included, not `return@label`); loops and their `break`/`continue` (labels followed) on loop keywords and jumps*
- [x] [P0] `textDocument/documentSymbol` — hierarchical outline (classes, objects, functions, properties, typealiases, enum entries)
- [~] [P0] `workspace/symbol` — fuzzy/camel-hump search across project + libraries — *partial: project declarations, and library classes of every module's classpath (the best 20, located in sources jars or generated stubs); not library members*
- [ ] [P2] `workspaceSymbol/resolve`
- [x] [P1] Call hierarchy: `prepareCallHierarchy`, `incomingCalls`, `outgoingCalls` (functions and constructors; callers are functions, constructors, properties or classes)
- [x] [P1] Type hierarchy: `prepareTypeHierarchy`, `supertypes`, `subtypes` (subtypes among project sources)
- [~] [P1] `textDocument/documentLink` — URLs in comments, `@see`, file paths in strings — *partial: URLs in comments, KDoc and strings*
- [ ] [P2] Go to test / go to tested subject (custom command)
- [ ] [P2] Go to super method (custom command or via `declaration`)
- [ ] [P2] Decompile/show Kotlin bytecode & Java equivalent (custom command)

---

## 3. Information Display

### 3.1 Hover
- [x] [P0] Declaration signature with resolved types and modifiers
- [x] [P0] Rendered KDoc (Markdown, `@param`, `@return`, `@throws`, `@sample`, `[links]`)
- [x] [P1] Javadoc rendering for Java symbols
- [x] [P1] Inferred type of expressions / smart-cast type (locals, `it`, generic substitution; "Smart cast to `T`" where a check narrows a local)
- [x] [P1] Constant values (`const val`, and `val`s with constant initializers: literals, operators with Int/Long overflow, string templates, other such `val`s)
- [ ] [P2] Deprecation info with replacement
- [x] [P2] Source library / module of the symbol

### 3.2 Signature help
- [~] [P0] `textDocument/signatureHelp` — active parameter, overloads, named arguments, trailing lambda awareness, vararg — *partial: calls, constructors, supertype/annotation/`this(…)` calls and function-typed values; overloads ranked by the arguments typed so far; KDoc summary and `@param` docs; not inside trailing lambdas*
- [x] [P1] Generic type parameters shown in instantiated form (bound by the receiver and the arguments typed so far; the least generic fitting overload is active)

### 3.3 Inlay hints (`textDocument/inlayHint`, `inlayHint/resolve`)
- [x] [P1] Local variable / property inferred types (also loop variables and destructuring; hidden for literals, constructor calls and casts)
- [x] [P1] Parameter names at call sites (configurable: literals only, all, off)
- [x] [P1] Lambda implicit `it` / `this` receiver type and return value hints (`^label` before the result of multi-line lambdas not returning `Unit`)
- [x] [P1] Chained call (fluent) intermediate types (chains over three or more lines with at least two types)
- [x] [P2] Function return type hints for expression bodies
- [ ] [P2] Ranges (`0 ≤ i < 10` for `0 until 10`)
- [ ] [P2] Suspend call markers, implicit receiver hints, value-parameter hints for `Function` types

### 3.4 Semantic tokens (`textDocument/semanticTokens/full`, `/delta`, `/range`)
- [x] [P0] Token types: namespace, class, interface, enum, enumMember, typeParameter, function, method, property, parameter, variable, keyword, modifier, annotation, string, number, comment, operator, label
- [~] [P1] Modifiers: declaration, readonly (`val`), mutable (`var`), static/companion, deprecated, abstract, suspend, extension, backing field, smart-cast, documentation — *partial: all but backing field and smart-cast; plus `defaultLibrary` for library symbols*
- [x] [P1] Soft keywords highlighted contextually (`data`, `value`, `sealed`, `by`, `get`, `set`…)
- [x] [P1] KDoc: `@tags` as decorators, `[links]` and tag subjects by what they resolve to (with the `documentation` modifier)
- [ ] [P2] String template injections, regex/SQL/JSON language injection highlighting

### 3.5 Other display
- [~] [P1] `textDocument/codeLens` — run/debug `main` & tests, references count, implementations count, overrides — *partial: reference counts on classes, functions and properties, computed in `codeLens/resolve` (command `kotlin.showReferences` with uri, position, locations)*
- [x] [P1] `textDocument/foldingRange` — imports, blocks, KDoc, multiline comments, `when` branches, region comments
- [x] [P1] `textDocument/selectionRange` — expand/shrink selection syntactically
- [ ] [P2] `textDocument/inlineValue` (debugger)
- [ ] [P2] `textDocument/documentColor` / `colorPresentation` (Compose `Color(...)`, Android colors)
- [ ] [P2] `textDocument/linkedEditingRange` (e.g. label `foo@` ↔ `@foo`)
- [ ] [P2] `textDocument/moniker`

---

## 4. Completion (`textDocument/completion`, `completionItem/resolve`)

- [x] [P0] Scope members: locals, parameters, members, top-levels, imported symbols
- [x] [P0] Member completion after `.` and `?.` (incl. extensions applicable to receiver type; Java getters as properties)
- [~] [P0] Keyword completion (context-aware: `override`, `return@label`, `when` branches…) — *partial: statement, expression, top-level and class-body keyword sets; labels: `return@` (lambdas, by label or called function), `break@`/`continue@` (labeled loops), `this@` (classes, lambdas), also after a typed `@`*
- [~] [P0] Auto-import: unimported classes, top-level functions, extensions (adds `import` via `additionalTextEdits`) — *partial: classes (libraries + project), top-level functions and properties (project + libraries, outside default imports), and extensions applicable to the receiver after `.` (project + libraries)*
- [~] [P1] Smart/type-aware ranking (expected type first, recently used, same module) — *partial: values of the expected type first (argument positions when every candidate agrees on the parameter class, declared variable/property/return types, assignment and `==` left sides, conditions, `when` subjects; subclasses, objects, classes with visible constructors, `true`/`false`, `null` for nullable types), then locals → members → extensions → package/imports → defaults → keywords → imports needed; case-matching first; not recently used*
- [x] [P1] Named argument completion (`param =`)
- [x] [P1] Override/implement member completion (generates full signature + body)
- [x] [P1] `when` exhaustive branch completion (sealed/enum; also `Boolean` and `null`): each missing condition, or all at once
- [~] [P1] Snippets / postfix templates: `.if`, `.not`, `.for`, `.val`, `.var`, `.return`, `.let`, `.notnull`, `.null`, `.try`, `.par`, `.sout`, `.when`, `.arg`, `.spread` — *partial: `.val`, `.var`, `.return`, `.if`, `.while`, `.not`, `.notnull`, `.null`, `.for`, `.when`, `.par`, `.arg`, `.println`, `.try`, `.spread` (arrays in arguments) (by type and statement position; names from the type); `.let` is the stdlib `let { }` completion*
- [~] [P1] Live templates: `main`, `fun0/1/2`, `sout`, `iter`, `ifn`, `inn`, `void`, `closure`, `object`, `interface`, `exfun`, `exval` — *partial: all but `void`, plus `maina`, `soutv`, `serr` (as snippet completions in statements, at top level or in classes)*
- [x] [P1] Lambda parameter completion (`{ a, b -> }` from expected function type; `(key, value) ->` for map entries and pairs)
- [x] [P1] Completion in string templates (`$` / `${`)
- [~] [P1] Completion in KDoc (`[Symbol]`, tags) — *partial: `[links]` (parameters, names in scope, members after `Type.`), `@param`/`@throws`/`@see` subjects, and tag names after an `@` that starts a line*
- [x] [P1] Annotation completion (incl. use-site targets): annotation classes first, then other classes; `field:`, `get:`, `param:`… targets
- [x] [P1] Package / import statement completion
- [~] [P2] Static member completion through companion without qualifier, enum entry completion without class prefix — *partial: companion/nested members of enclosing classes; entries of an expected enum type (`Color.RED`, matched by the entry name)*
- [x] [P2] Callable references (`::fn`, `Type::member`)
- [ ] [P2] Variable name suggestions from type (`val user: User` → `user`)
- [ ] [P2] Chained completion (`users.first().` → suggests via expected-type chains)
- [ ] [P2] Gradle Kotlin DSL completion (dependencies, plugin IDs, task names)
- [~] [P2] Commit characters, `insertReplace` ranges, label details, deprecation strike-through — *partial: label details, deprecation tags*
- [~] [P2] Snippet tab-stops for function arguments — *partial: cursor inside `()` / trailing lambda `{ }`*

---

## 5. Diagnostics

Delivery: [P0] `textDocument/publishDiagnostics` (push) **and/or** [P1] `textDocument/diagnostic` + `workspace/diagnostic` (pull). *(implemented: push, and document pull)* Include `code`, `codeDescription`, `relatedInformation`, `tags` (`Unnecessary`, `Deprecated`).

### 5.1 Compiler errors (subset of the K2 compiler diagnostic set)
- [x] [P0] Syntax errors (expecting X, unexpected token, unclosed string/comment); malformed literals (empty or multi-character `''`, illegal escapes, misplaced `_`, `l` suffix, out-of-range numbers, `L`/`u` on floating-point literals)
- [~] [P0] `UNRESOLVED_REFERENCE`, `UNRESOLVED_IMPORT` — *partial: imports, types, and expression names that exist nowhere (typos, deleted declarations), including members after call chains of certain type (`all().first().nmae`)*
- [~] [P0] `TYPE_MISMATCH`, `ARGUMENT_TYPE_MISMATCH`, `RETURN_TYPE_MISMATCH`, `INITIALIZER_TYPE_MISMATCH` — *partial: values of certain type — literals, locals/parameters (declared or initialized with a certain value; engine smart casts applied), properties with written types, and calls/member chains whose candidates agree on the result, with the call's type parameters inferred from argument types and lambda results and lambda parameters typed by their call (`listOf(u).first()`, `users().map { it.name }`; integer literals and conflicting arguments bind nothing; unknown type arguments kept as unknown); initializers, default values and return values (also each branch result of `if`/`else`, `when` and `try`; type arguments of variables and properties against written types, by declared and use-site variance); arguments likewise, for functions without overloads and with non-generic parameter types; `NONE_APPLICABLE` when every overload rejects a certain argument's type; `ASSIGNMENT_TYPE_MISMATCH` for assignments to variables/properties declared with a type; `CONDITION_TYPE_MISMATCH` for non-Boolean (or nullable) conditions; `RETURN_TYPE_MISMATCH` for returned values and expression bodies; `EQUALITY_NOT_APPLICABLE` (`==` between unrelated classes) and `INCOMPATIBLE_TYPES` (`when` branch values); `INVALID_IF_AS_EXPRESSION`*
- [~] [P0] `NONE_APPLICABLE`, `OVERLOAD_RESOLUTION_AMBIGUITY`, `TOO_MANY_ARGUMENTS`, `NO_VALUE_FOR_PARAMETER`, `NAMED_PARAMETER_NOT_FOUND` — *partial: argument counts and names (not types), all overloads considered; no ambiguity*
- [~] [P0] Null safety: `UNSAFE_CALL`, `UNSAFE_OPERATOR_CALL`, `UNSAFE_IMPLICIT_INVOKE_CALL`, `NULL_FOR_NONNULL_TYPE` — *partial: `UNSAFE_IMPLICIT_INVOKE_CALL` for nullable function-typed locals and parameters (with a `?.invoke` fix); nullable values passed, assigned or returned where non-null types are expected (type mismatches); `UNSAFE_OPERATOR_CALL` and `ITERATOR_ON_NULLABLE` (unless an operator for nullable receivers exists, like `String?.plus`), indexing nullable values; members of nullable property chains not null-checked earlier (`u.nick.length`); narrowing the engine does not model (earlier `!!`, `?:`, non-null assignments, `requireNotNull`, contract-style checks, calls returning `Nothing`) is respected; `NULL_FOR_NONNULL_TYPE` for literal `null`; `UNSAFE_CALL` for members of nullable call results (`find(1).name`, `map[k].name`, `a?.f().g()`: never smart cast) and of locals/parameters declared nullable or initialized with a nullable certain value, unless smart casts make them non-null at the use or a check the engine does not model (`!!`, `?:`, `?.`, passing as an argument, extension calls, assignments) narrows them*
- [~] [P0] `VAL_REASSIGNMENT`, `UNINITIALIZED_VARIABLE`, `MUST_BE_INITIALIZED`, `MUST_BE_INITIALIZED_OR_BE_ABSTRACT` — *partial: all four; `UNINITIALIZED_VARIABLE` (also class properties read by an earlier initializer or `init` block) and `VAL_REASSIGNMENT` of `val`s assigned later by control-flow analysis (branches, loops, `try`, jumps, `Nothing` calls; assignments in lambdas are unknown)*
- [~] [P0] `NO_RETURN_IN_FUNCTION_WITH_BLOCK_BODY`, `RETURN_NOT_ALLOWED` — *partial: `NO_RETURN_IN_FUNCTION_WITH_BLOCK_BODY` when any path reaches the end of a block-bodied function, anonymous function or getter with a result type (control flow through branches, exhaustive `when`s, loops, `try`, jumps and `Nothing` calls; unsure after `when`s of unknown exhaustiveness, unresolved calls and inline lambdas that may return; without types: no `return`/`throw` anywhere); `RETURN_NOT_ALLOWED` in `init` blocks and for values returned from constructors; `RETURN_TYPE_MISMATCH` for a bare `return` in a function with a result and for values returned from `Unit` functions; `BREAK_OR_CONTINUE_OUTSIDE_A_LOOP`; `RETURN_NOT_ALLOWED` for a bare `return` in a lambda passed to a non-inline function or `noinline` parameter (or stored), `NON_LOCAL_RETURN_NOT_ALLOWED` in `crossinline` lambdas; labels: `UNRESOLVED_LABEL`, `NOT_A_LOOP_LABEL`*
- [~] [P0] `ABSTRACT_MEMBER_NOT_IMPLEMENTED`, `ABSTRACT_CLASS_MEMBER_NOT_IMPLEMENTED`, `MANY_IMPL_MEMBER_NOT_IMPLEMENTED` — *partial: the first two, with an "Implement members" fix; `MANY_IMPL_MEMBER_NOT_IMPLEMENTED` for implementations from two unrelated Kotlin interfaces (no superclass)*
- [~] [P0] `NOTHING_TO_OVERRIDE`, `VIRTUAL_MEMBER_HIDDEN` (missing `override`), `OVERRIDING_FINAL_MEMBER`, `FINAL_SUPERTYPE` — *partial: all four (members of Kotlin and project supertypes; not Java supertypes); also `VAR_OVERRIDDEN_BY_VAL`, `CANNOT_WEAKEN_ACCESS_PRIVILEGE`, `RETURN_TYPE_MISMATCH_ON_OVERRIDE`, `PROPERTY_TYPE_MISMATCH_ON_OVERRIDE`, `VAR_TYPE_MISMATCH_ON_OVERRIDE`, `DEFAULT_VALUE_NOT_ALLOWED_IN_OVERRIDE`*
- [~] [P0] Visibility: `INVISIBLE_REFERENCE`, `INVISIBLE_MEMBER`, `EXPOSED_*` (exposed visibility) — *partial: `INVISIBLE_REFERENCE` for `receiver.member` when every member of that name is private (other class/file), protected (outside subclasses) or internal (Kotlin libraries), and for bare names of private top-level declarations of other files; `EXPOSED_FUNCTION_RETURN_TYPE`, `EXPOSED_PARAMETER_TYPE`, `EXPOSED_PROPERTY_TYPE`, `EXPOSED_RECEIVER_TYPE`, `EXPOSED_SUPER_CLASS`, `EXPOSED_SUPER_INTERFACE` for private/internal project classes (type arguments included; `protected` not compared)*
- [x] [P0] `NO_ELSE_IN_WHEN` / `WHEN_ON_SEALED_*` non-exhaustive (enum, Boolean, nullable, source sealed hierarchies; "Add remaining branches" fix)
- [~] [P0] `ILLEGAL_SUSPEND_FUNCTION_CALL`, `NON_LOCAL_SUSPENSION_POINT` — *partial: `ILLEGAL_SUSPEND_FUNCTION_CALL` (in non-suspend functions, accessors, initializers and plain lambdas passed to a single known function; inline lambdas inherit the context; SAM lambdas skipped), with a "Make 'f' suspend" fix; `NON_LOCAL_SUSPENSION_POINT` for suspend calls in lambdas passed to `crossinline` parameters of project functions*
- [~] [P0] `CONFLICTING_OVERLOADS`, `REDECLARATION`, `PACKAGE_OR_CLASSIFIER_REDECLARATION` — *partial: classifiers, members and locals in one scope*
- [~] [P1] Generics (*partial: `WRONG_NUMBER_OF_TYPE_ARGUMENTS`, star projections counted, not where arguments may be inferred or omitted; `TYPE_VARIANCE_CONFLICT` for `in`/`out` class type parameters in member signatures (through declared and use-site variance and function types; `@UnsafeVariance` and private members exempt); `UPPER_BOUND_VIOLATED` for explicit type arguments of types and calls*): `UPPER_BOUND_VIOLATED`, `WRONG_NUMBER_OF_TYPE_ARGUMENTS`, `TYPE_VARIANCE_CONFLICT`, `CANNOT_INFER_PARAMETER_TYPE`, `NEW_INFERENCE_NO_INFORMATION_FOR_PARAMETER`
- [~] [P1] Modifiers: `INCOMPATIBLE_MODIFIERS`, `REDUNDANT_MODIFIER`, `WRONG_MODIFIER_TARGET`, `REPEATED_MODIFIER` — *partial: all but `REDUNDANT_MODIFIER`*
- [~] [P1] Annotations: `WRONG_ANNOTATION_TARGET`, `ANNOTATION_ARGUMENT_MUST_BE_CONST`, opt-in (`OPT_IN_USAGE`, `OPT_IN_USAGE_ERROR`) — *partial: `WRONG_ANNOTATION_TARGET` for project annotations with `@Target` and common `kotlin.jvm` annotations (without use-site targets), `JVM_STATIC_NOT_IN_OBJECT_OR_COMPANION`, `ANNOTATION_ARGUMENT_MUST_BE_CONST` (non-`const` project properties and locals); opt-in for markers of the project and of libraries (read from class files; on the used declaration, or its class for constructor calls), satisfied by `@OptIn`/the marker on an enclosing declaration, statement or file, or the module's `optIn` compiler options (with an "Opt in" fix); not `@WasExperimental` with older API versions*
- [~] [P1] Data/value/sealed/enum class rule violations (`DATA_CLASS_WITHOUT_PARAMETERS`, `VALUE_CLASS_*`, `SEALED_SUPERTYPE*`) — *partial: data classes, interface/object constructors and initializers; value classes (one `val` parameter, final, not local/inner, no backing fields or delegates, no class supertypes, `@JvmInline` outside multiplatform modules); sealed constructors not public/internal, sealed inheritors in another package or module, or local/anonymous; enum entries not implementing the enum's abstract members; `fun interface` shape; `SUPERTYPE_APPEARS_TWICE`*
- [~] [P1] `expect`/`actual`: `NO_ACTUAL_FOR_EXPECT`, `ACTUAL_WITHOUT_EXPECT`, `ACTUAL_MISSING`, incompatible signatures — *partial: `NO_ACTUAL_FOR_EXPECT` (top-level, no `actual` anywhere in the project), `ACTUAL_WITHOUT_EXPECT` (with a "Remove 'actual'" fix); not per platform module*
- [~] [P1] `CONST_VAL_*`, `LATEINIT_*`, `DELEGATE_SPECIAL_FUNCTION_*` (missing `getValue`/`setValue`) — *partial: `const` and `lateinit` rules; `DELEGATE_SPECIAL_FUNCTION_MISSING` for delegates of certain Kotlin types (no member or extension `getValue`/`setValue`/`provideDelegate`); also `ITERATOR_MISSING` for `for` loops; property shape errors: `VAL_WITH_SETTER`, `ACCESSOR_FOR_DELEGATED_PROPERTY`, `DELEGATED_PROPERTY_IN_INTERFACE`, `ABSTRACT_DELEGATED_PROPERTY`, `EXTENSION_PROPERTY_WITH_BACKING_FIELD`, `INLINE_PROPERTY_WITH_BACKING_FIELD`, `PROPERTY_WITH_NO_TYPE_NO_INITIALIZER`, `VARIABLE_WITH_NO_TYPE_NO_INITIALIZER`, getter/setter visibility rules, private setters of abstract/open properties; `expect` declarations with bodies, initializers or delegates; private interface functions without a body*
- [~] [P1] `OPERATOR_MODIFIER_REQUIRED`, `INFIX_MODIFIER_REQUIRED` — *partial: both (binary arithmetic and range operators of Kotlin declarations; no `operator` candidate among members and extensions)*
- [x] [P1] `CREATING_AN_INSTANCE_OF_ABSTRACT_CLASS` (abstract/sealed classes, interfaces, enums; not when a same-named factory function or companion `invoke` exists), `VARIABLE_EXPECTED`
- [~] [P1] `SUPERTYPE_NOT_INITIALIZED`, `SUPERTYPE_INITIALIZED_IN_INTERFACE`, `CYCLIC_INHERITANCE_HIERARCHY` — *partial: all three, plus `MANY_CLASSES_IN_SUPERTYPE_LIST`, `INTERFACE_WITH_SUPERCLASS`*
- [~] [P1] `DEPRECATION_ERROR`, `API_NOT_AVAILABLE` (api-version) — *partial: `DEPRECATION_ERROR` for project and library declarations deprecated with level `ERROR`/`HIDDEN`, and library ones whose `@DeprecatedSinceKotlin` error version the module's Kotlin API version reached (not yet deprecated ones are not reported); not Kotlin built-ins*
- [x] [P1] Signatures: `MULTIPLE_VARARG_PARAMETERS`, `REIFIED_TYPE_PARAMETER_NO_INLINE` ("Add 'inline'" fix), `TYPE_PARAMETER_AS_REIFIED` (`T::class`)
- [~] [P1] Platform-specific: `JVM_*` clashes (`CONFLICTING_JVM_DECLARATIONS`, `ACCIDENTAL_OVERRIDE`), JS/Native target errors — *partial: `CONFLICTING_JVM_DECLARATIONS` within a class or file facade (property accessors vs functions, overloads with the same erasure; generic functions, value classes and `@Jvm*`-annotated declarations skipped); inline functions: `USAGE_IS_NOT_INLINABLE` ("Add 'noinline'"), inline parameters called where they are not inlined ("Add 'crossinline'"), `NON_PUBLIC_CALL_FROM_PUBLIC_INLINE` (private, and internal without `@PublishedApi`)*
- [x] [P0] `SMARTCAST_IMPOSSIBLE`: members of nullable `var`, open, delegated or custom-getter properties (`x` or `this.x`) after a null check that would smart cast a stable value (`if`/`else` branches, `&&`/`||` operands, `if (x == null) return` guards), with `?.`/`!!.` fixes
- [x] [P1] Operators and arguments: `INAPPLICABLE_OPERATOR_MODIFIER` (names and parameter counts of the conventions, members/extensions only), `INAPPLICABLE_INFIX_MODIFIER`, `INC_DEC_SHOULD_NOT_RETURN_UNIT`, `COMPARE_TO_TYPE_MISMATCH`, `ASSIGNMENT_OPERATOR_SHOULD_RETURN_UNIT`, `DECLARATION_CANT_BE_INLINED`, `NO_THIS`, `SUPER_NOT_AVAILABLE`, `ARGUMENT_PASSED_TWICE`, `NON_VARARG_SPREAD`, `NAMED_ARGUMENTS_NOT_ALLOWED` (Java methods and constructors)
- [x] [P1] References: `FUNCTION_CALL_EXPECTED` (bare names of top-level or local functions, with "Add '()'"), `NO_COMPANION_OBJECT` (project and Java classes), `ABSTRACT_SUPER_CALL`, `CONST_VAL_WITH_NON_CONST_INITIALIZER` (project calls and non-const properties), `COMPONENT_FUNCTION_MISSING` (initializers of certain type); `VAL_OR_VAR_ON_FUN_PARAMETER` / `_SECONDARY_CONSTRUCTOR_PARAMETER` / `_LOOP_PARAMETER` / `_CATCH_PARAMETER`
- [x] [P1] Declaration restrictions: `TOPLEVEL_TYPEALIASES_ONLY`, `LOCAL_INTERFACE_NOT_ALLOWED`, `LOCAL_OBJECT_NOT_ALLOWED`, `LOCAL_ENUM_NOT_ALLOWED`, `ILLEGAL_DECLARATION_IN_WHEN_SUBJECT`, `COMMA_IN_WHEN_CONDITION_WITHOUT_ARGUMENT` (fix: `||`), `DATA_CLASS_VARARG_PARAMETER`, `WRONG_MODIFIER_CONTAINING_DECLARATION` (`inner` in objects/interfaces), `DELEGATION_IN_INTERFACE`, `COMPANION_OBJECT_NOT_ALLOWED`, `MANY_COMPANION_OBJECTS`, `SECONDARY_CONSTRUCTOR_IN_OBJECT`, `PRIMARY_CONSTRUCTOR_DELEGATION_CALL_EXPECTED`, `UPPER_BOUND_IS_EXTENSION_FUNCTION_TYPE`, `BOUND_ON_TYPE_ALIAS_PARAMETER_NOT_ALLOWED`, `IS_ENUM_ENTRY`, `ENUM_ENTRY_AS_TYPE`, `TYPE_PARAMETERS_IN_OBJECT`, `TYPE_PARAMETERS_IN_ENUM`, `ANONYMOUS_FUNCTION_WITH_NAME`, `TAILREC_ON_VIRTUAL_MEMBER_ERROR`; `CANNOT_CHECK_FOR_ERASED` (`is` checks with concrete type arguments on values of non-generic types)
- [x] [P1] Misused declarations and operators: `FUNCTION_EXPECTED` (properties of non-invocable types called like functions, `s.length()`, with a fix), calls of function-typed locals/parameters (argument count and types), `NO_SET_METHOD` (indexed assignment on read-only collections and types without `set`), `INVISIBLE_SETTER` (`private set` outside its class/file), `LATEINIT_INTRINSIC_CALL_ON_NON_LATEINIT`, `SPREAD_OF_NULLABLE`, operators whose `operator` functions reject a certain operand (`1 + "a"`, `1 < "a"`: `NONE_APPLICABLE`) or do not exist (`true + 1`, `-"a"`: `UNRESOLVED_REFERENCE`; built-in and project classes), unresolved names after project classifiers (`E.B`; annotated classes skipped for compiler plugins), `INCOMPATIBLE_TYPES` for `is` checks between unrelated classes and integer literals against unrelated classes (`when`/`==`), integer literals too large for `Int` typed `Long`, nullable `val` properties of the class, Java classes, `?.let { }`, `T?` values and `?: null` returned as non-null, getter values and `return`s against the property type; declarations: `NULLABLE_SUPERTYPE`, `CONST_VAL_WITH_DELEGATE`, `WRONG_GETTER_RETURN_TYPE`, `WRONG_SETTER_PARAMETER_TYPE`, `CYCLIC_CONSTRUCTOR_DELEGATION_CALL` (targets chosen by argument count), `EXPLICIT_DELEGATION_CALL_REQUIRED` (project superclasses without a no-argument constructor)
- [x] [P1] Values, lambdas and declarations: `ASSIGNMENT_TYPE_MISMATCH` also for variables and properties typed by their initializer (`var x = 1; x = "s"`); compound assignments and ranges whose operators reject the operand (`i += "s"`, `1.."a"`) or do not exist (`s -= 1`); `TYPE_MISMATCH` for `throw`/`catch` of non-`Throwable` types; `INCOMPATIBLE_TYPES` for `is` branches of `when`; `FUNCTION_EXPECTED` for locals of non-invocable types and objects called like constructors (`x(1)`, `O()`; not when a function of that name is reachable or the type extends a function type); `VAL_REASSIGNMENT` of loop, `catch` and lambda parameters; annotation parameters (`VAR_ANNOTATION_PARAMETER`, `MISSING_VAL_ON_ANNOTATION_PARAMETER`, `INVALID_TYPE_OF_ANNOTATION_MEMBER`); `UNSAFE_CALL` on nullable class properties not named elsewhere in the function; `TYPE_CANT_BE_USED_FOR_CONST_VAL`, `CONST_VAL_WITH_NON_CONST_INITIALIZER` for library top-level calls and constructors; vararg element types; arguments of generic calls typed by the declared type (`val xs: List<Int> = listOf("a")`, all overloads of that arity rejecting); `EXPECTED_PARAMETERS_NUMBER_MISMATCH` / `EXPECTED_PARAMETER_TYPE_MISMATCH` for lambdas against declared function types and the parameters of every call candidate (not Java members of mapped built-ins, `Map.forEach(BiConsumer)`); `ABSTRACT_MEMBER_NOT_IMPLEMENTED` for plain Java interfaces (`Runnable`, library interfaces; members matched by name and arity); constructor arguments of enum entries
- [x] [P1] Receivers, constructors and references: `VAL_REASSIGNMENT` for compound assignments on `val`s of built-in and read-only collection types (`x += 1`); missing `inc`/`dec`/`contains` operators (`"a"++`, `1 in 2`) and `NO_GET_METHOD` (`1[0]`); superclass constructor arguments (`class B : A()`: count and types, project classes with one constructor); `INVISIBLE_REFERENCE` for calls of private constructors; generic calls checked against declared return types (`fun f(): List<String> = listOf(1)`) and `k to v` pairs against map type arguments (`mapOf(1 to 2)` as `Map<String, Int>`); indexed assignments whose `set` rejects the indices or value (`m["a"] = "b"`); elements of the wrong type added to mutable collections (`l += "s"`); `REDECLARATION` of classes in one file and of parameters; `UNRESOLVED_REFERENCE` for outer instance members used from nested classes and companions, and instance members called through the class (`C.m()`; inherited companion members and member extensions skipped); `FUNCTION_EXPECTED` for top-level and member properties; arguments against type parameter bounds (`fun <T : Number> g(t: T)` called with `"a"`); callable references against expected function types (`val f: (String) -> Int = ::g`); `CONSTRUCTOR_IN_OBJECT` for `object O(val x: Int)`
- [ ] [P2] Compiler plugin diagnostics (serialization: `SERIALIZER_NOT_FOUND`, Compose: `COMPOSABLE_INVOCATION`…)

### 5.2 Compiler warnings
- [x] [P1] Values: `FORBIDDEN_IDENTITY_EQUALS` (Kotlin 2; `DEPRECATED_IDENTITY_EQUALS` before) for `===` between primitives, `UNNECESSARY_LATEINIT` (assigned directly in an `init` block), `DIVISION_BY_ZERO` (integral), `USELESS_ELVIS_RIGHT_IS_NULL`, `SENSELESS_NULL_IN_WHEN`, `ARRAY_EQUALITY_OPERATOR_CAN_BE_REPLACED_WITH_EQUALS` ("Replace with 'contentEquals'"), `NO_TAIL_CALLS_FOUND`
- [~] [P0] `UNUSED_VARIABLE`, `UNUSED_PARAMETER`, `UNUSED_EXPRESSION`, `UNUSED_VALUE`, `ASSIGNED_BUT_NEVER_ACCESSED_VARIABLE` (tag `Unnecessary`) — *partial: `UNUSED_EXPRESSION` for literals, local names, `this`, `is` checks and equalities of those in statement position, `UNUSED_LAMBDA_EXPRESSION` ("Wrap in 'run'"); (`UNUSED_VALUE` and `VARIABLE_WITH_REDUNDANT_INITIALIZER` by data-flow analysis; locals captured by lambdas are skipped); `UNUSED_VARIABLE`, `UNUSED_PARAMETER` (functions and constructors whose signature is not fixed by an override, annotation, operator or function reference; "Remove parameter" fix updates call sites of private/local functions), `UNUSED_ANONYMOUS_PARAMETER` (rename to `_`), as lint rules*
- [x] [P0] `UNNECESSARY_SAFE_CALL`, `UNNECESSARY_NOT_NULL_ASSERTION`, `USELESS_ELVIS`, `SENSELESS_COMPARISON` (receivers with declared non-null types)
- [x] [P0] `DEPRECATION` (tag `Deprecated`)
- [~] [P1] `USELESS_CAST`, `USELESS_IS_CHECK`, `CAST_NEVER_SUCCEEDS`, `UNCHECKED_CAST` — *partial: all four, on literals and certain-typed values (smart casts applied); `CAST_NEVER_SUCCEEDS` between unrelated classes (not interfaces); `UNCHECKED_CAST` from non-generic types to generic ones and non-reified type parameters*
- [x] [P1] `UNREACHABLE_CODE` (by control flow: after jumps, `if`/`when` whose branches all leave, `Nothing` calls, loops without `break`), `REDUNDANT_ELSE_IN_WHEN`, `DUPLICATE_LABEL_IN_WHEN` (as `DUPLICATE_BRANCH_CONDITION_IN_WHEN`: literals, enum entries, objects); also `ELSE_MISPLACED_IN_WHEN`
- [x] [P1] `NAME_SHADOWING`, `VARIABLE_WITH_REDUNDANT_INITIALIZER`
- [~] [P1] `PLATFORM_CLASS_MAPPED_TO_KOTLIN`, `EXTENSION_SHADOWED_BY_MEMBER` — *partial: `PLATFORM_CLASS_MAPPED_TO_KOTLIN` in types (boxed primitives, `Object`, `String`, collection interfaces…); `EXTENSION_SHADOWED_BY_MEMBER` (non-generic extensions)*
- [~] [P1] `NOTHING_TO_INLINE`, `REDUNDANT_PROJECTION`, `FINAL_UPPER_BOUND` — *partial: `NOTHING_TO_INLINE` (with a fix), `FINAL_UPPER_BOUND` (non-null, non-generic bounds), `REDUNDANT_PROJECTION` (with a fix; also `CONFLICTING_PROJECTION` and `PROJECTION_IN_IMMEDIATE_ARGUMENT_TO_SUPERTYPE` errors), plus `REDUNDANT_NULLABLE` (`T??`)*
- [~] [P2] `-Werror` / `-Xsuppress-warning` / `@Suppress` awareness — *partial: `@Suppress`, `@file:Suppress` and `//noinspection` for lint rules*

### 5.3 Inspections (IntelliJ-style, beyond compiler; each configurable severity + `@Suppress("Id")`)

*Implemented as lint rules (`src/analysis/lints/`): severity per rule or category via the `lint.rules` setting, `@Suppress`/`//noinspection` aware, quick fixes as code actions. Type-based rules run only when the module's dependencies resolved.*

**Redundancy**
- [x] [P1] `RedundantSemicolon`
- [x] [P1] `RemoveRedundantQualifierName` (package and enclosing-class qualifiers in types, constructor calls and member accesses when the shorter name means the same class; not `java.lang`, inherited nested classes or class headers)
- [x] [P1] `RedundantExplicitType` / `RedundantUnitReturnType` (locals initialized with a literal or a constructor call of the written type)
- [x] [P1] `RedundantVisibilityModifier` (`public` default; off in explicit API mode)
- [x] [P1] `RedundantModalityModifier` (`final` default, `open` in interface)
- [x] [P1] `RedundantUnitExpression`
- [x] [P1] `RedundantIf` (`if (x) true else false`)
- [~] [P1] `RedundantLambdaArrow`, `RedundantLambdaOrAnonymousFunction` — *partial: `RedundantLambdaOrAnonymousFunction` (lambdas invoked in place, fix: `run`); `RedundantLambdaArrow` (`{ -> … }`, `{ it -> … }`; as a call argument only when the call has one candidate)*
- [x] [P1] `RedundantSuspendModifier` (final, non-override functions whose bodies call nothing that is or may be `suspend`)
- [x] [P1] `RemoveRedundantBackticks`
- [~] [P1] `RemoveEmptyParenthesesFromLambdaCall`, `RemoveEmptyClassBody`, `RemoveEmptyPrimaryConstructor`, `RemoveEmptySecondaryConstructorBody` — *partial: `RemoveEmptySecondaryConstructorBody`; all but `RemoveEmptySecondaryConstructorBody`*
- [x] [P1] `RemoveCurlyBracesFromTemplate` (`${x}` → `$x`), `RemoveSingleExpressionStringTemplate`
- [x] [P1] `RedundantCompanionReference`, `RedundantObjectTypeCheck` (`C.Companion.member`; `is Obj` → `== Obj`)
- [x] [P1] `RedundantGetter` / `RedundantSetter`
- [~] [P1] `RedundantInnerClassModifier`, `RedundantEmptyInitializerBlock` — *partial: `RedundantInnerClassModifier`; `RedundantEmptyInitializerBlock`*
- [x] [P1] `RedundantNullableReturnType`, `RedundantRequireNotNullCall` (final functions whose every result is certainly non-null; values of certain non-null type)
- [~] [P1] `RedundantWith`, `RedundantAsSequence`, `RedundantSamConstructor` — *partial: `RedundantWith` (blocks not using the receiver), `RedundantSamConstructor` (SAM interface arguments of single-candidate calls); `RedundantAsSequence` (iterables and arrays followed by a terminal operation)*
- [x] [P1] `RedundantElvisReturnNull`, `RedundantRunCatching`
- [x] [P1] `RedundantConstructorKeyword`, `RedundantEnumConstructorInvocation`
- [~] [P1] `RedundantLabelMigration`, `RedundantValueArgument` (arg equals default) — *partial: `RedundantValueArgument` (project functions, literal or name defaults; named or trailing positional arguments)*
- [~] [P1] `RedundantExplicitTypeArguments` (`listOf<Int>(1)`) — *partial: as `RemoveExplicitTypeArguments`, for calls with one candidate whose type parameters are whole parameter types bound by arguments of exactly that certain type*
- [~] [P1] `UnusedImport`, `UnusedSymbol` (unused private/internal declarations), `UnusedReceiverParameter`, `UnusedLambdaExpressionBody`, `UnusedUnaryOperator` — *partial: `UnusedReceiverParameter`; `UnusedImport`, `UnusedSymbol` for private and local declarations*
- [~] [P2] `RedundantOverride` (only calls super), `RedundantInterpolationPrefix` — *partial: `RedundantOverride` (functions forwarding their parameters; not in data/value/enum classes or with other modifiers)*

**Idiomatic Kotlin / style**
- [x] [P1] `CanBeVal` (`var` never reassigned)
- [x] [P1] `CanBeParameter` (constructor `val` used only in init): `private` constructor properties read only in initializers and `init` blocks
- [x] [P1] `CanBePrimaryConstructorProperty`
- [x] [P1] `ConvertToStringTemplate` (string concatenation starting with a plain literal, on one line)
- [~] [P1] `LiftReturnOrAssignment` (out of `if`/`when`/`try`) — *partial: `if`/`else` and `when` with `else` whose branches are single returns or assignments to one target; not `try`*
- [x] [P1] `CascadeIf` → `when`, `IntroduceWhenSubject` (three or more conditions; subjects via the intention)
- [x] [P1] `ReplaceGetOrSet` (`a.get(i)` → `a[i]`), `ReplaceCallWithBinaryOperator` (`a.plus(b)` → `a + b`)
- [~] [P1] `ReplaceRangeToWithUntil` / `ReplaceUntilWithRangeUntil` (`..<`) — *partial: `ReplaceUntilWithRangeUntil` (Kotlin 1.9+); `a..b - 1` (`ReplaceRangeToWithRangeUntil`)*
- [~] [P1] `ReplaceSizeCheckWithIsNotEmpty`, `ReplaceSizeZeroCheckWithIsEmpty`, `ReplaceIsEmptyWithIfEmpty` — *partial: `ReplaceIsEmptyWithIfEmpty` (`ifEmpty`, `ifBlank`); the size/length checks*
- [x] [P1] `UseExpressionBody` (single `return` block body; a cursor action by default)
- [x] [P1] `IfThenToSafeAccess`, `IfThenToElvis`, `FoldInitializerAndIfToElvis`
- [x] [P1] `ReplaceNegatedIsEmptyWithIsNotEmpty`, `SimplifyNegatedBinaryExpression`, `SimplifyBooleanWithConstants` (stdlib `isEmpty`/`isBlank` pairs; `==`, `===`, `in`, `is`; `&& true`, `|| false`, `== true/false` on non-null Booleans)
- [~] [P1] `ConvertTwoComparisonsToRangeCheck`, `ConvertNaNEquality` — *partial: `ConvertTwoComparisonsToRangeCheck` (integral values, `..`/`..<`); `ConvertNaNEquality`*
- [x] [P1] `ReplaceJavaStaticMethodWithKotlinAnalog` (`Math.max`, `System.out.println`, `Arrays.asList`…): `Math` functions (adding `kotlin.math` imports), `Arrays` content functions, `parseInt`-style parsing, `Character` tests, `System.out.print(ln)`
- [~] [P1] `ReplaceToStringWithStringTemplate`, `ReplaceSubstringWithTake/Drop/SubstringBefore/After` — *partial: `ReplaceSubstringWithTake`, `ReplaceSubstringWithDropLast`*
- [x] [P1] `ReplaceManualRangeWithIndicesCalls` (`0 until list.size` → `list.indices`)
- [x] [P1] `ReplaceWithOperatorAssignment` (`x = x + 1` → `x += 1`) — *numbers, chars and strings (other types may have `plusAssign`); names and `this.x`/`a.b` chains only*
- [~] [P1] `ReplaceArrayEqualityOpWithArraysEquals`, `ReplaceArrayOfWithLiteral` (annotations) — *partial: `ReplaceArrayOfWithLiteral`; array `==` via the compiler warning*
- [x] [P1] `ReplacePutWithAssignment` (`map.put(k, v)` → `map[k] = v`) — *on `MutableMap`s, when the result is not used*
- [~] [P1] `ReplaceAssociateFunction`, `ReplaceMapIndexedWithListGenerator` — *partial: `ReplaceMapIndexedWithListGenerator`; `ReplaceAssociateFunction` (`associateWith`, `associateBy`)*
- [x] [P1] `JoinDeclarationAndAssignment`, `MoveVariableDeclarationIntoWhen` (assignment / `when` right after)
- [x] [P1] `MayBeConstant` (`val` → `const val`)
- [~] [P1] `ObjectLiteralToLambda`, `ConvertLambdaToReference`, `ConvertReferenceToLambda` — *partial: `ObjectLiteralToLambda` (SAM interfaces, one override without `this`/`return`); `ConvertLambdaToReference` (code action; trailing lambdas calling one top-level function)*
- [~] [P1] `RemoveExplicitTypeArguments`, `RemoveToStringInStringTemplate` — *partial: `RemoveToStringInStringTemplate`; `RemoveExplicitTypeArguments`*
- [~] [P1] `SelfAssignment`, `SuspiciousEqualsCombination`, `SuspiciousCallableReferenceInLambda` — *partial: `SuspiciousCallableReferenceInLambda`; `SelfAssignment` of local variables*
- [x] [P1] `ReplaceNotNullAssertionWithElvisReturn`, `ReplaceGuardClauseWithFunctionCall` (`require`/`check`, `requireNotNull`/`checkNotNull`; `val x = y!!` in functions returning Unit or nullable)
- [~] [P1] `ReplaceWithEnumMap`, `ReplaceWithIgnoreCaseEquals` — *partial: `ReplaceWithEnumMap` (`hashMapOf`/`HashMap` with explicit enum keys); `ReplaceWithIgnoreCaseEquals`*
- [~] [P1] `ReplaceStringFormatWithLiteral`, `ReplaceReadLineWithReadln`, `ReplacePrintlnWithLogging` — *partial: `ReplaceStringFormatWithLiteral` (only `%s`); `ReplaceReadLineWithReadln`*
- [~] [P1] `ScopeFunctionConversion` (`let` ↔ `also` ↔ `run` ↔ `apply`), `ComplexRedundantLet`, `SimpleRedundantLet` — *partial: `ComplexRedundantLet` (`x.let { f(it) }`); `SimpleRedundantLet` (`x.let { it.f() }`, `x?.let { it.f() }`)*
- [~] [P1] `UnnecessaryVariable`, `UnnecessaryOptInAnnotation` — *partial: `UnnecessaryVariable` (`val x = e; return x`)*
- [~] [P1] `KotlinConstantConditions` (always true/false) — *partial: literal conditions (not `while (true)`) and comparisons of integer literals*
- [~] [P1] `ConvertSecondaryConstructorToPrimary`, `ConvertTryFinallyToUseCall` — *partial: `ConvertTryFinallyToUseCall` (`val r = …` then `try { } finally { r.close() }`)*
- [~] [P1] `ControlFlowWithEmptyBody`, `EmptyRange` — *partial: `EmptyRange` (integer literals, with a `downTo` fix); `ControlFlowWithEmptyBody`*
- [x] [P2] `DestructuringWrongName`, `UnusedDataClassCopyResult`
- [~] [P2] `ReplaceCollectionCountWithSize`, `PlatformExtensionReceiverOfInline` — *partial: `ReplaceCollectionCountWithSize`*

**Collections & sequences**
- [~] [P1] `SimplifiableCallChain` (`filter{}.first()` → `first{}`, `map{}.filterNotNull()` → `mapNotNull{}`, `sortedBy{}.first()` → `minBy{}`…) — *partial: `filter{}` + `first`/`firstOrNull`/`last`/`lastOrNull`/`single`/`singleOrNull`/`count`/`size`/`any`/`none`/`isEmpty`/`isNotEmpty`, `map{}` + `filterNotNull`/`joinToString`, `sortedBy{}`/`sortedByDescending{}` + `first`/`firstOrNull` on iterables, arrays and sequences*
- [x] [P1] `SimplifiableCall` (`filter { it is T }` → `filterIsInstance<T>()`, `filter { it != null }` → `filterNotNull()`)
- [ ] [P1] `ConvertCallChainIntoSequence` (long chains on large collections)
- [~] [P1] `UselessCallOnCollection` / `UselessCallOnNotNull` (`orEmpty()` on non-null, `filterNotNull()` on non-null elements) — *partial: `UselessCallOnCollection` (`filterNotNull()` on non-null elements); `UselessCallOnNotNull` (`orEmpty()`, `isNullOrEmpty()`, `isNullOrBlank()` on values of certain non-null type)*
- [ ] [P1] `ReplaceManualForEachWithMap` / loop-to-stdlib suggestions (`for` loop → `map/filter/sum/any/all/count`)
- [~] [P1] `ReplaceWithCount`, `RedundantAsSequence` — *partial: `RedundantAsSequence`; `filter { }.count()` via `SimplifiableCallChain`*
- [x] [P2] `JavaCollectionsStaticMethod`, `JavaMapForEach`

**Naming & formatting conventions**
- [x] [P1] `ClassName`, `FunctionName`, `PropertyName`, `ConstPropertyName`, `PrivatePropertyName`, `ObjectPropertyName`, `LocalVariableName`, `EnumEntryName`, `PackageName`, `TestFunctionName`
- [x] [P1] `PackageDirectoryMismatch`
- [~] [P1] `MayBeConstant`, `TrailingComma` (missing/redundant per code style) — *partial: `MayBeConstant`*
- [~] [P2] `KDocUnresolvedReference`, `KDocMissingDocumentation` (public API) — *partial: `KDocMissingDocumentation` (off by default)*
- [x] [P2] `FileNameMatchesClass` (single top-level class)

**Probable bugs**
- [x] [P1] `EqualsOrHashCode` (one without the other)
- [~] [P1] `RecursivePropertyAccessor` / `RecursiveEqualsCall` — *partial: `RecursiveEqualsCall` (`this == other` in `equals`); `RecursivePropertyAccessor` (getters reading, setters assigning their own property; "Replace with 'field'" when there is a backing field)*
- [x] [P1] `ArrayInDataClass` (needs custom `equals`/`hashCode`)
- [x] [P1] `LeakingThis` (calling open member / passing `this` in constructor): init blocks, property initializers and secondary constructor bodies of open/abstract/sealed classes; not inside lambdas
- [x] [P1] `SuspiciousVarProperty`, `SuspiciousCollectionReassignment` (`+=` on `var` read-only list)
- [~] [P1] `ImplicitNullableNothingType`, `ImplicitThis` (configurable) — *partial: `ImplicitNullableNothingType`*
- [x] [P1] `DelegationToVarProperty`
- [x] [P1] `WrapUnaryOperator` (`-1.inc()` precedence)
- [x] [P1] `NestedLambdaShadowedImplicitParameter` (nested `it`) — *when the enclosing lambda's `it` is used too; not for `it.let`/`also`/`takeIf`/`takeUnless`*
- [~] [P1] `ReplaceArrayEqualityOpWithArraysEquals`, `CopyWithoutNamedArguments` — *partial: `CopyWithoutNamedArguments` (and the compiler's `ARRAY_EQUALITY_OPERATOR_CAN_BE_REPLACED_WITH_EQUALS`)*
- [~] [P1] `UnusedResultOfAsync` / `DeferredResultUnused`, `SuspendFunctionOnCoroutineScope`, `RedundantRunBlocking` — *partial: `DeferredResultUnused` (with "Replace with 'launch'")*
- [~] [P1] `RunBlockingInSuspendFunction`, `BlockingMethodInNonBlockingContext` (`Thread.sleep` in coroutine) — *partial: `BlockingMethodInNonBlockingContext` (`Thread.sleep`); `RunBlockingInSuspendFunction`*
- [~] [P1] `FlowCollectWithoutLaunch`, `GlobalCoroutineUsage` (`GlobalScope`) — *partial: `GlobalCoroutineUsage`*
- [~] [P1] `WarningOnMainUnusedParameterMigration`, `MainFunctionReturnUnit` — *partial: `MainFunctionReturnUnit`*
- [~] [P1] `OverridingDeprecatedMember`, `DeprecatedCallableAddReplaceWith` — *partial: `OverridingDeprecatedMember`*
- [x] [P1] `ConvertNaNEquality` (`x == Double.NaN`)
- [x] [P1] `ThrowableNotThrown` (`IllegalStateException()` without `throw`)
- [ ] [P1] `KotlinThrowsInspection` / `@Throws` mismatch for Java callers
- [x] [P1] `PublicApiImplicitType` (explicit API mode: `-Xexplicit-api`): as the compiler's `NO_EXPLICIT_VISIBILITY_IN_API_MODE` / `NO_EXPLICIT_RETURN_TYPE_IN_API_MODE` (with fixes) in modules with `explicitApi()`
- [~] [P2] `SuspiciousCascadingIf`, `ProtectedInFinal`, `NonExhaustiveWhenStatementMigration` — *partial: `ProtectedInFinal`*
- [~] [P2] `IncompleteDestructuring`, `UnsafeCastFromDynamic` (JS) — *partial: `IncompleteDestructuring` (project data classes)*
- [ ] [P2] `SerializationAnnotation`/`kotlinx.serialization` misuse, Compose-specific rules (modifier param, remember, stable)

**Java interop**
- [x] [P1] `UsePropertyAccessSyntax` (`getFoo()` → `foo`): Java getters (`isX` returning Boolean), not methods of types mapped to Kotlin or `getAndX`
- [~] [P1] `PlatformExtensionReceiverOfInline`, `HasPlatformType` (public API exposes platform type) — *partial: `HasPlatformType` (with nullable / non-null type fixes)*
- [x] [P1] `JavaIoSerializableObjectMustHaveReadResolve`
- [~] [P2] `FunctionWithLambdaExpressionBody`, `JvmStaticWithoutCompanion` — *partial: `FunctionWithLambdaExpressionBody` (and the compiler's `JVM_STATIC_NOT_IN_OBJECT_OR_COMPANION`)*

**Build scripts**
- [ ] [P2] Gradle `.kts`: deprecated configurations (`compile` → `implementation`), dependency version catalog suggestions, newer library version available

---

## 6. Code Actions (`textDocument/codeAction`, `codeAction/resolve`)

Code action kinds: `quickfix`, `refactor`, `refactor.extract`, `refactor.inline`, `refactor.rewrite`, `source`, `source.organizeImports`, `source.fixAll`.

### 6.1 Quick fixes (bound to diagnostics)
- [~] [P0] **Import** unresolved symbol (with chooser when ambiguous; class, function, extension, operator, delegate accessor) — *partial: classes, top-level functions/properties and extensions applicable to the receiver, from the project and libraries; not operators or delegate accessors*
- [~] [P0] **Add `?.`** / **add `!!`** / **surround with null check** / **add elvis** for unsafe calls — *partial: `?.`, `!!` (also for nullable type mismatches), surround the statement with a null check (locals)*
- [~] [P0] **Change type** of variable/parameter/return to match — *partial: for type mismatches and `null` (`T?`); parameter type of the called project function (also in another file); **Convert to 'T'** (`.toInt()`, `.toString()`, `.code`, `"1"`, `1.0`)*
- [~] [P0] **Add explicit cast** (`as T`) / **change to safe cast** (`as?`) — *partial: "Cast to 'T'" for type mismatches where the expected type is a subtype*
- [x] [P0] **Implement members** / **make class abstract**
- [~] [P0] **Add `override` modifier**, **remove `override`**, **make super member `open`** — *partial: add/remove `override`, make a project super member `open` (also in another file)*
- [x] [P0] **Add missing `when` branches** / **add `else` branch**
- [~] [P0] **Remove unused** variable/parameter/import/declaration — *partial: variable, import, private/local declaration*
- [x] [P0] **Change `val` to `var`**
- [~] [P0] **Initialize property** / **make `lateinit`** / **make `abstract`** / **move to constructor** — *partial: initializer with a default value for the written type, `lateinit`, `abstract` (in abstract/sealed classes)*
- [~] [P0] **Add `suspend` modifier** to containing function / wrap in `runBlocking`/`launch` — *partial: add `suspend`*
- [~] [P0] **Create from usage**: function, extension function, property, local variable, parameter, class, interface, enum, object, enum entry, constructor, type parameter, annotation, `component`/operator functions — *partial: function (top-level or member, incl. in other files), extension function/property, property, local variable, parameter, class; types from arguments and the expected type*
- [x] [P0] **Add missing return** / **add `return` with expression** (a default value for the return type)
- [~] [P1] **Add/remove argument**, **add named argument**, **change function signature** to match call, **add parameter to function** — *partial: add parameter(s) to a project function from extra positional or unknown named arguments (typed from the arguments, also in another file)*
- [ ] [P1] **Add type arguments**, **specify type explicitly**
- [~] [P1] **Make visible** (change visibility of referenced declaration), **make `public`/`internal`/`private`/`protected`** — *partial: "Make 'x' public" for `INVISIBLE_REFERENCE` (project declarations, any file); "Add '@PublishedApi'" for internal declarations used by public inline functions*
- [x] [P1] **Add `open`** to class/member, **remove `final`** (`FINAL_SUPERTYPE`, `OVERRIDING_FINAL_MEMBER`; in other files too)
- [~] [P1] **Add `operator`/`infix`/`inline`/`reified`/`crossinline`/`noinline`/`tailrec`/`data`/`const` modifier** — *partial: `operator` (`OPERATOR_MODIFIER_REQUIRED`), `inline` (`REIFIED_TYPE_PARAMETER_NO_INLINE`), `crossinline`/`noinline`, `suspend`; removals of misplaced `abstract`, `tailrec`, `const`, `inner`, `val`/`var`*
- [~] [P1] **Remove incompatible / redundant modifier** — *partial: redundant `public`/`final`/`open`/`abstract`*
- [ ] [P1] **Opt-in**: add `@OptIn(X::class)` to statement/function/class/file, or propagate `@X` marker, or add compiler arg
- [ ] [P1] **Replace deprecated usage** (`@Deprecated(replaceWith = ReplaceWith(...))`) — single and whole project
- [x] [P1] **Suppress warning** for statement/function/class/file (`@Suppress("...")`) — *for lint rules: innermost declaration or file*
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
- [~] [P1] **Apply inspection fix** for every inspection in §5.3 (one-to-one) — *partial: the implemented rules that have a safe fix*
- [~] [P2] **Fix all of this type in file / project** (`source.fixAll` + per-kind batch) — *partial: `source.fixAll` for unused/redundant code in the file*
- [ ] [P2] **Upgrade language/API version** / **enable language feature** in build file

### 6.2 Source actions
- [~] [P0] `source.organizeImports` — remove unused, sort per code style, collapse to `*` per threshold, handle aliases — *partial: removes unused/duplicate imports and sorts by `.editorconfig` `ij_kotlin_imports_layout` (default IntelliJ layout); no `*` collapsing*
- [~] [P1] `source.fixAll` — apply all safe fixes — *partial: unused and redundant code*
- [~] [P1] **Generate**: `equals()`/`hashCode()`, `toString()`, secondary constructor, KDoc stub, test class/method, `serialVersionUID` — *partial: `equals()`/`hashCode()` and `toString()` from constructor and backing-field properties (arrays via `contentEquals`), KDoc stub with `@param`/`@property`/`@return` (code action kind `source.generate`)*
- [ ] [P1] **Implement members…** / **Override members…** (with selection)
- [ ] [P1] **Add missing imports** for whole file (on paste / on save)
- [ ] [P2] **Optimize imports on the fly**, **convert star imports to explicit**

### 6.3 Intentions / refactor.rewrite (cursor-context actions)
**Declarations**
- [x] [P1] Convert to expression body ↔ block body (block body adds the inferred return type)
- [x] [P1] Specify type explicitly ↔ remove explicit type (adds imports; removal only when the initializer has exactly the declared type)
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
- [x] [P2] Add KDoc / generate `@param` tags

**Expressions & control flow**
- [~] [P1] `if` ↔ `when`; `when` with subject ↔ without subject; flatten / merge `when` branches — *partial: `if`/`else if` chain → `when`; `when` (subject a plain name) → `if` chain*
- [ ] [P1] `if` → elvis / safe access, elvis → `if`, `!!` → `if` check / elvis
- [~] [P1] Invert `if` condition; split `if` (`&&`/`||`) ↔ merge nested `if`s — *partial: invert `if`/`else` (conditions negated in place: `==`↔`!=`, `<`↔`>=`, `is`↔`!is`, `in`↔`!in`, `!x`↔`x`), split `if (a && b)` without `else`, merge nested `if`s without `else`*
- [x] [P1] Add/remove braces (`if`/`for`/`while`/`do`; not around a nested `if`)
- [~] [P1] Introduce / remove `when` subject; add `else` / remaining branches — *partial: introduce (`x == c`, `x is T`, `x in r` with one side-effect-free subject) and remove a subject; remaining branches via the `NO_ELSE_IN_WHEN` fix*
- [~] [P1] `for` loop ↔ `forEach`; loop → collection operations (`map/filter/sumOf…`); `forEach` → `for` — *partial: `for` → `forEach` (no jumps in the body), standard-library `forEach` → `for` (`return@forEach` → `continue`, implicit `it` named)*
- [ ] [P1] `for (i in 0 until n)` ↔ `repeat(n)`, `indices`, `withIndex()`
- [ ] [P1] `try/finally` → `use {}`
- [ ] [P1] Lambda ↔ anonymous function ↔ callable reference
- [x] [P1] Move lambda argument out of / into parentheses
- [~] [P1] Add/remove explicit lambda parameter (`it` ↔ named), specify explicit lambda signature — *partial: `it` → named (from `it` or the lambda's `{`)*
- [ ] [P1] Scope functions: convert between `let`/`run`/`also`/`apply`/`with`
- [~] [P1] Add/remove named arguments (single / all) — *partial: add names to all positional arguments (Kotlin functions)*
- [ ] [P1] Add/remove trailing comma, put arguments/parameters on separate lines ↔ one line
- [~] [P1] Swap binary expression operands, flip `equals`, De Morgan's law — *partial: flip comparisons and equalities, De Morgan on `&&`/`||` chains (dropping an existing `!(…)`)*
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
- [~] [P1] Concatenation ↔ string template — *partial: concatenation → template (starting from a String)*
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
- [~] [P0] Locals, parameters, properties, functions, classes, type params, labels — *partial: all but labels; library, Java, operator and library-override declarations are refused with a reason*
- [~] [P0] Cross-file and cross-module (incl. Java usages, `@JvmName` aware) — *partial: Kotlin files of the workspace; Java usages are not updated*
- [~] [P1] Rename overriding/overridden members together; rename `expect` + all `actual`s — *partial: the override family*
- [~] [P1] Rename class → rename file; rename package → move directory (`workspace/willRenameFiles`) — *partial: a top-level class's file named after it (when the client supports `RenameFile`)*
- [~] [P1] Rename named-argument usages and `component`/`getX` Java accessors — *partial: named arguments (calls, constructors, annotations); destructuring is left alone*
- [~] [P1] Rename in KDoc references, string literals/comments (opt-in), `@Suppress` IDs — *partial: KDoc links and `@param`/`@property`*
- [ ] [P1] Conflict detection (shadowing, redeclaration, visibility)
- [ ] [P2] Rename backing field / accessor pair, rename test along with subject

### 7.2 Extract (`refactor.extract`)
- [~] [P0] Extract variable (introduce local `val`; with "replace all occurrences") — *partial: the selected expression, named from the call/property or type; with "replace all occurrences" in the function's outermost block; refused out of conditional, short-circuit, loop or lambda code; no "replace all"*
- [~] [P0] Extract function (local, member, top-level, extension; infers params/return, handles multiple outputs via data class/pair) — *partial: statements or an expression → private function after the enclosing one; read locals become parameters, one local used afterwards the result; refused with jumps out, assignments to caller locals, or receiver lambdas*
- [~] [P1] Extract property (member / top-level), extract constant (`const val`) — *partial: extract constant (top-level `private const val` from a literal, optionally replacing all equal literals in the file)*
- [ ] [P1] Extract parameter (introduce parameter, with default value), extract lambda parameter (functional parameter)
- [ ] [P1] Extract interface / superclass
- [ ] [P1] Extract `typealias`
- [ ] [P2] Extract class from members (move fields + methods), extract sealed class hierarchy
- [ ] [P2] Introduce parameter object

### 7.3 Inline (`refactor.inline`)
- [x] [P0] Inline local variable (`val`; parenthesized where needed, `$x` → `${…}`)
- [~] [P1] Inline function (single call / all calls), inline property, inline `typealias`, inline constant — *partial: inline a top-level/local single-expression function into all calls (arguments or defaults substituted, parenthesized where needed; refused for recursion, nested calls, or a non-trivial argument used twice)*
- [ ] [P1] Inline super class / interface
- [ ] [P2] Inline lambda / scope function (`x.let { it.foo() }` → `x.foo()`)

### 7.4 Move & structural
- [ ] [P1] Move top-level declarations to another file / package (update imports)
- [ ] [P1] Move member to companion ↔ companion to class; move nested class to top level ↔ top level into class
- [~] [P1] Change signature (reorder, add, remove, rename params; change return type; update all call sites & overrides; add/remove receiver) — *partial: move a parameter left/right and remove an unused parameter, updating calls across the project (functions without an override family; refused when a reference is not a plain call)*
- [ ] [P1] Pull members up / push members down
- [ ] [P1] Safe delete (with usages check)
- [ ] [P2] Convert Java file to Kotlin (J2K)
- [ ] [P2] Convert anonymous object to named class / lambda
- [ ] [P2] Encapsulate field (introduce getter/setter or backing property)
- [ ] [P2] Copy class / file

---

## 8. Formatting

- [~] [P0] `textDocument/formatting` — full-document format per Kotlin coding conventions — *partial: indentation (blocks, continuation of `.`/`?.`/`?:`/operators/`=`/`when` arrows, brace-less bodies, accessors, supertype lists, block comments moving with their first line; raw strings untouched), trailing whitespace and final newline, with the client's tab size/spaces; spacing inside lines is kept. On the corpus: whitespace-only, idempotent, 0.1–0.5% of lines re-indented on IntelliJ-formatted projects*
- [~] [P1] `textDocument/rangeFormatting` (+ `rangesFormatting`) — *partial: `rangeFormatting` (lines starting in the range)*
- [~] [P1] `textDocument/onTypeFormatting` — auto-indent on `\n`, `}`, `)`; KDoc `*` continuation — *partial: re-indent the line on `}`*
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
- [x] [P0] Never crash on malformed input; isolate panics per request
- [ ] [P1] Bounded memory with LRU eviction of analysis results; lazy library indexing
- [ ] [P1] Parallel analysis across files/modules; work-stealing scheduler with priority for the active file
- [ ] [P1] Deterministic results independent of request order (salsa-style incremental queries)
- [ ] [P1] Configurable: per-inspection severity, inspection profiles (shared `.editorconfig`/project file), feature toggles
- [ ] [P1] Telemetry-free by default; structured logging with levels
- [ ] [P2] Test corpus: golden tests vs. `kotlinc` diagnostics, fuzzing the parser, LSP conformance tests
