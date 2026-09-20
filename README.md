# termlog

Local terminal recording for macOS and Linux. Saves asciicast v3 recordings and
searchable, timestamped text. Input capture is off by default; displayed secrets
are still recorded. Use `--capture-input` to include hidden input, or `--no-capture-input` to override configuration.

```sh
cargo install --locked --path .
termlog shell
termlog run -- fish -l
termlog list
termlog search 'pattern'
termlog show SESSION
termlog replay SESSION
```

Set your terminal startup command to the absolute path of `termlog` followed by
`shell`. Configuration: `$XDG_CONFIG_HOME/termlog/config.toml` (default
`~/.config/termlog/config.toml`). Recordings: `$XDG_STATE_HOME/termlog` (default
`~/.local/state/termlog`). `termlog --help` lists commands.

```sh
cargo test --locked
cargo build --locked && python3 tests/pty_integration.py
```
