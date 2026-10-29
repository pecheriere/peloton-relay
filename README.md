# peloton-relay

A small HTTP relay for a single Peloton account. The relay holds the Peloton
credential and exposes a handful of typed endpoints, so that agents, scripts and
dashboards can read workout data, search the class library and plan rides
(schedule classes and manage the Stack) without ever seeing the credential.

The server is one static binary that runs as an unprivileged user in a
`FROM scratch` container image.

## API

All responses are JSON. The OpenAPI 3.1 description is served at `/openapi.json`;
it lists every parameter and enum value and walks through one example (find a
20-minute low-impact ride and schedule it for Monday 08:00).

### Reads

| Route | Description |
|-------|-------------|
| `GET /v1/me` | Profile of the authenticated user. |
| `GET /v1/workouts` | A page of workouts. Query: `page` (from 0), `limit` (1 to 100, default 20), `from` and `to` (RFC 3339 instants). |
| `GET /v1/workouts/{id}` | One workout of the account; another member's workout answers 404. |
| `GET /v1/workouts/{id}/metrics` | Performance samples of a workout of the account. Query: `every_n` (sample spacing in seconds, 1 to 60, default 1). |
| `GET /v1/classes/{id}` | Details of a class (ride). |
| `GET /v1/library` | A page of the on-demand class library, filtered and sorted by Peloton. Query: see [Library filters](#library-filters). |
| `GET /v1/library/filters` | The values `/v1/library` currently accepts for a `category` (required): categories, class types, instructors, durations and sorts, live from Peloton. |
| `GET /v1/schedule` | Everything on the account's schedule in a window. Query: `from` (default now) and `to` (default 14 days after `from`, at most 31 days after it). |
| `GET /v1/schedule/{scheduled_id}` | One scheduled class. |
| `GET /v1/stack` | The Stack (the queue of classes that plays on the bike) in play order, with class summaries and total time. |
| `GET /openapi.json` | The OpenAPI description. |
| `GET /healthz` | Liveness probe. |
| `GET /readyz` | Readiness: a usable Peloton credential is held. |

### Writes

| Route | Body | Description |
|-------|------|-------------|
| `POST /v1/schedule` | `{"class_id", "start_time"}` | Schedule an on-demand class; answers 201 with the scheduled class and a `Location` header. |
| `PATCH /v1/schedule/{scheduled_id}` | `{"start_time"}` | Move a scheduled class; Peloton gives it a **new** `scheduled_id`, returned with it. |
| `DELETE /v1/schedule/{scheduled_id}` | | Remove a scheduled class; answers 204. |
| `POST /v1/stack` | `{"class_id"}` | Append a class to the Stack. |
| `DELETE /v1/stack/{class_id}` | | Remove a class from the Stack, keeping the others in order. |
| `PUT /v1/stack` | `{"class_ids": [...]}` | Replace the whole Stack and its order (distinct ids, at most 100; `[]` clears it). |

A Stack write answers with the Stack as it now is, read back from Peloton, or
204 when the write succeeded but that read failed. The relay runs its own Stack
writes one at a time; removing and reordering read the Stack and write it back
whole, so a change made on the bike in between can be overwritten. A Stack
holding an item the relay cannot write back faithfully refuses those writes.

Class ids are 32 lowercase hexadecimal characters, as everywhere else in the
API; the relay encodes Peloton's own class ids itself. Unknown query parameters
and unknown body fields are rejected, and every input is validated before
anything is sent to Peloton.

### Schedule rules

- `start_time` is RFC 3339 **with an explicit offset** (a time without one is
  rejected), in the future and at most 14 days ahead by the relay's clock;
  fractions of a second are dropped. It is sent to Peloton in UTC. For a local
  wall-clock time, send the offset in effect on that day, e.g.
  `2026-11-02T08:00:00+01:00` for 08:00 in Berlin that Monday.
- Peloton decides everything else (conflicts, slots, its own window); a refusal
  answers 422 `cannot_schedule` with Peloton's explanation.
- `scheduled_id` is Peloton's id for the scheduled class re-encoded as URL-safe
  base64 without padding (`[A-Za-z0-9_-]`), so it fits a path segment. Use the
  value the relay returns; anything else is rejected.
- `GET /v1/schedule` lists every item Peloton reports in the window, each with a
  `kind`: `on_demand` for a scheduled class, otherwise Peloton's own type name
  (such items carry no class details).
- `GET /v1/schedule/{scheduled_id}` answers only for a class still on the
  schedule: after a remove, and for the old id after a move, it answers 404
  `not_found`.

### Library filters

`GET /v1/library` takes `category`, `class_type`, `instructor`, `duration`,
`sort`, `desc`, `page` (from 0) and `limit` (1 to 100, default 20), each a single
value:

- `category`: a browse category slug (`cycling`, `yoga`, `strength`, …; the
  OpenAPI lists all of them).
- `class_type` and `instructor`: 32-hex ids. They change over time, so take them
  from `GET /v1/library/filters?category=<slug>`, which serves Peloton's current
  values (e.g. the id of the "Low Impact" class type).
- `duration`: the exact class length in minutes, one of 5, 10, 15, 20, 30, 45,
  60, 75, 90 or 120.
- `sort`: `original_air_time`, `trending`, `popularity`, `top_rated` or
  `difficulty`, with `desc` (default `true`; `difficulty` with `desc=false` is
  easiest first). Without `sort` and `desc`, Peloton's order: newest first.

### Errors

Errors have the body `{"error": "<code>", "message": "<text>"}`:

| Status | `error` | Meaning |
|--------|---------|---------|
| 400 | `invalid_input` | A path, query or body value is malformed or out of range. |
| 404 | `not_found` | The resource, or the route, does not exist (including a class that is not in the Stack). |
| 405 | `method_not_allowed` | The route exists but not for this method. |
| 409 | `already_in_stack` | The class is already in the Stack. |
| 422 | `cannot_schedule` | Peloton refused to schedule the class; the message is Peloton's explanation. |
| 500 | `internal_error` | The relay failed while handling the request. |
| 502 | `upstream_error` | Peloton failed or returned something unexpected. After a write, whether it happened is unknown: read before retrying. |
| 503 | `not_authenticated` | The relay holds no usable Peloton credential; see [Bootstrap](#bootstrap). |
| 503 | `upstream_rate_limited` | Peloton is rate limiting the relay; a `Retry-After` header is set. |

## Security model

- **A fixed set of Peloton operations.** Reads: `GET /me`,
  `GET /user/{id}/workouts`, `GET /workout/{id}`,
  `GET /workout/{id}/performance_graph`, `GET /ride/{id}/details`,
  `GET /v2/ride/archived`, `GET /ride/filters` and `GET /browse_categories`, the
  GraphQL queries `YourScheduleItems`, `ScheduledClass`, `ViewUserStack` and
  `CheckDoesClassExistInUserStack`, and the token refresh
  (`POST /oauth/token`). Writes: only the GraphQL mutations
  `AddClassToSchedule`, `RescheduleClass`, `RemoveClassFromSchedule`,
  `AddClassToStack` and `ModifyStack`, behind the six write routes above. There
  is no generic pass-through, and no code path touches anything else on the
  account (profile, follows, workouts, history).
- **Reads and writes can be authorised separately.** Every write is a
  `POST`, `PATCH`, `PUT` or `DELETE` under `/v1/schedule` or `/v1/stack`, and
  every read is a `GET`, so a proxy in front of the relay can grant reading and
  writing as two separate permissions (the OpenAPI tags the writes
  `peloton-write`). Writes are human-scale and reversible (reschedule, remove,
  rewrite the Stack).
- **Writes are audit-logged.** Each write that passes validation logs one info
  event `event="peloton_write"` with `op` (`schedule_add`, `schedule_move`,
  `schedule_remove`, `stack_add`, `stack_remove` or `stack_replace`), the ids
  involved (`class_id`, `class_ids`, `scheduled_id`, `new_scheduled_id`), the
  UTC `start_time` when there is one, and `outcome` (`ok` or the error code).
  It never carries tokens or Peloton's message text.
- **The credential stays in the relay.** Tokens live in a state file inside the
  relay's state directory, are refreshed by the relay, and are never returned by
  the API, written to logs or error messages, or passed on a command line.
- **No authentication of its own.** The API has no login. Anyone who can reach
  the listening port can read the account's workout data and change its
  schedule and Stack, so bind it to a private interface or put it behind a
  network boundary or authenticating proxy. Do not expose it to the internet.
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

The image has no shell, so the binary is addressed by its path. The container
answers `docker exec peloton-relay /peloton-relay healthcheck` (exit 0 when the
server answers its liveness probe on the listen address, 1 otherwise), which
suits a Compose or Kubernetes health check (`["CMD", "/peloton-relay",
"healthcheck"]`); the image defines none itself.

## Bootstrap

The relay needs a Peloton token set before `/readyz` turns ready. The login
helper obtains one through the real Peloton web login (so passwords and 2FA are
handled by Peloton's own page) and prints it as JSON on stdout. It is a macOS
tool and is not part of the image. On a Mac:

```sh
cargo run --release -p peloton-relay-login | \
  ssh <host> sudo docker exec -i <container> /peloton-relay seed
```

`seed` reads the token set from stdin, verifies it with Peloton and stores it in
the state directory. The tokens never touch a command line or a disk outside the
state directory. Seed right after logging in: a token set whose access token has
already expired fails the check against `/me`; run the login again. For a login
that only reports non-secret facts, run
`cargo run --release -p peloton-relay-login -- check`.

To check the schedule and Stack operations against a real account (for example
after Peloton changes its web app), the helper has a spike:

```sh
cargo run --release -p peloton-relay-login -- spike --class <ride id>
```

It logs in, schedules the given on-demand class about 13 days ahead, reads,
moves and removes it, then adds the class to the Stack and writes the saved
Stack back, checking that both end as they started. These are **real writes to
the account**. Each step is reported PASS, FAIL or SKIP, and the raw responses
are saved to `--dir` (default `captures/` in the work tree, which is
git-ignored); they are unscrubbed and personal, so never commit them. After seeding, the relay
refreshes the credential on its own, checking it every 15 minutes even without
traffic; repeat the bootstrap only if Peloton invalidates the refresh token.

## Configuration

| Variable | Default | Meaning |
|----------|---------|---------|
| `PELOTON_RELAY_LISTEN` | `0.0.0.0:8080` | Socket address the server binds. |
| `PELOTON_RELAY_STATE_DIR` | `/state` | Directory holding `state.json`; must be writable by the container user. |

Logs are JSON lines on stdout. A failed Peloton authentication is logged once
per transition into the failed state as `event="peloton_auth_failed"`. When
Peloton rejects an access token the relay has just refreshed after a 401, it
logs `event="peloton_unauthorized_after_refresh"` once and forces at most one
refresh per 10 minutes, rather than rotating the credential on every request.
Each write logs `event="peloton_write"` (see [Security model](#security-model)).
A panic is logged as `event="panic"` with its source location only.

## Building

```sh
cargo build --release -p peloton-relay      # the server
docker build -t peloton-relay .             # the image
```

CI runs `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`,
`cargo deny check` and an arm64 and amd64 image build. A release tag (`v1.2.3`)
publishes the image only after those checks pass on the tagged commit, and only
if that commit is on `main`. No test or CI job talks to Peloton.

## Caveat

Peloton has no public API. This project uses the same private, undocumented API
as Peloton's own web app, signs in the way the web app does, and may stop
working at any time. Using it may be against Peloton's Terms of Service. It is
meant for reading your own account's data and planning your own rides; writes
through an unofficial API are a larger gray area than reads, so keep them
human-scale. You are responsible for how you use it. This project is not affiliated with or endorsed by Peloton.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
