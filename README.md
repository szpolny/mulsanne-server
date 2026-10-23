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

## Deploy

The server runs on Fly.io in two apps: `fly.test.toml` (testing, used by dev builds via `MULSANNE_API_URL`) and `fly.toml` (production, `api.mulsanne.xyz`). Secrets (`DATABASE_URL`, `S3_ENDPOINT`, `S3_ACCESS_KEY_ID`, `S3_SECRET_ACCESS_KEY`) are set with `fly secrets set`.

GitHub Actions (`.github/workflows/ci.yml`) deploys automatically:

- **Testing:** every push to `master` whose tests pass.
- **Production:** with every app release. The app's release workflow deploys this repo's `master` (the commit testing runs, once its CI has passed) before it builds the installer. For a server-only fix between app releases, run the **CI** workflow by hand on `master` with **Deploy this commit to production** ticked.

**Compatibility:** installed apps update at their own pace, so production must keep working for every released app version. Make API changes additive: new endpoints and optional fields, never removed or renamed ones.

This repo's deploys use app-scoped Fly deploy tokens stored as repository secrets, `FLY_API_TOKEN_TESTING` and `FLY_API_TOKEN_PRODUCTION`:

```bash
fly tokens create deploy -a mulsanne-api-test | gh secret set FLY_API_TOKEN_TESTING
```

```bash
fly tokens create deploy -a mulsanne-api | gh secret set FLY_API_TOKEN_PRODUCTION
```

Manual deploys still work: `fly deploy -c fly.test.toml` for testing, `fly deploy` for production. After deploying, check `/healthz` and `/v1/ip`. Admin commands run on the machine: `fly ssh console -a <app> -C "mulsanne-server tracks list"`.
