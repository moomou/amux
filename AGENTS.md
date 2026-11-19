# amux Repository Guidelines

## Versioning & Releases
- Always bump the minor version (`0.X.0`) in `Cargo.toml` when implementing features or bug fixes.
- Keep `Cargo.lock` in sync (`cargo test` / `cargo build --release`), run `cargo fmt --check`, `cargo clippy --release --all-targets -- -D warnings`, and sequential `cargo test -- --test-threads=1` before pushing.
- When publishing:
  1. Push commits and tag `v0.X.0` to `origin/main` (`https://github.com/moomou/amux`).
  2. Verify GitHub Actions CI passes on `main`.
  3. Build `target/release/amux`, package `amux-0.X.0-x86_64-unknown-linux-gnu.tar.gz`, and publish the GitHub Release (`gh release create v0.X.0 ...`) with release notes and SHA-256 checksum.
  4. Refresh the local installation with `cargo install --path /home/moomou/dev/amux` and roll any running `amux` server onto the new binary with `amux restart-server`.

## Safety & Resource Constraints
- Never touch or attach to a user's production `tmux` server (`/tmp/tmux-$UID/default`); all `tmux` tests must use isolated `-S <tempdir>/...sock` sockets with `TMUX` and `TMUX_PANE` unset.
- Run builds and test suites sequentially (`--test-threads=1` when running e2e suites) to keep memory and CPU bounded.
