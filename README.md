# peloton-relay

A small HTTP relay for one or more Peloton accounts. The relay holds each
account's Peloton credential and exposes a handful of typed endpoints, so that
agents, scripts and dashboards can read workout data, search the class library
and plan rides (schedule classes and manage the Stack) without ever seeing the
credential.

The server is one static binary that runs as an unprivileged user in a
`FROM scratch` container image.

## API

All responses are JSON (the [admin page](#admin-page) aside). The OpenAPI 3.1
description is served at `/openapi.json`;
it lists every parameter and enum value and walks through one example (find a
20-minute low-impact ride and schedule it for Monday 08:00).

Every route except `/healthz`, `/readyz`, `/openapi.json` and the admin routes
under `/admin` serves one account, named by the `X-Peloton-Account` request
header (see [Accounts](#accounts)).

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
| `GET /readyz` | Readiness: the server is up. It never consults credentials. |

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
| 403 | `forbidden` | The request does not name an account this relay serves (no `X-Peloton-Account` header, or an account that is not configured). Checked before anything else, including routing and validation. |
| 404 | `not_found` | The resource, or the route, does not exist (including a class that is not in the Stack). |
| 405 | `method_not_allowed` | The route exists but not for this method. |
| 409 | `already_in_stack` | The class is already in the Stack. |
| 422 | `cannot_schedule` | Peloton refused to schedule the class; the message is Peloton's explanation. |
| 500 | `internal_error` | The relay failed while handling the request. |
| 502 | `upstream_error` | Peloton failed or returned something unexpected. After a write, whether it happened is unknown: read before retrying. |
| 503 | `not_authenticated` | The relay holds no usable Peloton credential for this account; see [Bootstrap](#bootstrap). |
| 503 | `upstream_rate_limited` | Peloton is rate limiting the relay; a `Retry-After` header is set. |

## Accounts

The relay serves the accounts listed in `PELOTON_RELAY_ACCOUNTS`, a
comma-separated list of distinct names (no spaces, no empty items). An account
name is 1 to 16 lowercase ASCII letters, chosen by the deployment (for example
`alice,bob`); it is not derived from the Peloton username.

Each request names its account in the `X-Peloton-Account` header. A request
that names no configured account answers 403 `forbidden`, for every path but
`/healthz`, `/readyz`, `/openapi.json` and those under `/admin` (unmatched paths
included), before any parameter or body is validated and before Peloton is
contacted. The header is
set by the proxy in front of the relay, not by clients (see
[Security model](#security-model)).

The state directory holds one directory per account:

```
<PELOTON_RELAY_STATE_DIR>/
  .bindings.lock          cross-account lock for the bindings
  alice/                  0700
    state.json            0600, the account's token set
    binding.json          0600, {"user_id": "<32 hex>"}
    .state.json.lock      0600, serialises seed, bind and token refresh
  bob/
    ...
```

`binding.json` records which Peloton user the account belongs to. The rules:

- `seed` binds an account to the Peloton user of the credential it stores.
- A Peloton user is bound to at most one account; seeding the same user into a
  second account is refused, even with `--rebind`.
- An account bound to one user refuses a credential of another user unless
  `seed` is run with `--rebind`.
- The server refuses a state file whose user does not match the binding
  (logged as `state_refused`) and keeps the account unauthenticated. It also
  refuses a state whose user another account is bound to, and any state while
  the binding of another account cannot be read (fail closed). These are
  checked when the account's state loads or reloads, not continuously.

Seed an account (see [Bootstrap](#bootstrap)):

```sh
... | docker exec -i <container> /peloton-relay seed --account alice [--rebind]
```

On success it prints:

```
account "alice": bound to Peloton user <username> (id …00f2)
```

with `(no username)` in place of `<username>` when Peloton reports none, and the
id shown as its last four hex digits. When `--rebind` changed the user, it
first prints `account "alice": was bound to Peloton user id …<last 4 hex>`.

To move a Peloton user to another account, delete the old account's
`binding.json`, then seed the new account. The old account's `state.json` may
stay: the server refuses it while the account is unbound, and `bind` refuses it
while its user is bound to the new account.

**Upgrading from 0.2.x.** Earlier releases kept a single `state.json` directly
in the state directory, and 0.3.0 refuses to start while that file is there.
Move it into its account's directory and bind the account to the user of that
state file, in this order:

1. Stop the 0.2.x container.
2. Create the account directory, owned by the container's user (65532 for the
   published image, or the `user:` your deployment sets; otherwise `bind` and
   the server cannot open it), and move the state file into it. A `mv` within
   one filesystem keeps the file's ownership, which must stay that user:

   ```sh
   install -d -m 0700 -o <uid> -g <gid> <state dir>/alice
   mv <state dir>/state.json <state dir>/alice/state.json
   ```

3. Start 0.3.0 with `PELOTON_RELAY_ACCOUNTS` set (it has no default), e.g.
   `PELOTON_RELAY_ACCOUNTS=alice`.
4. Bind the account to the user of its state file:

   ```sh
   docker exec <container> /peloton-relay bind --account alice
   ```

Between steps 3 and 4 the account's `/v1` routes answer 503
`not_authenticated` and the server logs `event="state_refused"` with
`reason="unbound"`; both are expected and end once `bind` has run.

**Upgrading from 0.3.x.** The state directory is unchanged: stop 0.3.x and start
0.4.0 on the same directory. The [admin page](#admin-page) stays closed (403)
until the proxy sets `X-Peloton-Admin: 1` for the operator; to renew sign-ins
from it, also set `PELOTON_RELAY_PUBLIC_URL`.

## Admin page

`GET /admin` is a small page for the operator: one card per account in
`PELOTON_RELAY_ACCOUNTS`, with its credential health (healthy, needs sign-in, or
unreadable when the state or binding file cannot be read or refreshed tokens
cannot be saved), when the relay last refreshed its access token and when that
token expires, the Peloton user it is bound to (username and the last four hex
digits of the id), and its recent credential problems (the last 8, kept in
memory, as fixed messages). It never shows a token. Each time it is opened, the
page first checks every account's credential the way the background check does.
It works without JavaScript, apart from the Copy button and the live update
while renewing; it loads nothing from elsewhere and follows the light or dark
setting of the device.

**Access.** Every path under `/admin` is served only when the request carries
exactly one `X-Peloton-Admin` header whose value is `1`; anything else (no
header, another value, the header twice) answers 403 `forbidden` before
routing, and is logged as `event="admin_refused"` with the reason, never the
value. The admin routes take no `X-Peloton-Account`, and `X-Peloton-Admin` never
opens a `/v1` route. Like the account header, the admin header is set by the
proxy in front of the relay, from an app capability that only the operator
holds, and any copy the client sent is dropped (see
[Security model](#security-model)). Every answer under `/admin` carries a strict
`Content-Security-Policy` (`default-src 'none'`, the page's own inline script
and style allowed by hash, `connect-src 'self'`, `form-action 'self'`,
`frame-ancestors 'none'`, `base-uri 'none'`), `Cache-Control: no-store`,
`X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer` and
`X-Frame-Options: DENY`.

**Revoke.** *Revoke sign-in*, confirmed by a second *Really revoke* button,
posts to `POST /admin/accounts/{name}/revoke`. It deletes the account's
`state.json` under the account's lock, keeps `binding.json`, and drops the
credential from the running relay, so the account's callers get 503
`not_authenticated` at once, until it is signed in again (a request already in
flight finishes with the token it had). It logs `event="account_revoked"` with
the account and whether there was a state file to delete, then redirects to
`/admin`, so reloading the page does not post again. Revoking makes the relay
forget the credential; it does not invalidate it at Peloton: the refresh token
stays valid there until it is rotated or expires, and so do copies of
`state.json` in backups.

**Renew or add.** *Renew sign-in* (or *Sign in* for an account that never had
one) posts to `POST /admin/accounts/{name}/code`, which needs
`PELOTON_RELAY_PUBLIC_URL`: without it the page says so and offers no button.
The relay issues a one-time code for that account and answers with the command
to run on the Mac, shown once with a Copy button (reloading that page posts the
form again and issues a new code, which replaces the one shown):

```sh
peloton-relay-login --account bob --post-to https://relay.example.net/admin/seed --code <code>
```

The helper first names the relay the sign-in will go to and asks to continue
(`--yes` skips the question; without a terminal to ask on and without `--yes`, it
refuses), since a pasted command could name any host. It then opens the
sign-in window and posts the token set with the code to
`POST /admin/seed`, which runs the same checks as `seed` (`/me`, the account's
binding, and refusing a Peloton user bound to another account; `--rebind` is not
available there) and takes the credential into use without a restart. The page
polls `GET /admin/accounts/{name}/status` and switches to *Signed in* once the
account is healthy. The helper takes `--account`, `--post-to` and `--code`
together or not at all; it refuses a `--post-to` that is not `https` to an IP
address or a DNS name of letters, digits and hyphens (plain `http` only to a
loopback address), never follows a redirect, prints the relay's
confirmation line (or why the relay refused) on stderr, and never prints the
code or the tokens. The code:

- is 128 random bits, kept only as its SHA-256 digest (compared in constant
  time), in memory: a restart invalidates it;
- is valid for 10 minutes, for that one account; a new code for the account
  replaces the old one;
- is spent by a successful seed, and by a refusal of the binding (the account is
  bound to another user, or the user to another account); a failure to reach
  Peloton, a token Peloton rejects, a busy lock or a binding that cannot be
  read leave it valid for a retry;
- is never logged or put in an answer other than the page that issued it. It
  is on the helper's command line, so it shows in the process list and the
  shell history until it is spent or expires.

After 5 wrong, expired or spent codes within 10 minutes, across all accounts,
`POST /admin/seed` refuses every code with 429 `too_many_attempts` and a
`Retry-After` until the oldest of those failures is 10 minutes old. A retry of a
code that already succeeded counts too (for example when the helper gave up
waiting for an answer the relay then sent); the status on the page shows whether
it did.

`POST /admin/seed` takes a JSON body (`Content-Type: application/json`, checked
before the body is read; at most 64 KiB):

```json
{"account": "bob", "code": "<code>", "tokens": <the token set the helper prints for seed>}
```

and answers `{"account": "bob", "message": "<the line seed prints>"}`. Its
outcomes are logged as `event="admin_seed"` with the account and `outcome` (`ok`
or the error code), and refused codes as `event="admin_code_refused"`.

The two page forms carry a CSRF token (an HMAC, under a key drawn at start, of
the form and the account; a restart makes open pages stale) and are refused with
403 `csrf_rejected` when it is missing or wrong, or when the browser reports the
request as coming from another origin: `Sec-Fetch-Site` other than
`same-origin`, or an `Origin` other than exactly the origin of
`PELOTON_RELAY_PUBLIC_URL` (scheme, host and port), or, when that is unset, other
than the request's own host. The page sends no referrer, so browsers send
`Origin: null` with its forms; that passes only with
`Sec-Fetch-Site: same-origin`. `POST /admin/seed` is not a browser form and needs
no CSRF token: the code and the admin header authorise it. It refuses any
request a browser sent (one with an `Origin` header, or a `Sec-Fetch-Site` other
than `none`) with 403 `browser_request` before looking at the code, so no web
page can spend or guess codes.

| Status | `error` | Meaning |
|--------|---------|---------|
| 400 | `invalid_input` | The seed body is malformed. |
| 403 | `forbidden` | The request is not marked as admin. |
| 403 | `csrf_rejected` | A form's CSRF token or origin is wrong; reload the page. |
| 403 | `browser_request` | A browser sent the request meant for the login helper. |
| 403 | `invalid_code` | The code is wrong, expired, spent or for another account. |
| 404 | `not_found` | No such account (or admin route). |
| 409 | `not_configured` | Renewing needs `PELOTON_RELAY_PUBLIC_URL`. |
| 409 | `bound_to_another_user` | The account is bound to another Peloton user. |
| 409 | `user_bound_elsewhere` | The Peloton user is bound to another account. |
| 409 | `binding_unreadable` | Another account's binding (or this one's) cannot be read, so the seed is refused; see the log. |
| 413 | `payload_too_large` | The seed body exceeds 64 KiB. |
| 415 | `unsupported_media_type` | The seed body is not `application/json`. |
| 422 | `token_rejected` | Peloton rejected the posted access token; sign in again. |
| 429 | `too_many_attempts` | Too many wrong codes; `Retry-After` says when to retry. |
| 502 | `upstream_error` | Peloton failed to answer the `/me` check. |
| 503 | `busy` | A seed or token refresh held the account's lock too long; retry. |

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
- **No authentication of its own.** The API has no login. The relay trusts the
  `X-Peloton-Account` header, so it must be reachable only through a proxy that
  sets that header from the caller's authenticated identity and drops any copy
  the client sent. Anyone who can reach the listening port directly can choose
  any configured account, read its workout data and change its schedule and
  Stack, so bind it to a private interface or put it behind a network boundary
  and an authenticating proxy. Do not expose it to the internet.
- **Admin access is separate.** The admin routes trust the
  `X-Peloton-Admin: 1` header the same way: the proxy must set it only for the
  operator (for example from an app capability of the gateway that only the
  operator holds) and drop any copy the client sent. Anyone who can reach the
  listening port directly can revoke and renew sign-ins, and with the code the
  page shows, seed an account. The admin page never shows a token; signing in
  stays in the login helper, so the relay never sees a Peloton password.
- **One Peloton user per account.** The binding keeps an account's state file
  from holding another user's credential, and a Peloton user from serving two
  accounts (see [Accounts](#accounts)).
- **Minimal image.** Static musl binary, no shell, no package manager, no CA
  bundle (TLS trust anchors are compiled in via rustls and `webpki-root-certs`),
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

Each account needs its own Peloton token set before the relay can answer for it
(without one, its `/v1` routes answer 503 `not_authenticated`; `/readyz` only
reports that the server is up). The login helper obtains one through the real
Peloton web login (so passwords and 2FA are handled by Peloton's own page) and
prints it as JSON on stdout. It is a macOS tool and is not part of the image.
On a Mac:

```sh
cargo run --release -p peloton-relay-login | \
  ssh <host> sudo docker exec -i <container> /peloton-relay seed --account <name>
```

`seed` reads the token set from stdin, verifies it with Peloton, binds the
account to the credential's Peloton user and stores it in the account's
directory. The tokens never touch a command line or a disk outside the state
directory. `seed` and the server's token refresh take turns on a lock file
(`.state.json.lock`, 0600, in the account's directory next to `state.json`), so
neither overwrites the other: a seed that lands first is used by the refresh,
and a seed that arrives during a refresh waits for it (up to a minute, then it
fails and can be run again). Seed right after logging in: a token set whose
access token has already expired fails the check against `/me`; run the login
again. For a login that only reports non-secret facts, run
`cargo run --release -p peloton-relay-login -- check`. With
`PELOTON_RELAY_PUBLIC_URL` set, the [admin page](#admin-page) gives a command
that posts the token set to the relay over HTTPS instead of through
`docker exec`.

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
| `PELOTON_RELAY_ACCOUNTS` | none, required | Comma-separated account names served by this relay; see [Accounts](#accounts). |
| `PELOTON_RELAY_STATE_DIR` | `/state` | Directory holding one directory per account; must be writable by the container user. |
| `PELOTON_RELAY_PUBLIC_URL` | none | The `https` origin the proxy serves the relay at (for example `https://relay.example.net`; no path, query or credentials), for the renew command of the [admin page](#admin-page). Renewing from the page is off when unset. |

Logs are JSON lines on stdout. Every event about a credential or a write
carries an `account` field with the account name; Peloton user ids appear only
as their last four hex digits (`user="…00f2"`), and usernames never. A failed
Peloton authentication is logged once per transition into the failed state as
`event="peloton_auth_failed"`. When Peloton rejects an access token the relay
has just refreshed after a 401, it logs
`event="peloton_unauthorized_after_refresh"` once and forces at most one
refresh per 10 minutes, rather than rotating the credential on every request.
A failure to save refreshed tokens is logged once per streak as
`event="state_save_failed"`, and the next successful save as
`event="state_save_recovered"`. A state file that does not match its account's
binding is refused with `event="state_refused"`, whose `reason` is `unbound`,
`user_mismatch` or `binding_corrupt` for the account's own binding, and
`bound_elsewhere`, `peer_binding_corrupt` or `peer_binding_unreadable` when
another account (named in `peer`) is bound to the user or its binding cannot
be used. When the running relay picks up a rebind (a seed with `--rebind` that
moved an account to another Peloton user), it logs `event="account_rebound"`;
the seed's own output goes to the operator's terminal, not to the log. A
request that names no configured account logs `event="account_refused"` (the
name only when it is a valid account name).
Each write logs `event="peloton_write"` (see [Security model](#security-model)).
The admin routes log `admin_refused`, `admin_form_refused`, `admin_code_issued`,
`admin_code_refused`, `admin_seed`, `account_revoked` and
`account_revoke_failed` (see
[Admin page](#admin-page)); none carries a code or a token.
A panic is logged as `event="panic"` with its source location only.
Every request gets a fresh id, returned as the `x-request-id` header (an id sent
by the client is ignored), and its log lines carry the id, the method and the
matched route template (never the query string or the path's own ids) in their
`span` field. Responses from Peloton and its token endpoint are read up to
8 MiB; a larger one fails the request with 502 `upstream_error` (or the token
refresh or seed with an error).

## Building

```sh
cargo build --release -p peloton-relay      # the server
docker build -t peloton-relay .             # the image
```

CI runs `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`,
`cargo deny check` (clippy, test and deny with `--locked`, so they use the
committed `Cargo.lock` as is) and an arm64 and amd64 image build, and on pull requests the dependency age check
below. A release tag (`v1.2.3`) publishes the image only after those checks and
the dependency age check pass on the tagged commit, and only if that commit is
on `main`. No test or CI job talks to Peloton.

Every pull request is also checked by [zizmor](https://docs.zizmor.sh), which
audits the workflows themselves (pinned actions, token permissions, template
injection, cache poisoning), and by CodeQL. `cargo deny check advisories` runs
every night on `main`, so a newly published advisory shows up there before it
fails an unrelated pull request, and OpenSSF Scorecard reports weekly. Dependabot
proposes updates once they are 14 days old. Report a vulnerability privately as
described in [SECURITY.md](SECURITY.md).

### Dependency age

Every crate version in `Cargo.lock` must have been published on crates.io at
least 14 days before it is added, so a compromised or broken release has time to
be noticed and yanked. On each pull request,
[`.github/scripts/crate_age.py`](.github/scripts/crate_age.py) compares
`Cargo.lock` with the base branch's, looks up the publish date of every registry
crate version that is new or changed, and fails the build for one younger than
14 days. Unchanged versions are never looked up, so a pull request that leaves
`Cargo.lock` alone makes no request to crates.io; a lookup that fails fails the
check rather than skipping the crate. The release workflow runs the same check
against the previous release tag (or against nothing for the first release), so
a tag never ships a version that skipped it, even one pushed straight to `main`.

The one exception is the oldest release that fixes a security advisory. List it
in [`.github/crate-age-allow.toml`](.github/crate-age-allow.toml), one entry per
crate version, with a reason that names the RUSTSEC advisory:

```toml
[[allow]]
name = "webpki-root-certs"
version = "1.0.10"
reason = "Oldest release without RUSTSEC-2026-0383"
```

An entry covers that exact version only. Once its version leaves `Cargo.lock`
the entry is stale and fails the check, so it is removed with the update that
replaces the crate. To run the check locally on the working tree against `main`,
or on one commit against another (reading `Cargo.lock` and the allow-list at
`--head`):

```sh
python3 -I .github/scripts/crate_age.py --base main
python3 -I .github/scripts/crate_age.py --base v0.3.0 --head v0.4.0
```

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
