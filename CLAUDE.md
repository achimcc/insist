# CLAUDE.md

Working rules for this repository. `README.md` says what the program does,
`docs/design.md` why it is shaped the way it is.

## Language

**This repository is English** — identifiers, comments, commit messages,
README. Notification texts are configuration, not code.

## The test cycle

1. **Write the test first** and see it red for the reason you expect.
2. **Implement the smallest thing that passes.**
3. **Run all three, every time** (inside `nix develop`): `cargo test`,
   `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`.
4. **Commit**, staging files by name.

`nix flake check` builds the package, runs clippy, fmt and the VM test; run
it before tagging.

## Rules that hold everywhere

- **This program must never make the alert path weaker.** Mail does not go
  through insist. An error is never "nothing is firing". When in doubt,
  be loud.
- **Ask for the result, not the exit code.** A test that asserts only a
  status code proves nothing about state.
- **No secret in an error, a log line, `argv` or a `Debug` dump.** Tokens,
  the HMAC key, topic names and the watchdog URL live in `Secret`.
- **Field names come from a recorded answer**, never from memory.
  Fixtures under `fixtures/<program>-<version>/` carry a `SOURCE.md`;
  anything built by hand is named `constructed-…`.
- **Tests that build their own payload agree with the code and with nobody
  else.** Prefer the recorded file.
