# Caffeine-ls

[The old LSP (tree-sitter based)](https://github.com/cubewhy/caffeine-ls/tree/legacy)

The next-gen LSP for JVM family languages

## Usage

### Language server

Editors launch the binary without arguments (or with `serve`); it communicates
over stdio using LSP.

```sh
caffeine-ls            # start the language server over stdio
caffeine-ls serve      # same thing, explicitly
```

### Headless diagnostics

`diagnostics` analyzes a repository without an editor by driving the same
server lifecycle an IDE would (workspace probe → build-system sync → file
scan → library indexing), then pulling diagnostics for every source file.

```sh
# human-readable report on stdout
caffeine-ls diagnostics path/to/project

# JSON report for tooling, written to a file
caffeine-ls diagnostics . --format json -o report.json

# count warnings as findings too (see exit codes); every lint-level warning
# (raw types, unchecked conversions, deprecation) is reported, so this is how
# a report is narrowed to the errors
caffeine-ls diagnostics . --min-severity warning

# resolve ambiguous workspaces (e.g. gradle + maven files in one root)
caffeine-ls diagnostics . --build-system maven
```

Options:

| Flag | Description |
|---|---|
| `--format <text\|json>` | Report format (default `text`) |
| `-o, --output <FILE>` | Write the report to a file instead of stdout |
| `--min-severity <error\|warning\|all>` | Threshold for reported diagnostics and the exit code (default `error`) |
| `--build-system <gradle\|maven\|eclipse\|idea>` | Pick a build system when the layout is ambiguous |
| `--java-home <PATH>` | JDK used for workspace loading and library indexing |
| `--log-file <FILE>` | Also write server logs to a file |

Exit codes: `0` no findings at or above the threshold · `1` findings found ·
`2` analysis failed (bad path, no JDK, build-system sync failure, timeout).

### Library sources

A library element resolves to a classfile entry, which has no file and no
range, so go-to-definition and hover would have nothing to point at. The server
therefore attaches sources to libraries:

- the JDK's `lib/src.zip` (or `src.zip`);
- any `<stem>-sources.jar` sitting beside a classpath jar;
- the source archive the build system resolved, when it reports one.

Nothing is extracted or loaded eagerly. Each archive is indexed once by its
entry names (~1 MB for a JDK `src.zip`, against its ~25k compilation units and
250 MB of text), and a single file is read out of the archive into
`<cache_dir>/sources/v1/<library-id-hex>/` — and loaded into the database —
only when a navigation request resolves into it. A reference that lands in a
library makes the server materialize exactly the files it needs (the declaring
class plus the supertypes of a member call, usually under a dozen), answer, and
leave everything else inside the archive. The workspace sync reports the
preparation as an "Indexing library sources" progress step.

One caveat: the cache grows with what you navigate into — browsing thousands of
JDK classes materializes thousands of files under `<cache_dir>/sources`. The
directories of libraries that are no longer on the classpath are pruned on
every workspace load, so removing a dependency reclaims its space; there is no
eviction policy for files of libraries that are still present.

The classfile stays the source of truth: resolution, typing, generics, access
flags, deprecation and the `--release`/`ct.sym` view all come from the stub
index, and no source text ever feeds them. Sources contribute exactly the
location of a declaration and — when a classfile carries no `MethodParameters`
attribute — the parameter names in a hovered signature. Library sources are
read-only third-party code: they never appear in `workspace/diagnostic`.

The `caffeine_ls.downloadSources` setting (`download_sources` in the server's
`initializationOptions`) lets the build system fetch missing dependency sources
before the export: Gradle resolves each coordinate's `sources` classifier in a
detached configuration, Maven runs `dependency:sources`. With it off, only
archives already on disk are used. Changing it re-syncs the workspace.

### Headless parse

`parse` parses one source file, a folder of source files or stdin and reports,
per file, the detected language, the syntax-tree node count, the tree dump and
any syntax errors. It reuses the same parsers the server uses for Java and
Kotlin, so it works great as a dev/debugging tool and inside scripts.

Language is detected by file extension (`.java` → Java, `.kt`/`.kts` →
Kotlin). Stdin has no extension, so it requires `--language`.

```sh
# text report on stdout (tree dump + errors)
caffeine-ls parse path/to/File.java

# parse a folder, JSON report
caffeine-ls parse path/to/project --format json

# JSON Lines — one compact JSON object per parsed file, great for tools
caffeine-ls parse path/to/project --format jsonl

# pipe source code in with an explicit language (unix-pipe friendly)
echo 'class Hello {}' | caffeine-ls parse --language java --format jsonl

# stream and aggregate results line by line
caffeine-ls parse . --format jsonl | jq -s 'map({file, node_count})'
```

Options:

| Flag | Description |
|---|---|
| `<PATH>` | Source file or folder to parse; omit (or use `-`) to read from stdin |
| `--language <java\|kotlin>` | Language to parse with; required for stdin, overrides extension detection |
| `--format <text\|json\|jsonl>` | Report format (default `text`) |
| `-o, --output <FILE>` | Write the report to a file instead of stdout |

Exit codes: `0` every file parsed without syntax errors · `1` at least one
syntax error was found · `2` parsing failed (bad path, missing `--language`,
unreadable file).

## Contribute

The development of the LSP is in very early stage, please contribute!

## Development

### Requirements

- Rust toolchain
- JDK 1.8+ (25 recommended) for building the sidecar

### Run VSCode Extension

Run the VSCode extension development host with the following command

```sh
cargo xtask vscode
# or with custom cargo arguments
# cargo xtask vscode -- -r
```

If you want to inspect a tree:

```sh
cargo xtask parse path/to/file.java
```

If you want to inspect trees in a folder:

```sh
cargo xtask batch-parse path/to/folder -o output/
```

## License

This project is licensed under GPL-3.0,

Files under lib/rust-sam licensed under MIT.
