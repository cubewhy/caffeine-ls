# Adding a language

Every layer of the workspace answers per _file kind_, and each layer owns one
registry: a language is an identity plus one registration per layer, and a
feature dispatches through the registration instead of through a language name.
Nothing below `caffeine-ls` (the LSP handlers, the CLI, the extension
allowlists) names a language — they all read the registries — so adding one is
a bounded, mechanical change.

The model this follows is IntelliJ's: a `Language` plus its per-feature
extension points, with the JVM-shaped part of the model (classes, members,
classfiles) living in a shared layer that every JVM language plugs into.

## What a new language costs

1. **A parser crate** — `crates/<lang>-syntax`, a workspace member in the root
   `Cargo.toml` and a dependency of `crates/syntax/Cargo.toml`. It exposes the
   language's `SourceFile`, its `Parse<T>` and its error kinds, as
   `crates/java-syntax` and `crates/kotlin-syntax` do.

2. **`crates/syntax/src/lang/<lang>.rs`** — one `impl LanguageSyntax`
   ([`crates/syntax/src/lang.rs`](../crates/syntax/src/lang.rs)): the kinds it
   answers for, the file extensions it owns _with the kind each names_
   (most specific first), its LSP `languageId`, the name snapshots spell it
   with, and how it parses. Register it in the table.

3. **`crates/hir-def/src/<lang>/`** — the declaration model and its lowering,
   plus `crates/hir-def/src/<lang>/plugin.rs` with the two registrations
   ([`crates/hir-def/src/lang.rs`](../crates/hir-def/src/lang.rs)):
   `impl LangLowering` (how the file's text becomes a model) and
   `impl Declarations` + the wrapper the facade holds, and the typed accessor
   that language's own layers read the model with. The shared declaration IR —
   a type reference, a formal parameter, an annotation application — is already
   in the JVM layer ([`crates/hir-def/src/jvm/decl.rs`](../crates/hir-def/src/jvm/decl.rs));
   lower into it rather than declaring your own.

4. **`crates/hir-ty/src/<lang>/`** — the type layer, plus
   `crates/hir-ty/src/<lang>/plugin.rs` with
   ([`crates/hir-ty/src/lang.rs`](../crates/hir-ty/src/lang.rs)):
   - `impl JvmMemberSource` — the JVM-visible members a class of this language
     declares: the methods and fields named `name`, and the abstract members a
     functional-interface test sees. This is what every other language reaches
     a class of yours through, so it must answer in classfile shapes (what a
     compiler would emit), not in source shapes.
   - `impl LanguageTypes` — `ty_from_jvm` and `ty_to_jvm`, the two directions
     of the codec between your types and the JVM's (`ty_from_jvm` is where
     platform types, if your language has them, are introduced), plus your
     supertypes, your call sites' access context, and the file dependency
     index.

5. **`crates/hir/src/<lang>/`** — `impl LanguageFileIndex`
   ([`crates/hir/src/lang.rs`](../crates/hir/src/lang.rs)): the symbols the
   file contributes to the workspace index, its doc comments, its package and
   the facade class a compiler synthesizes for its top-level declarations.

6. **`crates/ide/src/<lang>/`** — `impl LanguageIde`
   ([`crates/ide/src/lang.rs`](../crates/ide/src/lang.rs)): definition,
   references, hover and its documentation, the outline, highlighting and
   inlay hints. Each method forwards to the module that implements the
   feature for this language, as `crates/ide/src/java/plugin.rs` does.

7. **`crates/ide-diagnostics/src/<lang>/`** — `impl LanguageDiagnostics`
   ([`crates/ide-diagnostics/src/lang.rs`](../crates/ide-diagnostics/src/lang.rs),
   crate-private because the sink is): the body/type findings the file reports,
   its declaration-level findings, and the `@SuppressWarnings` scopes in force.
   A language with none of these answers the trait's defaults rather than
   declaring an empty implementation.

8. **Two lines of salsa** — `DefDatabase` (`crates/hir-def/src/db.rs`) and
   `HirDatabase` (`crates/hir/src/db.rs`) gain `+ <Lang>Database`: the empty
   per-language marker trait your tracked queries hang off, mirroring
   `JavaDatabase`/`KotlinDatabase`. This is what lets a language add queries
   without touching another language's.

9. **`project-model`** — the build systems' source-directory unions gain
   `src/main/<lang>` (and the test root), so a project of this language is
   found. See `crates/project-model/src/gradle/model.rs`.

Nothing else: no feature code, no LSP handler, no extension allowlist (the
server reads `syntax::lang::file_extensions`), no diagnostics code.

## The invariants a port must keep

1. `LanguageKind` appears only in `crates/syntax/src/{language.rs,lang.rs,lang/**}`,
   in `base-db`'s classification query, and inside a language's own module
   (`crates/<crate>/src/<language>/**`) — never in a language-agnostic module.
2. No language-agnostic module names a concrete language: no `as_java()`,
   `is_kotlin`, `java::…` in a shared file. A shared module reaches a language
   through a registry, keyed by the target's kind or by the target's file.
3. A language reaches another language only through a registry: a cross-language
   lookup asks the _target's_ layer (`for_file(db, target_file)`), and a cast to
   a concrete declaration model is legal only inside that model's own module.
4. A shared _data_ enum may still carry one variant per language
   (`DiagnosticCode::{Java, Kotlin}`, `SourceSymbolKind`, the body IR's
   `ExprData`). What the rule forbids is per-language _dispatch_ in shared code,
   not per-language data.
5. The JVM-shaped model belongs to the JVM layer: a language's declaration and
   type layers plug into `hir-def::jvm` / `hir-ty::jvm` (the member vocabulary,
   the member set, the classfile-shaped enumeration) rather than re-implementing
   it, and a language's own concepts stay in that language's modules.

`crates/ide/tests/language_registration.rs` asserts the registries agree on
every kind, so a half-added language — registered in one layer and forgotten in
another, which would answer _nothing_ rather than failing — fails the test with
the layer and the kind.
