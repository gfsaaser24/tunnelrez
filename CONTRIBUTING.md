# Contributing

Thanks for helping. Everything you need is in [`AGENTS.md`](AGENTS.md); it applies to people and AI agents alike.

Short version:

1. Branch from `dev` and open the pull request into `dev`.
2. Build, test and lint every crate you touched (`cargo test`, `cargo clippy --all-targets -- -D warnings`,
   `cargo fmt --check`; the TV crate also for `armv7-linux-androideabi`). The toolchain is pinned in
   `rust-toolchain.toml`.
3. A claim about speed or latency needs a run-book entry from a real TV (`docs/runbook/`).
4. No personal data in code, docs, tests or screenshots: use documentation addresses (`192.0.2.x`).
5. Pinned versions ([`docs/PINNED.md`](docs/PINNED.md)) change only in their own pull request, with the reason.

Security problems: see [`SECURITY.md`](SECURITY.md), not a public issue.

The project is licensed under the [PolyForm Noncommercial License 1.0.0](https://polyformproject.org/licenses/noncommercial/1.0.0/).
See [`LICENSE`](LICENSE).
