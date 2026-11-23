## What changed

Describe the capability and the user-visible result.

## Why

Link the issue or explain the problem this solves.

## Verification

Paste the commands you ran and their outcome. For document or protocol changes,
include the regenerated schema check.

```sh
cargo +nightly fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
bash scripts/gates.sh
```

## Checklist

- [ ] Every commit is English and carries no step or milestone identifier.
- [ ] New dependencies are permissively licensed and pass `cargo deny check`.
- [ ] Generated `schema/` is up to date.
- [ ] No company names, internal hostnames, credentials or real customer data.
- [ ] Public contract or capability changes are reflected in the documentation.
