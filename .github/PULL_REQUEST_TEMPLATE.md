## Summary

Brief description of changes.

## Type

- [ ] Bug fix
- [ ] New feature
- [ ] Enhancement
- [ ] Refactoring
- [ ] Documentation
- [ ] CI/Build

## Checklist

- [ ] `cargo test` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean (not `--all-features` — that reaches `metal`, which pulls `objc2` and doesn't build on Linux; see CONTRIBUTING.md)
- [ ] `cargo fmt --all -- --check` passes
- [ ] Commit messages follow conventional commits (`type(scope): description`)
- [ ] Added/updated tests for new functionality (if applicable)

## Testing

How was this tested? What commands did you run?

## Related Issues

Closes #
