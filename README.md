# Java Analyzer

Extreme fast Java LSP, built in Rust

## Feature Matrix

[JLS Implement Status (AI generated)](docs/jls-implementation-status.md)

- Analyze Jar, Codebase and JDK builtins
- Code completion
- Symbols List (Outline)
- Goto definition
- Inlay hints
  - Inferred type on `var`
  - Parameter names
- Decompiler support (Vineflower, cfr)
- Treesitter based syntax highlight (semantic_tokens handler)
- Java 8 to 25 support
- Gradle 4.0 to 9.x support
- Maven 3.0+ support

## FAQ

- Is this a real LSP?
  YES
- Is this production ready?
  Probably yes, but not everything in JLS implemented perfectly yet.

## Development

- [The Treesitter inspector script](https://gist.github.com/cubewhy/7a43196d323488db4c4053f1c5126f9f)
- [Treesitter playground](https://tree-sitter.github.io/tree-sitter/7-playground.html)

## License

This work is licensed under GPL-3.0 license.

You're allowed to

- use
- share
- modify
