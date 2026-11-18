# Configuration reference

Every key below is read by `stormstorefront <role>` (source in `src/`). The
manifests set them through `envFrom` ConfigMaps (`05-config-web.yaml`,
`06-config-api.yaml`) or `env:`; Secrets supply the two passwords. Unset keys
use the default shown. Every listen port is above 1024: the process holds no
capabilities and runs as an arbitrary UID.

## Roles and ports

| Role | Listens | Notes |
|---|---|---|
| `edge` | `:8080` (traffic), `:8404` (`/metrics`, `/stats`) | 8404 is not on the LoadBalancer Service |
| `web` | `:8080`; `127.0.0.1:8081` (`/stub_status`) | `/healthz`, `/readyz` (503 until content), `/api/` proxied |
| `api` | `:8081` | see endpoints below |
| `store` ("postgres") | `:5432`; `127.0.0.1:5433` (`/stats`) | bearer-token HTTP/JSON |
| `cache` ("redis") | `:6379`; `127.0.0.1:6380` (`/stats`) | RESP, `AUTH` |
| `exporter` | `--listen` (default `:9100`) `/metrics` | web 9113, store 9187, cache 9121 |
| `validate [dir]` | none | default dir `manifests`; exit 1 on a failed check |
| `sleep [secs]` | none | preStop drain, default 5 |
| `version` | none | prints `stormstorefront <version>` |

## Environment

| Key | Default | Role(s) |
|---|---|---|
| `WEB_LISTEN` | `:8080` | web |
| `STATS_LISTEN` | web `127.0.0.1:8081`, store `127.0.0.1:5433`, cache `127.0.0.1:6380` | web, store, cache |
| `CONTENT_DIR` | `/srv/www` | web |
| `API_HOST` / `API_PORT` | `api` / `8081` | web, api, cache-warmer |
| `DEST` | `/srv/www` | content-init, content-sync |
| `SYNC_INTERVAL` | `30` (s) | content-sync |
| `PGHOST` / `PGPORT` | `postgres` / `5432` | api, wait-for-deps, backup (backup uses 127.0.0.1) |
| `REDIS_HOST` / `REDIS_PORT` | `cache` / `6379` | api, wait-for-deps |
| `STORE_PASSWORD` | none (no auth) | store, api, backup — the bearer token |
| `REDIS_PASSWORD` | none (no auth) | cache, api |
| `CACHE_TTL_SECONDS` | `30` | api |
| `ACCESS_LOG` | `/var/log/api/access.log` | api, access-log |
| `WARM_INTERVAL` / `WARM_DELAY` | `20` / `5` (s) | cache-warmer |
| `DEP_TIMEOUT` | `120` (s) | wait-for-deps |
| `STORE_LISTEN` | `:5432` | store |
| `STORE_DATA` | `/var/lib/store/data` | store |
| `STORE_SEED` | none | store (seeded once from this file) |
| `CACHE_LISTEN` | `:6379` | cache |
| `BACKUP_DIR` / `BACKUP_INTERVAL` / `BACKUP_KEEP` / `BACKUP_DELAY` | `/backups` / `3600` s / `6` / `30` s | backup |
| `EDGE_CONFIG` | `/etc/edge/live/edge.toml` | edge, config-reloader |
| `EDGE_SOURCE` | `/etc/edge/src/edge.toml` | validate-config, config-reloader |
| `RELOAD_INTERVAL` | `10` (s) | config-reloader |
| `POD_NAME` (else `HOSTNAME`), `POD_NAMESPACE`, `NODE_NAME`, `CLUSTER_DOMAIN` | —, —, `unknown`, `cluster.local` | service-host resolution, `X-Served-By` |

A bare service host (`postgres`) is expanded to
`<name>.<POD_NAMESPACE>.svc.<CLUSTER_DOMAIN>` when `POD_NAMESPACE` is set.

## `edge.toml`

Unknown keys are rejected. `listen` and `metrics_listen` must be distinct and
above 1024.

| Key | Default |
|---|---|
| `listen`, `metrics_listen`, `default_backend` | required |
| `health_path` | `/healthz` |
| `retries` | `3` |
| `timeout_ms` | `30000` |
| `resolve_interval_ms` | `5000` |
| `[[route]] prefix, backend` | — (`/api/` → `api`) |
| `[backend.<name>] service, port, check` | required |
| `balance` | `roundrobin` (`leastconn` for api) |
| `check_host` | the service name |
| `slots` | `12` |
| `cookie` | none (web uses `SRVID`) |
| `inter_ms` / `rise` / `fall` | `2000` / `2` / `3` |

## HTTP APIs

- **api** (`:8081`): `GET|HEAD /api/healthz`, `/api/readyz` (503 if the
  store or cache is unreachable), `/api/products` (cache, then store),
  `/api/metrics` (Prometheus text); `POST /api/orders`;
  `GET /api/orders/<id>`. `/healthz` and `/readyz` also answer unprefixed.
- **store** (`:5432`, `Authorization: Bearer $STORE_PASSWORD` on `/v1/`):
  `GET /healthz`, `/readyz`; `GET /v1/products`, `/v1/products/<sku>`;
  `POST /v1/orders`; `GET /v1/orders/<id>`; `GET /v1/dump`.
