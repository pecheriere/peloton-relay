# AGENTS.md

Guidance for coding agents working in this repository, either changing the
code or helping someone deploy the relay. The [README](README.md) is the
reference for behaviour, configuration and the security model; this file only
points into it and lists the rules a change must follow.

## Layout

| Crate | What it is |
|---|---|
| `crates/peloton` | Shared Peloton OAuth, identifiers and API types. |
| `crates/peloton-relay` | The relay server (`serve`, `seed`, `healthcheck`) and its admin page. |
| `crates/peloton-relay-login` | macOS login helper (embedded web view) that obtains a token set. Not in the image. |
| `crates/peloton-relay-test-support` | Helpers for the relay's integration tests; a dev-dependency only. |

`docs/design/` holds the design language (tokens, mascot, screenshots) for the
pages the relay serves. `.github/scripts/crate_age.py` is the dependency age
check.

## Helping someone deploy it

Walk them through the README in this order:
[Running](README.md#running), [Accounts](README.md#accounts),
[Bootstrap](README.md#bootstrap), [Configuration](README.md#configuration) and
[Admin page](README.md#admin-page). Make sure they understand the
[Security model](README.md#security-model) before anything is reachable:

- The relay has no authentication of its own. It trusts `X-Peloton-Account`
  and `X-Peloton-Admin`, so it must sit behind a proxy that sets those headers
  from the caller's authenticated identity and drops any copy the client sent.
  Never publish its port directly, and never on the internet.
- Grant reads (`GET`) and writes (`/v1/schedule*`, `/v1/stack*`) as separate
  permissions in that proxy, and the admin header to the operator only.
- Pin the image by digest and verify its attestation (see Running).
- Tokens reach the relay only on stdin (`seed`) or through the admin page's
  one-time code. Never put a token in an argument, an environment variable, a
  file outside the state directory, a log or a chat transcript.
- `PELOTON_RELAY_PUBLIC_URL` must be exactly the origin the admin page is
  opened at, or the page's forms are refused (403 `csrf_rejected`).

## Changing the code

Before you report a change as done, run what CI runs; all of it must pass:

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --exclude peloton-relay-login --all-targets --all-features -- -D warnings
cargo test --locked --workspace --exclude peloton-relay-login --all-features
cargo deny --all-features --locked check
uvx zizmor@1.31.1 .   # when a workflow or the Dependabot config changes
# on macOS, for the login helper:
cargo clippy --locked -p peloton-relay-login --all-targets --all-features -- -D warnings
cargo test --locked -p peloton-relay-login --all-features
```

Rules:

- **Warnings are errors.** Clippy pedantic is on and `unsafe_code` is
  forbidden. Do not silence a lint; if one truly does not apply, lift it on
  the smallest item with `#[expect(...)]` or `#[allow(...)]` and a comment or
  `reason` that says why.
- **Tests first.** Every behaviour change comes with a test. No test or CI job
  talks to Peloton: mock upstream with `wiremock` and reuse the helpers in
  `crates/peloton-relay-test-support`.
- **HTTP clients** are built only with `peloton::tls::client_builder()`;
  `clippy.toml` forbids the reqwest constructors.
- **No new upstream calls without a reason.** The relay talks to a fixed set
  of Peloton operations (Security model). Adding one is a design change: say
  why in the pull request, and never add a generic pass-through.
- **Secrets and personal data never leave the relay.** No token, password,
  username or full Peloton user id in logs, errors or responses (logs carry the
  last four hex digits only). Login captures (`captures/`) are unscrubbed and
  personal; never commit them.
- **Dependencies.** A crate version must have been published at least 7 days
  before it lands in `Cargo.lock`, except the oldest release that fixes a
  security advisory (RUSTSEC or GHSA), listed in `.github/crate-age-allow.toml` (see
  [Dependency age](README.md#dependency-age)). Commit the updated `Cargo.lock`.
  GitHub Actions are pinned to a full commit SHA of a release at least 7 days
  old.
- **No dead code or TODO comments.** Delete what is unused; track open work as
  an issue.
- **Documentation moves with the code.** A change to an endpoint, a header, a
  log event or a variable updates the README in the same pull request.

## Commits and pull requests

- Commit subjects are plain imperative sentences, capitalised, with no prefix
  (`Refuse crate versions published less than 7 days ago`).
- Commits are signed, and `main` is changed only through pull requests that
  are squash-merged after CI passes.
- This is a public repository. Examples use the account names `alice`, `bob`
  and `carol` and the origin `https://relay.example.net`; never put real
  names, accounts, hostnames, ids or captured responses in code, tests, docs or
  commit messages.
- Releases are cut by the maintainer from a signed `v*` tag on `main`.
