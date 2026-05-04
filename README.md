# Caffeine-ls

[The old LSP (tree-sitter based)](https://github.com/cubewhy/caffeine-ls/tree/legacy)

The next-gen LSP for JVM family languages

## Contribute

The development of the LSP is in very early stage, please contribute!

## Development

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

If you want to inspect a tree in a folder:

```sh
cargo xtask batch-parse path/to/folder -o output/
```

## License

This project is licensed under GPL-3.0,

Files under lib/rust-sam licensed under MIT.
