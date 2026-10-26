# peloton-relay

A small, read-only HTTP relay for a single Peloton account. The relay holds the
Peloton credential and exposes a handful of typed `GET` endpoints, so that
agents, scripts and dashboards can read workout data without ever seeing the
credential.

The server is one static binary that runs as an unprivileged user in a
`FROM scratch` container image.

## API

All responses are JSON. The OpenAPI 3.1 description is served at `/openapi.json`.

| Route | Description |
|-------|-------------|
| `GET /v1/me` | Profile of the authenticated user. |
| `GET /v1/workouts` | A page of workouts. Query: `page` (from 0), `limit` (1 to 100, default 20), `from` and `to` (RFC 3339 instants). |
| `GET /v1/workouts/{id}` | One workout. |
| `GET /v1/workouts/{id}/metrics` | Performance samples of a workout. Query: `every_n` (sample spacing in seconds, 1 to 60, default 1). |
| `GET /v1/classes/{id}` | Details of a class (ride). |
| `GET /openapi.json` | The OpenAPI description. |
| `GET /healthz` | Liveness probe. |
| `GET /readyz` | Readiness: a usable Peloton credential is held. |

Ids are 32 lowercase hexadecimal characters. Unknown query parameters are
rejected.

Errors have the body `{"error": "<code>", "message": "<text>"}`:

| Status | `error` | Meaning |
|--------|---------|---------|
| 400 | `invalid_input` | A path or query value is malformed. |
| 404 | `not_found` | The resource, or the route, does not exist. |
| 405 | `method_not_allowed` | The route exists but only answers `GET`. |
| 502 | `upstream_error` | Peloton failed or returned something unexpected. |
| 503 | `not_authenticated` | The relay holds no usable Peloton credential; see [Bootstrap](#bootstrap). |
| 503 | `upstream_rate_limited` | Peloton is rate limiting the relay; a `Retry-After` header is set. |

## Security model

- **Read-only toward Peloton.** The relay only calls `GET /me`,
  `GET /user/{id}/workouts`, `GET /workout/{id}`,
  `GET /workout/{id}/performance_graph` and `GET /ride/{id}/details`, plus the
  token refresh (`POST /oauth/token`). It has no code path that changes anything
  on the account.
- **The credential stays in the relay.** Tokens live in a state file inside the
  relay's state directory, are refreshed by the relay, and are never returned by
  the API, written to logs or error messages, or passed on a command line.
- **No authentication of its own.** The API has no login. Anyone who can reach
  the listening port can read the account's workout data, so bind it to a
  private interface or put it behind a network boundary or authenticating proxy.
  Do not expose it to the internet.
- **Minimal image.** Static musl binary, no shell, no package manager, no CA
  bundle (TLS trust anchors are compiled in via rustls and `webpki-roots`),
  running as UID/GID 65532.

## Running

The image is published as `ghcr.io/pecheriere/peloton-relay`.

```sh
docker run -d --name peloton-relay \
  -p 127.0.0.1:8080:8080 \
  -v peloton-relay-state:/state \
  ghcr.io/pecheriere/peloton-relay:<version>
```

Pin a version tag, or better its digest. Release images carry a build
provenance attestation:

```sh
gh attestation verify oci://ghcr.io/pecheriere/peloton-relay:<version> --owner pecheriere
```

The container answers `peloton-relay healthcheck` (exit 0 when the local server
is alive, 1 otherwise), which suits a Compose or Kubernetes health check; the
image defines none itself.

## Bootstrap

The relay needs a Peloton token set before `/readyz` turns ready. The login
helper obtains one through the real Peloton web login (so passwords and 2FA are
handled by Peloton's own page) and prints it as JSON on stdout. It is a macOS
tool and is not part of the image. On a Mac:

```sh
cargo run --release -p peloton-relay-login | \
  ssh <host> sudo docker exec -i <container> peloton-relay seed
```

`seed` reads the token set from stdin, verifies it with Peloton and stores it in
the state directory. The tokens never touch a command line or a disk outside the
state directory. Run `peloton-relay-login check` for a login that only reports
non-secret facts. After seeding, the relay refreshes the credential on its own;
repeat the bootstrap only if Peloton invalidates the refresh token.

## Configuration

| Variable | Default | Meaning |
|----------|---------|---------|
| `PELOTON_RELAY_LISTEN` | `0.0.0.0:8080` | Socket address the server binds. |
| `PELOTON_RELAY_STATE_DIR` | `/state` | Directory holding `state.json`; must be writable by the container user. |

Logs are JSON lines on stderr. A failed Peloton authentication is logged once
per transition into the failed state as `event="peloton_auth_failed"`.

## Building

```sh
cargo build --release -p peloton-relay      # the server
docker build -t peloton-relay .             # the image
```

CI runs `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`, and
`cargo deny check`. No test or CI job talks to Peloton.

## Caveat

Peloton has no public API. This project uses the same private, undocumented API
as Peloton's own web app, signs in the way the web app does, and may stop
working at any time. Using it may be against Peloton's Terms of Service. It is
meant for reading your own account's data; you are responsible for how you use
it. This project is not affiliated with or endorsed by Peloton.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
