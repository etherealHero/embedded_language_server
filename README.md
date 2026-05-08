# Embedded language server

Embedded language server that provides [LSP](https://microsoft.github.io/language-server-protocol/) features powered by [tiberius](https://github.com/prisma/tiberius) data interface.

## Usage

Download prebuilt binaries from release page

```sh
$ embedded_language_server --help
Usage: embedded_language_server [OPTIONS] [COMMAND]

Commands:
  lsp            Start language server service (stdio transport)
  sign           Sign config file
  sample-config  Print sample config to stdout
  help           Print this message or the help of the given subcommand(s)

Options:
      --debug  Enable debug level logging
  -h, --help   Print help

# emit tests/sample_config.toml into my_custom_config.toml:
$ embedded_language_server sample-config >> my_custom_config.toml

# language client should start language server with following arguments:
$ embedded_language_server lsp my_custom_config.toml
INFO embedded_language_server: resolved config: my_custom_config.toml
INFO embedded_language_server: start service...
INFO embedded_language_server: service is running

# you should manual sign config file after change get_symbols_query option:
$ embedded_language_server sign my_custom_config.toml
sign config successfull!

# standalone binary (tests/sample_config.toml built in binary yet)
$ standalone lsp
INFO standalone: start service...
INFO standalone: service is running
```

## Build from source

[Rust](https://rust-lang.org/) requirement

```sh
$ git clone <repo_url>
$ cd embedded_language_server/
$ echo "SECRET=YOUR_SECRET_KEY" >> .env

# ... manual edit tests/sample_config.toml, adapt tests/unit_test.rs, then sign new config ...
$ cargo run --bin embedded_language_server -- sign tests/sample_config.toml

# check tests pass
$ cargo test

# build artifacts then take binaries from target/release/[embedded_language_server|standalone]
$ cargo build --release
$ ./target/release/embedded_language_server
```

## Text editor integration plugins

- [Generic LSP Proxy (VS Code)](https://github.com/mjmorales/vscode-generic-lsp-proxy)
- [LSP4IJ (IntelliJ)](https://github.com/redhat-developer/lsp4ij)
- [LSP (Sublime Text)](https://github.com/sublimelsp/LSP/)
- [nvim-lspconfig (Neovim)](https://github.com/neovim/nvim-lspconfig)
