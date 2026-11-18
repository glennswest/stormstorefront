# Architecture

The original's [ARCHITECTURE.md](https://github.com/glennswest/openshift-storefront-example/blob/main/docs/ARCHITECTURE.md)
holds for this one; this file says the same things for the stormcos version
and marks where it differs.

## The shape

Four tiers in one project, each a separate workload so it can be scaled,
rolled and disrupted independently:

```
edge (2)  →  web (3–9)  →  api (3–9)  →  postgres (1)
                    ↘  api  ↗              cache (1)
```

The edge routes by path: `/api/*` skips the web tier and goes straight to
the API; everything else goes to the web tier. The web tier *also* proxies
`/api/`, so a pod reached directly (port-forward) behaves the same as one
reached through the edge.

## One image, every role

Every container runs `image: stormstorefront` with
`command: ["/bin/stormstorefront", "<role>"]`. The original's images (HAProxy,
nginx, Python, PostgreSQL, Redis and three exporters, from three registries)
become roles of one static Rust binary:

| original | here | module |
|---|---|---|
| HAProxy 2.9 | `edge`, `config-reloader`, `validate-config` | `src/edge.rs`, `src/sidecars.rs` |
| nginx + sync-content.sh | `web`, `content-init`, `content-sync` | `src/web.rs`, `src/content.rs` |
| api.py (ubi9/python-311) | `api` | `src/api.rs` |
| tail -F, curl loops, /dev/tcp | `access-log`, `cache-warmer`, `wait-for-deps` | `src/sidecars.rs` |
| PostgreSQL 15 + pg_dump | `store`, `backup` | `src/store.rs`, `src/sidecars.rs` |
| Redis 7 | `cache` | `src/cache.rs`, `src/resp.rs` |
| nginx/postgres/redis exporters | `exporter` | `src/exporter.rs` |
| validate.sh (Python) | `validate` | `src/validate.rs` |

The image carries no shell, so the original's shell scripts in ConfigMaps
are code here, and the ConfigMaps carry only configuration (`edge.toml`,
`seed.json`, environment through `envFrom`). Probes are `httpGet` and
`tcpSocket`; the `preStop` drains run the binary's own `sleep`.

## Why every pod has more than one container

As in the original, each sidecar earns its place by needing something only a
shared pod gives it:

**Shared filesystem.** `content-sync` writes into the `emptyDir` the web
server reads. `access-log` follows a file the API writes.

**Shared loopback.** Every exporter reads its neighbour on `127.0.0.1`: the
web server's `stub_status`, the store's and the cache's stats listeners bind
loopback only and are unreachable from outside the pod. The `backup` sidecar
reads the store over loopback too, so it never crosses a NetworkPolicy; the
`data-egress-dns-only` policy would block it if it did.

**Shared lifecycle.** `config-reloader` is meaningless without the edge it
reloads. `cache-warmer` warms *this* pod's view.

**Shared PID namespace.** Only the edge pod sets `shareProcessNamespace`,
because signalling is the reload mechanism. Unlike HAProxy, the edge also
watches its live file, so a runtime that does not share the PID namespace
still reloads (the test reports which way it happened).

The init containers order things in time: `validate-config` fails the rollout
before anything binds a port, `wait-for-deps` turns a slow data tier into a
pending pod, `content-init` puts content in place before the first request.

## Where state lives

| State | Where | Survives pod restart? |
|---|---|---|
| Catalog and orders | the store, PVC `data` (1Gi, stormblock-csi) | yes — the test proves it |
| Logical backups | PVC `postgres-backups` (1Gi) | yes |
| Catalog cache | the cache's memory | no — by design |
| Rendered site | `emptyDir`, regenerated every 30 s | no — by design |
| Access log | `emptyDir`, followed to stdout | no — the collector has it |

Where the original's API serves an in-process catalog and only TCP-checks
postgres, this API reads the catalog from the store and writes orders to it:
the database tier is really on the request path, and an order is the datum
the test writes before restarting `postgres-0` and reads back after.

The cache being ephemeral is a property the API is written to tolerate: every
cache call is wrapped, a failure counts in
`storefront_api_upstream_errors_total` and falls through to the store.

## The request path, end to end

1. The LoadBalancer address (Cilium) or stormlb's router (HTTPRoute) reaches
   an edge pod on :8080.
2. The edge stamps `X-Request-Id`, answers `/healthz` itself, picks a backend
   by path.
3. `web` is round-robin with an insert cookie (`SRVID`) for affinity; `api`
   is least-connections with no affinity, because the API is stateless.
4. Both backends resolve a **headless** Service: one server per ready pod (up
   to 12, above the HPA ceiling of 9, so scaling needs no reload), each
   health-checked by the edge (`/healthz`, `/api/readyz`).
5. The API's `/api/readyz` is 503 whenever the store or the cache is
   unreachable: that one signal removes the pod from the Services *and* from
   the edge.

## Rollouts and disruption

Every Deployment uses `maxUnavailable: 0, maxSurge: 1`. Each stateless tier
has a `preStop` drain before SIGTERM, so endpoint removal reaches the
Services and the edge while the pod still serves.

PodDisruptionBudgets: `edge` `maxUnavailable: 1`, `web` and `api`
`minAvailable: 2`. The data tier has none on purpose.

One difference: the original's edge has **required** anti-affinity (two edge
pods never on one node). On a single-node stormcos (every test machine) the
second replica would stay Pending, so here it is **preferred**: spread when
there are nodes to spread over.

## Security posture

- Default-deny ingress; each tier opens exactly the ports its caller needs.
  The data tier also has an egress policy allowing DNS and nothing else.
- Four ServiceAccounts, one per tier; only the API keeps its token, and its
  Role is read-only, namespace-scoped and excludes Secrets.
- Secrets are referenced by `secretKeyRef`. The store requires its password
  as a bearer token on every `/v1/` call; the cache requires `AUTH`.
- Every container: no privilege escalation, every capability dropped, a
  read-only root filesystem (the original needs a writable one for nginx and
  PostgreSQL; nothing here writes outside its volumes).

## Deliberate simplifications

- **Single store replica**, no replication or failover; **single cache**.
- **The store is not PostgreSQL.** It is a JSON document store with the
  durability the example needs (atomic writes, a seed, backups). fastetcd is
  the platform's durable store, but it is the control plane's; an app tier
  with its own data on its own PVC is the point of the example.
- **CPU-based autoscaling**, as the original.
- **No TLS at the edge**: stormlb's router serves plain HTTP for a pod
  HTTPRoute today (docs/STORMCOS-NOTES.md).
