# Project Overview

## What is java-analyzer?

java-analyzer is an extremely fast Java Language Server Protocol (LSP) implementation written in Rust. It is designed as a from-scratch, high-performance alternative to existing Java language servers (e.g., Eclipse JDT LS, IntelliJ-based servers), prioritizing startup speed, low memory footprint, and responsiveness across large codebases.

The server speaks standard LSP over stdio and integrates with any compliant editor. A VS Code extension is included under `editors/code/`.

## Goals

- **Speed**: Tree-sitter-based incremental parsing; parallel class-file indexing via Rayon; lock-free concurrent data structures (DashMap).
- **Correctness**: Faithful implementation of JLS rules for name resolution, type inference, overload selection, and generics substitution — within the scope described in [JLS Implementation Status](./jls-implementation-status.md).
- **Broad Java support**: Java 8 through Java 25, Gradle 4.0 through 9.x, JDK 8 (rt.jar) through JDK 21+ (jimage/modules).
- **Decompiler integration**: First-class support for navigating into decompiled bytecode via Vineflower or CFR.

## Supported LSP Features

| Feature | Status |
|---|---|
| Text document sync (incremental) | Implemented |
| Code completion | Implemented |
| Inlay hints (`var` type, parameter names) | Implemented |
| Go to definition | Implemented |
| Document symbols (outline) | Implemented |
| Semantic tokens (syntax highlight) | Implemented |
| Hover | Stub (returns `null`) |
| Diagnostics / error reporting | Not implemented |
| References / find usages | Not implemented |
| Rename | Not implemented |
| Code actions | Not implemented |
| Formatting | Not implemented |

## Language Support

| Language | Parsing | Completion | Semantic tokens |
|---|---|---|---|
| Java | Full (tree-sitter) | Full | Full |
| Kotlin | Partial (tree-sitter) | Not implemented | Partial |

## Build Tool Support

| Tool | Status |
|---|---|
| Gradle (legacy: 4.x–6.x) | Supported via init-script export |
| Gradle (modern: 7.x–9.x) | Supported via init-script export |
| Maven (3.0–3.5) | Supported via legacy groovy script |
| Maven (3.6+) | Supported via modern groovy script |
| Bare classpath / JDK only | Supported |

## Key Dependencies

| Crate | Purpose |
|---|---|
| `tower-lsp` | LSP server framework |
| `tokio` | Async runtime |
| `tree-sitter` / `tree-sitter-java` | Incremental parsing |
| `rust-asm` | JVM `.class` file parsing |
| `rayon` | Parallel indexing |
| `dashmap` | Concurrent hash maps |
| `ropey` | Rope-based document text storage |
| `nucleo` / `nucleo-matcher` | Fuzzy completion scoring |
| `zip` | JAR/ZIP reading |
| `jimage-rs` | JDK 9+ `lib/modules` (jimage) reading |
| `postcard` | Binary serialization for index cache |
| `indoc` | Multiline string helpers |
| `insta` | Snapshot testing |

## Repository Layout

```
java-analyzer/
├── src/
│   ├── main.rs                 Entry point; starts LSP server on stdio
│   ├── lib.rs                  Crate root; re-exports all public modules
│   ├── lsp/                    LSP transport layer (server, handlers, capabilities)
│   ├── language/               Language-specific logic
│   │   └── java/               Java parsing, completion context, inlay hints, symbols, flow
│   ├── semantic/               Type representation, type resolver, cursor context
│   ├── index/                  Class metadata storage, JDK/JAR/source indexing
│   ├── completion/             Engine, providers, fuzzy scoring, post-processing
│   ├── build_integration/      Gradle detection, model extraction, workspace reload
│   ├── workspace/              Document store, analysis context routing
│   ├── decompiler/             Vineflower and CFR decompiler backends + cache
│   └── jvm/                    JVM descriptor and constant-pool utilities
├── editors/code/               VS Code extension (TypeScript)
├── docs/                       Design and reference documentation
├── third_party/                Vendored test data (Vineflower)
├── Cargo.toml
├── flake.nix                   Nix dev environment
└── .github/workflows/          CI (rust-ci.yaml)
```
