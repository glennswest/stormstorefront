# stormstorefront — the storefront on stormcos

The owner's [openshift-storefront-example](https://github.com/glennswest/openshift-storefront-example)
on stormcos: the same topology and the same manifests shape, so a person who
knows the OpenShift one reads this one. Everything lives in one project
(`storefront`), every pod runs more than one container, and every pod is
written for the `restricted` level: no root, no pinned UID, no fsGroup, no
privileged ports, no added capabilities. It is **offline and ours**: every
container runs one image, `stormstorefront`, a single static Rust binary
whose role is its first argument. Nothing is pulled from docker.io, quay.io
or registry.redhat.io.

It is also **the workload every test machine runs** (`stormstorefront-test`)
to prove that projects, multi-container pods, storage, networking and `sc`
behave like OpenShift.

```
                    Clients
                        │
        ┌───────────────┴────────────────┐
        │                                │
   HTTPRoute (stormlb router)   Service type=LoadBalancer (stormlb)
   Host: storefront.storm1…     edge-lb :80  →  :8080
   on any node's :80            externalTrafficPolicy: Local
   rules: / → web, /api/ → api           │
   (straight to web/api pods)            │
                                         ▼
        ┌────────────────────────────────────┐
        │  edge  ×2        Deployment        │   Rust L7 edge
        │  ├─ edge               :8080 :8404 │   path routing, per-pod health
        │  └─ config-reloader                │   checks, seamless reload
        │  (init: validate-config)           │
        └───────┬───────────────────┬────────┘
       path /   │                   │  path /api/
                ▼                   ▼
 ┌────────────────────────┐  ┌────────────────────────────┐
 │  web  ×3  (HPA 3–9)    │  │  api  ×3  (HPA 3–9)        │
 │  ├─ web          :8080 │──▶  ├─ api            :8081   │
 │  ├─ content-sync       │  │  ├─ access-log             │
 │  └─ exporter     :9113 │  │  └─ cache-warmer           │
 │  (init: content-init)  │  │  (init: wait-for-deps)     │
 └────────────────────────┘  └───────┬──────────────┬─────┘
                                     ▼              ▼
                    ┌────────────────────┐  ┌─────────────────────┐
                    │ postgres  ×1  STS  │  │ cache  ×1           │
                    │ ├─ store      :5432│  │ ├─ cache      :6379 │
                    │ ├─ exporter   :9187│  │ └─ exporter   :9121 │
                    │ └─ backup          │  └─────────────────────┘
                    │ PVC data   1Gi     │
                    │ PVC backups 1Gi    │
                    │ (stormblock)       │
                    └────────────────────┘
```

Steady state: **10 pods, 27 running containers, 8 init container runs**
(3 init containers: one per edge, web and api pod), 9 Services, 7
NetworkPolicies, 2 HPAs, 3 PDBs, 2 PVCs, one HTTPRoute.

## What each container is for

Every container is `image: stormstorefront`, `command: ["/bin/stormstorefront", "<role>"]`.

| Tier | Pod | Container | Role |
|---|---|---|---|
| edge | `edge` ×2 | *init* `validate-config` | checks the ConfigMap's `edge.toml` and stages it into a writable volume; a bad config fails the rollout instead of the running replicas |
| | | `edge` | L7 load balancer: `/healthz` itself, `/api/` → api, the rest → web; balances across **pod IPs** via the headless Services and health-checks each pod (rise/fall), cookie affinity for web, least-connections for api, retries, `X-Request-Id`, `X-Served-By` |
| | | `config-reloader` | watches the ConfigMap mount, validates, stages the new file, SIGHUPs the edge through the shared PID namespace (the edge also watches its file, so it reloads without one) |
| web | `web` ×3 | *init* `content-init` | renders the site into the shared volume before the server starts, so the pod is never Ready with a 404 |
| | | `web` | serves the content on :8080, proxies `/api/`, `/readyz` 503 until there is content |
| | | `content-sync` | re-renders every 30 s — stands in for "pull the built site from object storage" |
| | | `exporter` | reads the server's loopback-only `stub_status` and serves Prometheus on :9113 |
| api | `api` ×3 | *init* `wait-for-deps` | blocks until the store and the cache accept TCP, so a slow data tier is a pending pod, not a CrashLoop |
| | | `api` | the original's `api.py` endpoints (health, readiness, catalog, metrics) plus orders, written to the store |
| | | `access-log` | follows the API's file-based access log onto stdout for the log collector |
| | | `cache-warmer` | refreshes this pod's catalog entry so an expiry does not stampede the store |
| data | `postgres` ×1 | `store` | the catalog and the orders on the `data` PVC, behind an authenticated HTTP/JSON API; seeded once from the ConfigMap; every write atomic |
| | | `exporter` | the store's loopback-only counters → Prometheus on :9187 |
| | | `backup` | an hourly logical dump over loopback to the separate `backups` PVC, newest six kept |
| data | `cache` ×1 | `cache` | a RESP (Redis-protocol) key-value server, password-protected, deliberately non-persistent |
| | | `exporter` | the cache's loopback-only counters → Prometheus on :9121 |

## Quick start (sc)

```bash
make secrets     # random values into manifests/03-secrets.local.yaml (gitignored; never overwritten)
make validate    # offline checks — YAML, embedded configs, restricted hygiene, images
make deploy      # sc apply in dependency order, waiting for each tier
NODE=<node> make test   # through the LoadBalancer address and the HTTPRoute host
make status
```

Or by hand, as on OpenShift:

```bash
make secrets                                  # first: kustomization.yaml applies the generated file
sc apply -k manifests/
sc get pods -n storefront                     # every pod n/n Ready
sc logs <api pod> -c access-log -n storefront
sc describe pod <pod> -n storefront
sc rsh -n storefront <pod>                    # no shell in the image: sc exec … -- /bin/stormstorefront version
sc get hpa,pdb,networkpolicy,httproute -n storefront
```

`make undeploy` keeps the PVCs; `make destroy` deletes the project.

## What the test proves

`stormstorefront-test` (`test/`, the [test standard](https://github.com/glennswest/stormcentral/blob/main/docs/test-standard.md))
runs on every test machine:

- **short** — the manifests validate offline (the same checks as
  `make validate`), and the apiserver serves every kind they use.
- **medium** (required for a release, like short) — the storefront deployed
  into a project of the run's own, exactly as `sc apply -k manifests/` makes
  it, then:
  - every tier n/n ready in order (data, api, web, edge), and the **report of
    every pod with its containers** (`/results/pods.txt`): 10 pods, 27
    containers ready, 8 init containers completed, every pod more than one
    container;
  - the objects `sc get hpa,pdb,networkpolicy,httproute,svc,pvc` shows;
  - every path through the edge (`/healthz`, `/`, `/version.json`,
    `/api/healthz`, `/api/readyz`, `/api/products`, `/api/metrics`), the
    catalog served from the cache, requests spread over more than one api
    and web pod;
  - the edge through Service DNS, through the **LoadBalancer address**
    (stormlb's), and through the **HTTPRoute host** on the node's :80;
  - **stormlb's frontends and backend config** (stormlb#37): the address's
    requests spread over both edge pods and every web and api pod; the
    route's `/` over every web pod and `/api/` over every api pod with no web
    hop; the route's status `Accepted` and `ResolvedRefs`;
  - **every container's metrics** (11 endpoints, each exporter seeing its
    neighbour up);
  - `logs -c` of the sidecars, a backup written;
  - an order **surviving a `postgres` pod restart** (the PVC keeps it); the
    API **surviving a `cache` restart** (no API restart, the cache used
    again); a **killed `web` pod replaced** with no failed request through
    the edge, the address or the route; **`web` scaled 3→6→3** with the edge
    and the route's pool following each step; a **ConfigMap edit
    reloading the edge**;
  - then every object deleted, and **nothing left**: no pod, no claim, no
    PersistentVolume, and no engine volume (`storage.storm.io/v1`) naming a
    claim, a container or the project (stormblock#649's traceability rule).
- **long** — medium with the disruptions in five timed waves; a wave more
  than twice as slow as the first fails it.

A check that fails because a platform piece is not released yet is a
**gap**: it fails, and its line names the issue (`"gap": "rustkube#321"`
— UID assignment for restricted pods; `"gap": "stormlb#37"` — LoadBalancer
addresses and the route as backend config). Once a release carries it, the
same check passes as written.

## Layout

```
src/          the binary: one module per role (see CLAUDE.md)
manifests/    numbered for apply order; kustomization.yaml ties them together
scripts/      validate, make-secrets, deploy, smoke-test
test/         stormstorefront-test (Containerfile, build.sh, requires.toml, src/)
deploy/       check.sh — the full sc-build check
docs/         ARCHITECTURE.md (why it is shaped this way)
              CONFIGURATION.md (env keys, defaults, ports, APIs, edge.toml)
              STORMCOS-NOTES.md (what differs from OpenShift, and the traps)
```

## Requirements

- stormcos with rustkube, stormlb's router (for the HTTPRoute) and
  the built-in stormblock driver (the default StorageClass `stormblock`, for the two PVCs).
- The `stormstorefront` image in the node's release (or its registry).
- stormlb with a LoadBalancer pool (`lb_pool`, stormlb#37) for
  `svc/edge-lb`; without one it stays `<pending>` — use the HTTPRoute, or
  `make forward`.
- UID assignment for restricted pods (rustkube#321): the manifests pin no
  UID, as on OpenShift.
