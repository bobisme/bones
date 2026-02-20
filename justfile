set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

# Run all pre-release checks (same gates as CI release pipeline)
check:
    cargo fmt --all -- --check
    cargo clippy --workspace -- -D warnings
    cargo test --workspace

install:
    cargo install --path crates/bones-cli

completions:
    mkdir -p ~/.local/share/bash-completion/completions
    mkdir -p ~/.zfunc
    mkdir -p ~/.config/fish/completions
    cargo run -p bones-cli -- completions bash > ~/.local/share/bash-completion/completions/bn
    cargo run -p bones-cli -- completions zsh > ~/.zfunc/_bn
    cargo run -p bones-cli -- completions fish > ~/.config/fish/completions/bn.fish
