# kotlin-lsp-rust

A Kotlin language server written in Rust. This is the core infrastructure: protocol
handling, an error-tolerant parser, Gradle/Maven import with native dependency
resolution, binary classpath indexing (jars, the JDK image, Kotlin metadata), a
module-aware declaration index, and import/type resolution with diagnostics.

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

Commands (`workspace/executeCommand`): `kotlin.reloadProject`, `kotlin.projectModel`
(returns the imported model as JSON: modules, source roots, resolved artifacts, problems).

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
