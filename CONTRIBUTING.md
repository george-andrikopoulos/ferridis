# Contributing to Ferridis

Ferridis is designed in public. Issues, discussion threads, and pull requests are welcome from anyone who has felt the friction of integrating AI with the world.

## Project bias

When in doubt, prefer:

- **Simpler over more capable.** Adding a feature is a cost.
- **Web-native over invented.** If OAuth, OpenAPI, AsyncAPI, or JSON Schema already covers it, use them.
- **Lazy over eager.** Nothing should bloat the model's always-loaded surface without justification.
- **Open over closed.** No private extensions, no proprietary spec dialects.
- **Attributed over silent.** Capability publishers, contributors, and design authors keep their names on the work.

## Code conventions

The Rust code is type-driven by design. Two patterns are non-negotiable:

1. **Typestate** for lifecycle-bearing types — state transitions consume `self`.
2. **Illegal states unrepresentable** for value types — validation happens at construction, not at use.

Hard rules in the workspace:

- `#![forbid(unsafe_code)]` everywhere. No exceptions.
- `#![deny(missing_docs)]` on every public crate.
- All public APIs documented with rustdoc.
- Tests demonstrate the discipline working — especially the "this would be a compile error" cases.

Run before submitting (there is no hosted CI, so this is the gate):

```bash
bash scripts/check.sh   # cargo fmt --check, clippy on all targets with -D warnings, cargo test
```

If rustfmt complains, `cd rust && cargo fmt --all` fixes it. Run the gate on Linux and on Windows if you can reach both: some breaks show on only one of them.

## What changes are most welcome

- Resolving items in the "Open questions" section of [architecture.md](./architecture.md).
- Adding reference adapters that exercise the protocol against real services.
- Tightening the type-driven discipline — the constructor invariants and error variant coverage are the places to look.
- Improving documentation, especially the "About the name" section if you're a former student of Alexandros Ferridis.

## What to avoid

- Adding capabilities that duplicate something the web stack already provides.
- Introducing a new spec dialect when OpenAPI or AsyncAPI fits.
- Stripping attribution from any artifact.

## How to discuss

Open an issue before opening a large PR. Design questions are not resolved by the first patch — they are resolved by argument, written down, and then implemented. The `architecture-frictions.md` document is the model for that pattern.

## Code of conduct

Be kind, be specific, push back where it matters. The protocol is named for a teacher whose influence shaped the project's values; act in a way he'd recognise.
