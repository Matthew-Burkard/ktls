# kotlin-lsp-rust

A Kotlin language server written in Rust. This is the core infrastructure: protocol
handling, an error-tolerant parser, Gradle/Maven import with native dependency
resolution, binary classpath indexing (jars, the JDK image, Kotlin metadata), a
module-aware declaration index, import/type resolution with diagnostics, navigation
(definition, hover, references) and lint rules with quick fixes.

See [FEATURES.md](FEATURES.md) for the full feature catalog and what is implemented.

## Build and run

```sh
cargo build --release
./target/release/kotlin-lsp        # speaks LSP over stdio
KOTLIN_LSP_LOG=debug ./target/release/kotlin-lsp   # logs go to stderr
```

Neovim (0.11+):

```lua
vim.lsp.config('kotlin_rust', {
  cmd = { '/path/to/kotlin-lsp' },
  filetypes = { 'kotlin' },
  root_markers = { 'settings.gradle.kts', 'settings.gradle', 'build.gradle.kts', 'build.gradle', 'pom.xml' },
  init_options = { kotlin = { project = { allowBuildToolExecution = false } } },
})
vim.lsp.enable('kotlin_rust')
```

## Configuration

Passed as `initializationOptions` or via `workspace/configuration` (section `kotlin`):

| Setting | Default | Meaning |
|---|---|---|
| `jdkHome` | auto | JDK to index (otherwise `JAVA_HOME`, `java` on `PATH`, `/usr/lib/jvm`, SDKMAN, `~/.gradle/jdks`) |
| `cacheDir` | `$XDG_CACHE_HOME/kotlin-lsp-rust` | Downloaded artifacts and cached project models |
| `project.gradleMode` | `auto` | `static` (read build scripts), `tooling` (ask Gradle), `auto` (tooling when allowed and a wrapper exists) |
| `project.allowBuildToolExecution` | `false` | Allow running the project's `gradlew`. Off by default because it executes code from the project |
| `project.offline` | `false` | Use only local caches (`~/.m2`, Gradle's `files-2.1`, our cache) |
| `project.extraRepositories` | `[]` | Additional Maven repository URLs (`https://` or `file://`) |
| `diagnostics.enabled` / `diagnostics.debounceMs` | `true` / `200` | |
| `lint.enabled` | `true` | Run the lint rules below |
| `lint.rules` | `{}` | Level per rule id or category (see below) |

Commands (`workspace/executeCommand`): `kotlin.reloadProject`, `kotlin.projectModel`
(returns the imported model as JSON: modules, source roots, resolved artifacts, problems).

## Lint rules

IntelliJ-style inspections, reported as diagnostics with code = rule id. Most have a quick fix
(code action); every finding also offers *Suppress for declaration / file*. `source.fixAll`
applies all unused/redundant-code fixes in a file. Rules that need types only run when the
module's dependencies resolved, and every rule prefers silence over guessing.

Suppress with `@Suppress("RuleId")` (also `@file:Suppress`, the aliases such as `unused`, and
IntelliJ's `//noinspection RuleId` comment). Configure levels with `lint.rules`; keys are rule
ids, categories (`unused`, `redundancy`, `style`, `bugs`, `naming`) or `all`, values `error`,
`warning`, `info`, `hint`, `action` (no diagnostic, only a code action at the cursor) or `off`:

```lua
init_options = { kotlin = { lint = { rules = { naming = "off", UnusedImport = "error" } } } }
```

**Unused code** (`unused`)

| Rule | Default | Flags |
|---|---|---|
| `UnusedImport` | warning | Import directive that nothing in the file uses |
| `UNUSED_VARIABLE` | warning | Local variable that is never read |
| `UNUSED_PARAMETER` | warning | Parameter of a private or local function that is never used |
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
    repo.rs    ~/.m2, Gradle cache, file: repos, downloads (mirrors from settings.xml)
  classpath/   class file reader, generic signatures, JDK jimage reader, Kotlin metadata
               protobuf decoding (+ .kotlin_builtins), lazy per-class/per-package index
  analysis/    file summaries (Kotlin + Java), source index, name resolution, diagnostics,
               snapshot-based database (AnalysisHost → Snapshot)
    ide/       expression typing, definition, hover, references
    lints/     lint rules with quick fixes and suppression
  server/      LSP main loop, document sync, scheduling/debouncing, cancellation, progress
```

* **Dependencies without running the build.** Build scripts are interpreted statically and
  dependencies are resolved natively against Maven repositories, so opening a project never
  executes its code. With `allowBuildToolExecution`, Gradle itself reports the exact classpaths.
* **Laziness.** Indexing a classpath only reads zip directories and jimage tables; class files are
  decoded on first use. Identical classpaths are shared between modules.
* **No false alarms.** Semantic diagnostics run only for modules whose dependencies resolved
  completely (and with a JDK and the Kotlin stdlib present); otherwise files get syntax checks
  and the reason is reported once.

## Validation

* Parser: all 1,272 `.kt` files of the Kotlin stdlib/reflect/test/js/metadata source jars parse
  without errors (~20 MB/s); every prefix of a sample file parses without panics.
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
```
