# Configuration overview

Every setting can come from four sources, highest precedence first:

1. command-line flag (`--upload-access wot`)
2. process environment (`ALMOND_UPLOAD_ACCESS=wot`)
3. config file (dotenv format, same `ALMOND_*` names)
4. built-in default

```bash
almond --config /etc/almond/almond.env --bind-addr 0.0.0.0:3000
```

- **Config file:** `--config <FILE>` (or `ALMOND_CONFIG`) loads a dotenv file;
  without it, `./.env` is loaded when present. Values in the file never override
  real environment variables. [`.env.example`](https://github.com/flox1an/almond/blob/main/.env.example)
  documents every setting and is a good starting point.
- **Name mapping:** env name = `ALMOND_` + flag name upper-cased with `-` → `_`
  (`--chunk-max-sessions-per-pubkey` ↔ `ALMOND_CHUNK_MAX_SESSIONS_PER_PUBKEY`),
  so flags and environment variables can never drift apart.
- An empty value (`ALMOND_X=`) behaves exactly like an unset one.
- **Secrets** (S3 keys, metrics token) belong in the environment or config file:
  command-line arguments are visible to other users in `ps`.
- **Strict parsing:** unknown enum values, invalid booleans, invalid npubs and
  bad durations or sizes stop startup instead of falling back to a default.
- `almond --help` lists every flag with its env name and default;
  `almond --version` prints the version.
- **Logging:** `RUST_LOG` (e.g. `warn`, `almond=debug`; default `info`), also
  honored in the config file.

Where to next:

- [Command-line flags](/configuration/cli) — examples and CLI-specific syntax
- [Environment variables](/configuration/environment) — value formats, full
  reference and the pre-0.5 migration table
- [FIPS mesh variables](/configuration/environment#fips-mesh) — `FIPS_*`
  settings of the `almond-fips` image
