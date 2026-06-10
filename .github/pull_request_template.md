<!--
Thanks for contributing to the Agent Buddy gateway!
Note: `bridge/`, `packaging/`, and `install.*` are mirrored from a private
monorepo. Accepted PRs are back-ported there (you keep authorship credit) and
sync back here on the next release — so your PR may be closed with a reference to
the merge commit rather than showing a "merged" badge. See CONTRIBUTING.md.
-->

## What & why

<!-- What does this change and why? Link any related issue. -->

## Checklist

- [ ] Changes are limited to `bridge/`, `packaging/`, or `install.*` (firmware is
      closed-source and not in this repo).
- [ ] `cd bridge && cargo check --features gui` passes.
- [ ] `cd bridge && cargo test` passes.
- [ ] `cargo fmt` clean.
- [ ] For a large/design change, I opened an issue first.
