# Contributing

Keep the backend single-purpose: ChatGPT integration through the official OpenAI tunnel client, one durable host scope root, and the official Codex App Server API.

Validate changes with:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

Do not add tunnel supervision, tunnel credentials, active-directory state, profiles, or abandoned historical paths to Codex Connect.
