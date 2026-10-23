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
