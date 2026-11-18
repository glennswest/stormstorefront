# Changelog

## [Unreleased]

### 2026-11-18
- **docs:** docs/CONFIGURATION.md (env keys and defaults, ports, edge.toml, api and store endpoints) from the code; storage described as the built-in stormblock driver, not stormblock-csi (README, ARCHITECTURE, STORMCOS-NOTES, manifest comment); layout lists updated.
- **feat:** `stormstorefront`, one static Rust binary with every container's
  role (#1): `edge` (L7 path routing, per-pod balancing over headless DNS,
  health checks, cookie affinity, least-connections, retries, seamless
  reload), `config-reloader`, `validate-config`, `web`, `content-init`,
  `content-sync`, `exporter`, `api` (the original api.py's endpoints plus
  orders), `access-log`, `cache-warmer`, `wait-for-deps`, `store` (the
  catalog and orders on a PVC, atomic writes, bearer auth), `backup`, `cache`
  (RESP server with AUTH), `sleep`, `validate`, `version`.
- **feat:** manifests: the original's topology on stormcos — project
  `storefront`; edge ×2, web ×3, api ×3, postgres StatefulSet, cache; 9
  Services, 7 NetworkPolicies, quota and limits, RBAC, placeholder Secrets,
  ConfigMaps, 2 HPAs, 3 PDBs, an HTTPRoute (stormlb) and a LoadBalancer
  Service, ironprom scrape config and alert rules, kustomization (#1).
- **feat:** `stormstorefront validate` / `scripts/validate.sh`: the
  original's offline checks in Rust plus stormcos's (images ours only,
  `command` required, no shell scripts, no subPath, placeholder-only
  Secrets, no fsGroup) (#1).
- **feat:** `stormstorefront-test` (`test/`): short (offline validation, API
  kinds), medium (deploy into the run's project, every pod n/n with the
  report of every pod's containers, every path through ClusterIP, DNS,
  LoadBalancer and HTTPRoute, every container's metrics, logs, backup,
  postgres restart keeps an order, the API survives a cache restart, a killed
  pod replaced, web 3→6→3, a ConfigMap edit reloads the edge, teardown with
  nothing left), long (the disruptions in timed waves) (#1).
- **docs:** README with the diagram and what the test proves,
  docs/ARCHITECTURE.md, docs/STORMCOS-NOTES.md (#1).
- **chore:** scripts (deploy, smoke-test, make-secrets), Makefile,
  deploy/check.sh (clippy -D warnings), work plan, .gitignore (#1).
