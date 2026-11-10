# ktls

Vibe coded Kotlin language server in Rust.

A Kotlin language server written in Rust. This is the core infrastructure: protocol
handling, an error-tolerant parser, Gradle/Maven import with native dependency
resolution, binary classpath indexing (jars, the JDK image, Kotlin metadata), a
module-aware declaration index, import/type resolution with diagnostics, navigation
(definition, declaration, type definition, implementation, hover, references, highlights, rename,
call/type hierarchy), completion (incl. postfix templates and KDoc links), signature help, inlay hints, semantic tokens, folding and
selection ranges, import and create-from-usage fixes, refactoring assists (extract variable/function/constant, inline
variable/function, move/remove/add parameters, explicit types, block body, braces, `if` ↔ `when`, string templates, argument names,
named lambda parameter), reference-count code lenses, package/import updates when files move, formatting (indentation), smart casts, generate
`equals`/`hashCode`/`toString`/KDoc, organize imports and lint rules with
quick fixes.

See [FEATURES.md](FEATURES.md) for the full feature catalog and what is implemented.

## Build and run

```sh
cargo build --release
./target/release/ktls        # speaks LSP over stdio
KTLS_LOG=debug ./target/release/ktls   # logs go to stderr
```

Neovim (0.11+):

```lua
vim.lsp.config('ktls', {
  cmd = { '/path/to/ktls' },
  filetypes = { 'kotlin' },
  root_markers = { 'settings.gradle.kts', 'settings.gradle', 'build.gradle.kts', 'build.gradle', 'pom.xml' },
  init_options = { kotlin = { project = { allowBuildToolExecution = false } } },
})
vim.lsp.enable('ktls')
```

## Configuration

Passed as `initializationOptions` or via `workspace/configuration` (section `kotlin`):

| Setting | Default | Meaning |
|---|---|---|
| `jdkHome` | auto | JDK to index (otherwise `JAVA_HOME`, `java` on `PATH`, `/usr/lib/jvm`, SDKMAN, `~/.gradle/jdks`) |
| `cacheDir` | `$XDG_CACHE_HOME/ktls` | Downloaded artifacts and cached project models |
| `project.gradleMode` | `auto` | `static` (read build scripts), `tooling` (ask Gradle), `auto` (tooling when allowed and a wrapper exists) |
| `project.allowBuildToolExecution` | `false` | Allow running the project's `gradlew`. Off by default because it executes code from the project |
| `project.offline` | `false` | Use only local caches (`~/.m2`, Gradle's `files-2.1`, our cache) |
| `project.extraRepositories` | `[]` | Additional Maven repository URLs (`https://` or `file://`) |
| `diagnostics.enabled` / `diagnostics.debounceMs` | `true` / `200` | |
| `lint.enabled` | `true` | Run the lint rules below |
| `lint.rules` | `{}` | Level per rule id or category (see below) |
| `inlayHints.types` | `true` | Inferred types of properties, locals, loop variables and destructuring entries |
| `inlayHints.lambdaParameters` | `true` | Types of `it` and untyped lambda parameters |
| `inlayHints.returnTypes` | `true` | Return types of expression-bodied functions |
| `inlayHints.chainedCalls` | `true` | Types at the line ends of call chains over three or more lines |
| `inlayHints.lambdas` | `true` | Implicit `this` of lambdas with a receiver, and `^label` before what a multi-line lambda returns |
| `inlayHints.parameterNames` | `literals` | `literals` (literal arguments of multi-parameter calls), `all` or `none` |

Commands (`workspace/executeCommand`): `kotlin.reloadProject`, `kotlin.projectModel`
(returns the imported model as JSON: modules, source roots, resolved artifacts, problems).

## Lint rules

IntelliJ-style inspections, reported as diagnostics with code = rule id. Most have a quick fix
(code action); every finding also offers *Suppress for declaration / file*. `source.fixAll`
applies all unused/redundant-code fixes in a file. Rules that need types only run when the
module's dependencies resolved, and every rule prefers silence over guessing.

Suppress with `@Suppress("RuleId")` (also `@file:Suppress`, the aliases such as `unused`, and
IntelliJ's `//noinspection RuleId` comment). Configure levels with `lint.rules`; keys are rule
ids, categories (`compiler`, `unused`, `redundancy`, `style`, `bugs`, `naming`) or `all`, values `error`,
`warning`, `info`, `hint`, `action` (no diagnostic, only a code action at the cursor) or `off`:

```lua
init_options = { kotlin = { lint = { rules = { naming = "off", UnusedImport = "error" } } } }
```

**Compiler errors and warnings** (`compiler`)

Beyond syntax and unresolved imports/types (always reported). These run even with
`lint.enabled = false`; type-based ones only when the module's dependencies resolved, and
each stays silent when it cannot be sure (smart casts, unknown DSL receivers, unresolved
supertypes, generated code).

| Rule | Default | Flags |
|---|---|---|
| `UNRESOLVED_REFERENCE` | error | Name that does not resolve to any declaration |
| `VAL_REASSIGNMENT` | error | Assignment to a 'val' or parameter |
| `UNSAFE_CALL` | error | `.` member access on a nullable local/parameter that is never null-checked |
| `NO_RETURN_IN_FUNCTION_WITH_BLOCK_BODY` | error | Function with a return type whose block body has no 'return' |
| `ABSTRACT_MEMBER_NOT_IMPLEMENTED` | error | Concrete class missing an implementation of an interface member |
| `ABSTRACT_CLASS_MEMBER_NOT_IMPLEMENTED` | error | Concrete class missing an implementation of an abstract class member |
| `NOTHING_TO_OVERRIDE` | error | 'override' of a member no supertype declares |
| `VIRTUAL_MEMBER_HIDDEN` | error | Member with the signature of a supertype member but no 'override' |
| `OVERRIDING_FINAL_MEMBER` | error | 'override' of a final member |
| `NO_ACTUAL_FOR_EXPECT` | error | Top-level 'expect' declaration with no 'actual' anywhere in the project |
| `ACTUAL_WITHOUT_EXPECT` | error | 'actual' declaration with no matching 'expect' |
| `NO_ELSE_IN_WHEN` | error | 'when' that is not exhaustive |
| `REPEATED_MODIFIER` | error | The same modifier twice |
| `INCOMPATIBLE_MODIFIERS` | error | Modifiers that cannot be combined |
| `INAPPLICABLE_LATEINIT_MODIFIER` | error | 'lateinit' on a val, nullable or primitive type, or with an initializer |
| `WRONG_MODIFIER_TARGET` | error | Modifier not applicable to this declaration |
| `CONST_VAL_NOT_TOP_LEVEL_OR_OBJECT` | error | 'const val' inside a class |
| `CONST_VAL_WITHOUT_INITIALIZER` | error | 'const val' without a value |
| `CONST_VAL_WITH_GETTER` | error | 'const val' with a getter |
| `ABSTRACT_FUNCTION_WITH_BODY` | error | Abstract function with a body |
| `NON_ABSTRACT_FUNCTION_WITH_NO_BODY` | error | Member function without a body that is not abstract |
| `NON_MEMBER_FUNCTION_NO_BODY` | error | Top-level or local function without a body |
| `ABSTRACT_PROPERTY_WITH_INITIALIZER` | error | Abstract property with an initializer |
| `ABSTRACT_PROPERTY_WITH_GETTER` | error | Abstract property with a getter body |
| `ABSTRACT_FUNCTION_IN_NON_ABSTRACT_CLASS` | error | Abstract function in a concrete class or object |
| `ABSTRACT_PROPERTY_IN_NON_ABSTRACT_CLASS` | error | Abstract property in a concrete class or object |
| `MUST_BE_INITIALIZED` | error | Top-level property without an initializer |
| `MUST_BE_INITIALIZED_OR_BE_ABSTRACT` | error | Member property that is never initialized |
| `DATA_CLASS_WITHOUT_PARAMETERS` | error | Data class with no constructor parameters |
| `DATA_CLASS_NOT_PROPERTY_PARAMETER` | error | Data class constructor parameter without 'val'/'var' |
| `CONSTRUCTOR_IN_INTERFACE` | error | Interface with a constructor |
| `CONSTRUCTOR_IN_OBJECT` | error | Object with a constructor |
| `ANONYMOUS_INITIALIZER_IN_INTERFACE` | error | Interface with an 'init' block |
| `PROPERTY_INITIALIZER_IN_INTERFACE` | error | Interface property with an initializer |
| `SUPERTYPE_INITIALIZED_IN_INTERFACE` | error | Interface calling a supertype constructor |
| `INTERFACE_WITH_SUPERCLASS` | error | Interface extending a class |
| `SUPERTYPE_NOT_INITIALIZED` | error | Superclass constructor not called |
| `MANY_CLASSES_IN_SUPERTYPE_LIST` | error | More than one superclass |
| `FINAL_SUPERTYPE` | error | Extending a final class |
| `REDECLARATION` | error | Two declarations with the same name in one scope |
| `CONFLICTING_OVERLOADS` | error | Two functions with the same signature in one scope |
| `INITIALIZER_TYPE_MISMATCH` | error | Value of the wrong type (literal, typed local/parameter, constructor call) as an initializer or default value |
| `RETURN_TYPE_MISMATCH` | error | Value of the wrong type returned |
| `ILLEGAL_SUSPEND_FUNCTION_CALL` | error | Call to a 'suspend' function outside a coroutine or 'suspend' function |
| `CREATING_AN_INSTANCE_OF_ABSTRACT_CLASS` | error | Constructor call of an abstract or sealed class, an interface or an enum |
| `INVISIBLE_REFERENCE` | error | Use of a private, protected or internal declaration where it is not visible |
| `OPERATOR_MODIFIER_REQUIRED` | error | Operator use of a function that is not 'operator' |
| `UPPER_BOUND_VIOLATED` | error | Type argument outside its parameter's upper bound |
| `ASSIGNMENT_TYPE_MISMATCH` | error | Assigned value of the wrong type (literal, typed local/parameter, constructor call) |
| `CONDITION_TYPE_MISMATCH` | error | Condition that is not a Boolean |
| `INFIX_MODIFIER_REQUIRED` | error | Infix call of a function that is not 'infix' |
| `VARIABLE_EXPECTED` | error | Assignment to something that is not a variable |
| `RETURN_NOT_ALLOWED` | error | 'return' in an initializer block, or a value returned from a constructor |
| `BREAK_OR_CONTINUE_OUTSIDE_A_LOOP` | error | 'break' or 'continue' outside a loop |
| `UNINITIALIZED_VARIABLE` | error | Local variable read before it is assigned |
| `WRONG_NUMBER_OF_TYPE_ARGUMENTS` | error | Type with the wrong number of type arguments |
| `VAR_OVERRIDDEN_BY_VAL` | error | 'val' overriding a 'var' |
| `CANNOT_WEAKEN_ACCESS_PRIVILEGE` | error | Override with less visibility than the overridden member |
| `RETURN_TYPE_MISMATCH_ON_OVERRIDE` | error | Override whose return type is not a subtype of the overridden one |
| `PROPERTY_TYPE_MISMATCH_ON_OVERRIDE` | error | 'val' override whose type is not a subtype of the overridden one |
| `VAR_TYPE_MISMATCH_ON_OVERRIDE` | error | 'var' override whose type differs from the overridden one |
| `MULTIPLE_VARARG_PARAMETERS` | error | More than one 'vararg' parameter |
| `DEFAULT_VALUE_NOT_ALLOWED_IN_OVERRIDE` | error | Default parameter value in an 'override fun' |
| `REIFIED_TYPE_PARAMETER_NO_INLINE` | error | 'reified' type parameter of a non-inline declaration |
| `TYPE_PARAMETER_AS_REIFIED` | error | 'T::class' of a type parameter that is not reified |
| `EXPOSED_FUNCTION_RETURN_TYPE` | error | Function whose return type is less visible than the function |
| `EXPOSED_PARAMETER_TYPE` | error | Function or constructor whose parameter type is less visible than it |
| `EXPOSED_PROPERTY_TYPE` | error | Property whose type is less visible than the property |
| `EXPOSED_RECEIVER_TYPE` | error | Extension whose receiver type is less visible than the extension |
| `EXPOSED_SUPER_CLASS` | error | Class whose superclass is less visible than the class |
| `EXPOSED_SUPER_INTERFACE` | error | Class whose super-interface is less visible than the class |
| `EQUALITY_NOT_APPLICABLE` | error | '==' between values of unrelated classes |
| `INCOMPATIBLE_TYPES` | error | 'when' branch value whose class is unrelated to the subject's |
| `INVALID_IF_AS_EXPRESSION` | error | 'if' without 'else' used as a value |
| `ELSE_MISPLACED_IN_WHEN` | error | 'else' branch that is not the last one in a 'when' |
| `UNSAFE_OPERATOR_CALL` | error | Operator on a nullable value without an operator for nullable receivers |
| `ITERATOR_ON_NULLABLE` | error | 'for' over a nullable value |
| `DEPRECATION_ERROR` | error | Use of a project declaration deprecated with level ERROR or HIDDEN |
| `OPT_IN_USAGE_ERROR` | error | Use of a declaration that requires opt-in (marker with level ERROR) without opting in |
| `CYCLIC_INHERITANCE_HIERARCHY` | error | Class that inherits from itself through its supertypes |
| `MANY_IMPL_MEMBER_NOT_IMPLEMENTED` | error | Member inherited with implementations from two interfaces and not overridden |
| `ITERATOR_MISSING` | error | 'for' over a value without 'iterator()' |
| `DELEGATE_SPECIAL_FUNCTION_MISSING` | error | Property delegate without 'getValue' (or 'setValue' for a 'var') |
| `ARGUMENT_TYPE_MISMATCH` | error | Argument of the wrong type (literal, typed local/parameter, constructor call) for a function without overloads |
| `NULL_FOR_NONNULL_TYPE` | error | 'null' for a non-null type |
| `TOO_MANY_ARGUMENTS` | error | More arguments than parameters |
| `NO_VALUE_FOR_PARAMETER` | error | Required parameter without an argument |
| `NAMED_PARAMETER_NOT_FOUND` | error | Named argument for a parameter that does not exist |
| `NONE_APPLICABLE` | error | No overload accepts these arguments |
| `USELESS_CAST` | warning | Cast to the type the value already has |
| `USELESS_IS_CHECK` | warning | 'is' check that is always true or always false |
| `CAST_NEVER_SUCCEEDS` | warning | Cast between unrelated classes |
| `NOTHING_TO_INLINE` | warning | 'inline' function without parameters of function type |
| `REDUNDANT_NULLABLE` | warning | Type marked nullable twice ('T??') |
| `DUPLICATE_BRANCH_CONDITION_IN_WHEN` | warning | 'when' branch condition repeating an earlier one |
| `UNUSED_VALUE` | warning | Value assigned to a local that no path reads |
| `VARIABLE_WITH_REDUNDANT_INITIALIZER` | warning | Local initializer that is always overwritten before it is read |
| `ASSIGNED_BUT_NEVER_ACCESSED_VARIABLE` | warning | Local 'var' that is assigned but never read |
| `OPT_IN_USAGE` | warning | Use of a declaration that requires opt-in (marker with level WARNING) without opting in |
| `UNCHECKED_CAST` | warning | Cast to a generic type or type parameter the runtime cannot check |
| `EXTENSION_SHADOWED_BY_MEMBER` | warning | Extension with the signature of a member of its receiver |
| `FINAL_UPPER_BOUND` | warning | Type parameter bounded by a final class |
| `UNNECESSARY_SAFE_CALL` | warning | '?.' on a non-null receiver |
| `UNNECESSARY_NOT_NULL_ASSERTION` | warning | '!!' on a non-null value |
| `USELESS_ELVIS` | warning | '?:' after a non-null value |
| `SENSELESS_COMPARISON` | warning | Comparing a non-null value with null |
| `DEPRECATION` | warning | Use of a deprecated declaration |
| `REDUNDANT_ELSE_IN_WHEN` | warning | 'else' in a 'when' that is already exhaustive |
| `NAME_SHADOWING` | warning | Local declaration hiding an outer one |
| `UNREACHABLE_CODE` | warning | Code after 'return', 'throw', 'break' or 'continue' |
| `NON_FINAL_MEMBER_IN_FINAL_CLASS` | warning | 'open' member of a final class |
| `NON_FINAL_MEMBER_IN_OBJECT` | warning | 'open' member of an object |

**Unused code** (`unused`)

| Rule | Default | Flags |
|---|---|---|
| `UnusedImport` | warning | Import directive that nothing in the file uses |
| `UNUSED_VARIABLE` | warning | Local variable that is never read |
| `UNUSED_PARAMETER` | warning | Function or constructor parameter that is never used |
| `UNUSED_ANONYMOUS_PARAMETER` | info | Lambda parameter that is never used (can be '_') |
| `UnusedSymbol` | warning | Private or local declaration that is never used |
| `CanBeVal` | warning | Local 'var' that is never reassigned |

**Redundant code** (`redundancy`)

| Rule | Default | Flags |
|---|---|---|
| `RedundantSemicolon` | warning | Semicolon that is not needed |
| `RemoveCurlyBracesFromTemplate` | warning | '${x}' in a string template where '$x' suffices |
| `RedundantVisibilityModifier` | warning | 'public' modifier (the default visibility) |
| `RedundantModalityModifier` | warning | 'final', 'open' or 'abstract' that is implied anyway |
| `RedundantUnitReturnType` | warning | ': Unit' on a function with a block body |
| `RedundantIf` | warning | 'if (c) true else false' and similar |
| `RemoveEmptyClassBody` | warning | Empty '{}' class body |
| `RemoveEmptyPrimaryConstructor` | warning | Empty '()' primary constructor |
| `RemoveEmptyParenthesesFromLambdaCall` | info | 'foo() { }' where 'foo { }' suffices |
| `RedundantGetter` | warning | Getter that only returns the backing field |
| `RedundantSetter` | warning | Setter that only assigns the backing field |
| `RedundantEmptyInitializerBlock` | warning | Empty 'init {}' block |
| `RemoveRedundantBackticks` | warning | Backticks around a name that does not need them |
| `RedundantUnitExpression` | warning | 'Unit' expression that has no effect |
| `UnnecessaryVariable` | info | Variable that is only returned on the next line |
| `RemoveSingleExpressionStringTemplate` | info | '"$s"' where 's' is already a String |

**Style** (`style`)

| Rule | Default | Flags |
|---|---|---|
| `UseExpressionBody` | action | Block body consisting of a single 'return' |
| `MayBeConstant` | info | 'val' with a constant initializer that could be 'const' |
| `ReplaceRangeToWithRangeUntil` | info | 'a..b - 1' instead of 'a..<b' |
| `ReplaceManualRangeWithIndicesCalls` | info | '0 until xs.size' instead of 'xs.indices' |
| `ReplaceSizeZeroCheckWithIsEmpty` | info | 'xs.size == 0' instead of 'xs.isEmpty()' |
| `ReplaceSizeCheckWithIsNotEmpty` | info | 'xs.size > 0' instead of 'xs.isNotEmpty()' |
| `ReplaceGetOrSet` | info | Explicit 'get'/'set' operator call instead of indexing |
| `ReplaceCallWithBinaryOperator` | info | Explicit 'plus'/'minus'/... operator call instead of the operator |
| `ControlFlowWithEmptyBody` | info | 'if', 'else', 'for' or 'while' with an empty body |

**Probable bugs** (`bugs`)

| Rule | Default | Flags |
|---|---|---|
| `ConvertNaNEquality` | warning | Comparison with NaN using '==' (always false) |
| `SelfAssignment` | warning | Variable assigned to itself |
| `EqualsOrHashCode` | warning | Class overrides only one of 'equals()' and 'hashCode()' |
| `ArrayInDataClass` | warning | Array property in a data class without custom 'equals()'/'hashCode()' |
| `ThrowableNotThrown` | warning | Exception created but neither thrown nor used |

**Naming** (`naming`)

| Rule | Default | Flags |
|---|---|---|
| `ClassName` | info | Class, object and type alias names are UpperCamelCase |
| `FunctionName` | info | Function names are lowerCamelCase |
| `PropertyName` | info | Property names are lowerCamelCase (constants may be SCREAMING_CASE) |
| `ConstPropertyName` | info | 'const val' names are SCREAMING_SNAKE_CASE |
| `LocalVariableName` | info | Local variable names are lowerCamelCase |
| `EnumEntryName` | info | Enum entries are UPPER_CASE or UpperCamelCase |
| `PackageName` | info | Package names are lowercase |
| `PackageDirectoryMismatch` | info | Package does not match the directory |

## How it works

```
src/
  syntax/      lexer (modal, for string templates) → event parser → rowan CST; typed AST views
  project/     build import
    gradle/    tolerant Groovy/Kotlin DSL parser + interpreter (settings, plugins, catalogs,
               allprojects/subprojects, apply from, buildSrc convention plugins, KMP source sets),
               optional Gradle init-script import
    maven.rs   reactor, effective POMs (pom.rs: parents, profiles, interpolation, BOM imports)
    resolver.rs transitive resolution: Maven nearest-wins + scope mediation, Gradle highest-wins,
               Gradle Module Metadata variants/available-at/constraints, exclusions, ranges
    repo.rs    ~/.m2, Gradle cache, file: repos, downloads (mirrors, server credentials)
  classpath/   class file reader, generic signatures, JDK jimage reader, Kotlin metadata
               protobuf decoding (+ .kotlin_builtins), lazy per-class/per-package index
  analysis/    file summaries (Kotlin + Java), source index, name resolution, diagnostics,
               snapshot-based database (AnalysisHost → Snapshot)
    ide/       expression typing, completion, definition, hover, references, rename, signature
               help, inlay hints, semantic tokens, imports, create from usage, hierarchies,
               folding/selection ranges
    lints/     lint rules with quick fixes and suppression
  server/      LSP main loop, document sync, scheduling/debouncing, cancellation, progress
```

* **Dependencies without running the build.** Build scripts are interpreted statically and
  dependencies are resolved natively against Maven repositories, so opening a project never
  executes its code. With `allowBuildToolExecution`, Gradle itself reports the exact classpaths.
* **Private repositories.** Credentials come from what the build already uses: Gradle
  `credentials { username = …; password = … }` blocks (values from `gradle.properties`,
  `ORG_GRADLE_PROJECT_*` or environment variables), `credentials(PasswordCredentials::class)` /
  `credentials(HttpHeaderCredentials::class)` with the repository `name` convention, and Maven
  `<server>` entries in `~/.m2/settings.xml` (matched by repository or mirror id; `${env.X}` works,
  encrypted passwords don't). Credentials are only sent to their repository's host and are never
  written to the model cache.
* **Laziness.** Indexing a classpath only reads zip directories and jimage tables; class files are
  decoded on first use. Identical classpaths are shared between modules.
* **No false alarms.** Semantic diagnostics run only for modules whose dependencies resolved
  completely (and with a JDK and the Kotlin stdlib present); otherwise files get syntax checks
  and the reason is reported once.

## Validation

* Parser: all 1,272 `.kt` files of the Kotlin stdlib/reflect/test/js/metadata source jars parse
  without errors (~20 MB/s); every prefix of a sample file parses without panics.
* Pathological input: files nested 3,000 levels deep in each way the grammar nests (brackets,
  operator and call chains, lambdas, blocks, classes, generic types, string templates) are
  diagnosed and answered in under 1.5 s each, without exhausting the stack.
* Dependency resolution matches the real tools exactly: a Spring Boot Maven project
  (73 artifacts) against `mvn dependency:list`, and a Ktor + Multiplatform Gradle project
  (49 artifacts) against Gradle 9.8.1.
* Kotlin metadata: every class and top-level declaration of a 44-jar Ktor classpath plus the JDK
  (19.7k classes, 226k members) decodes with no unresolved types.
* Diagnostics on real projects (Exposed, Javalin, Moshi, spring-petclinic-kotlin, kotlinx-cli;
  1,151 Kotlin files): zero false reports.

```sh
cargo test                                   # unit + end-to-end LSP tests
cargo run --release --example parse_corpus   -- <dir-or-sources.jar>
cargo run --release --example import_project -- <project> [--deps] [--tooling] [--offline]
cargo run --release --example index_classpath -- <project> [fq.Name ...]
cargo run --release --example check_project  -- <project>
cargo run --release --example ide_bench      -- <project> [class.Fq ...]   # time IDE requests
```
