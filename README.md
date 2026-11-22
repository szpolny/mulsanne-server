# mulsanne-server

Backend for Mulsanne's online features (lap rankings, car setup sharing).

- `crates/api`: axum server (`mulsanne-server`)
- `crates/types`: shared request/response types and request signing (`mulsanne-api`)
- `migrations`: sqlx migrations, applied on startup

## Run

Needs Postgres 13+. Copy `.env.example` to `.env`, export the variables, then:

```bash
cargo run -p mulsanne-server
```

## Test

```bash
cargo test
```

Database tests are ignored by default; run them against a disposable database:

```bash
DATABASE_URL=postgres://mulsanne:mulsanne@localhost:5432/mulsanne_test cargo test -- --include-ignored
```

## Request signing

Every `/v1` request is signed with the install's Ed25519 key; see `crates/types/src/signing.rs`. The app (public repo) mirrors this in `src-tauri/src/online.rs`, and both test the same vector.

## Live timing

`/v1/live` relays live timing from the app to the viewer page at `mulsanne.xyz/live#<id>` (see `crates/api/src/live.rs`). The app opens a share (`POST /v1/live`) and pushes a JSON frame about once a second (`PUT /v1/live/{id}/frame`) plus the track outline when it changes (`PUT /v1/live/{id}/map`). Browsers read `GET /v1/live/{id}` and then follow `GET /v1/live/{id}/events` (server-sent events: `map`, `frame`, `end`). Those two routes allow any origin.

Frames are kept **in memory only**, so live timing needs a single machine: with more, a viewer and the driver's app can land on different ones. The `live_shares` table only records who owns each share, so after a restart or deploy the owner's next push brings the share back under the same link.

## Admin dashboard

`/v1/admin` backs the dashboard at `mulsanne.xyz/admin` (see `crates/api/src/admin.rs`): the rankings with every lap (held and anonymous ones too) and deleting laps, drivers and clearing display names, and who is live sharing now plus the history of shares.

The admin signs in with Steam. `GET /v1/admin/steam/login` sends the browser to Steam's OpenID page; Steam returns to `/v1/admin/steam/callback`, which checks the assertion with Steam and lets in only the account in `ADMIN_STEAM_ID`. It then redirects to `ADMIN_WEB_URL#token=<token>`, and the page sends the token as `Authorization: Bearer`. Sessions last 12 hours and are stored hashed in `admin_sessions`; changing `ADMIN_STEAM_ID` ends them. Without `ADMIN_STEAM_ID` the routes answer 404.

`ADMIN_STEAM_ID` is the repository secret of the same name (your SteamID64, the 17-digit number in `steamcommunity.com/profiles/<id>`). The deploy jobs stage it on both Fly apps before deploying, so it is never in the repo:

```bash
gh secret set ADMIN_STEAM_ID
```

`API_PUBLIC_URL` and `ADMIN_WEB_URL` are set in `fly.toml` and `fly.test.toml`. Testing returns to `http://localhost:5173/admin`, for the website's dev server pointed at testing.

## Deploy

The server runs on Fly.io in two apps: `fly.test.toml` (testing, used by dev builds via `MULSANNE_API_URL`) and `fly.toml` (production, `api.mulsanne.xyz`). Secrets (`DATABASE_URL`, `S3_ENDPOINT`, `S3_ACCESS_KEY_ID`, `S3_SECRET_ACCESS_KEY`) are set with `fly secrets set`; `ADMIN_STEAM_ID` comes from the repository secret (see above).

This repo is the only thing that deploys. GitHub Actions (`.github/workflows/ci.yml`):

- **Testing:** every push to `master` whose tests pass.
- **Production:** releases, via [release-please](https://github.com/googleapis/release-please) like the app. Use [conventional commits](https://www.conventionalcommits.org/) (`feat:`, `fix:`, `feat!:`); release-please keeps a release PR open with the next version and changelog. Merging it tags the release and deploys it to production. Hotfixes go the same way.

`GET /v1/version` reports what's running: the release version in production, `testing-<commit>` on testing.

**Releasing with the app:** an app release checks that production is at least the version in the app's `src-tauri/min-server-version.txt` before it builds the installer. For a change both need:

1. Merge the server change; testing updates.
2. Test it with a dev build against testing.
3. Merge this repo's release PR; production updates.
4. Raise `min-server-version.txt` in the app if it needs the new API.
5. Release the app.

**Compatibility:** installed apps update at their own pace, so production must keep working for every released app version. Make API changes additive: new endpoints and optional fields, never removed or renamed ones.

The deploys use app-scoped Fly deploy tokens stored as repository secrets, `FLY_API_TOKEN_TESTING` and `FLY_API_TOKEN_PRODUCTION`:

```bash
fly tokens create deploy -a mulsanne-api-test | gh secret set FLY_API_TOKEN_TESTING
```

```bash
fly tokens create deploy -a mulsanne-api | gh secret set FLY_API_TOKEN_PRODUCTION
```

After deploying, check `/healthz`, `/v1/version` and `/v1/ip`. Admin commands run on the machine: `fly ssh console -a <app> -C "mulsanne-server tracks list"`.
