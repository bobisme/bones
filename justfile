set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

default:
  just --list

# Run all pre-release checks (same gates as CI release pipeline)
check:
    cargo fmt --all -- --check
    rtk cargo clippy --workspace -- -D warnings
    rtk cargo test --workspace

install:
    rtk cargo install --locked --path crates/bones-cli

completions:
    mkdir -p ~/.local/share/bash-completion/completions
    mkdir -p ~/.zfunc
    mkdir -p ~/.config/fish/completions
    cargo run -p bones-cli -- completions bash > ~/.local/share/bash-completion/completions/bn
    cargo run -p bones-cli -- completions zsh > ~/.zfunc/_bn
    cargo run -p bones-cli -- completions fish > ~/.config/fish/completions/bn.fish

# Run all cargo-fuzz targets with nightly.
# Usage: just fuzz [seconds]
fuzz seconds="60":
    cargo +nightly fuzz --help >/dev/null
    cd fuzz && cargo +nightly fuzz run parse_line -- -max_total_time={{seconds}}
    cd fuzz && cargo +nightly fuzz run replay_state -- -max_total_time={{seconds}}
    cd fuzz && cargo +nightly fuzz run project_event -- -max_total_time={{seconds}}
