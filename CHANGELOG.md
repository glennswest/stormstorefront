# Changelog

## [Unreleased]

### 2026-11-20
- **fix:** `sc apply -k manifests/` no longer deploys the public CHANGEME passwords (#8, finding 1bcf68f63658). `kustomization.yaml` lists the generated `03-secrets.local.yaml` instead of `03-secrets.yaml`, so apply -k fails until `make secrets` has run; `validate` refuses a kustomization listing the placeholders (or not listing the generated file), a placeholder left in the generated file, and a Secret in any other manifest. `scripts/deploy.sh` applies the generated file or keeps the project's existing Secrets, and fails otherwise (it applied the placeholders with a warning); `make apply` checks first; `make secrets` never overwrites an existing file (`FORCE=1` does), so a re-run cannot rotate passwords under a running project. `make undeploy` deletes file by file (it no longer needs the generated file) and keeps the project, as it says.
- **fix:** store: a restart can no longer reuse an order id and overwrite a saved order (#7, finding 0f9a8d59a92b). `create_order` reserves the id in `next_order` before publishing the order; the order file is published no-clobber (hard link of the synced temp file), taking the next id if one is taken; `Store::open` recovers the sequence past the highest order on disk. `orders` in `/stats` counts order files only.

### 2026-11-18
- **feat:** stormlb as the edge (#1, stormlb#37): the HTTPRoute is stormlb's backend config (`/` → web, `/api/` → api, weights, timeouts, readiness as health); the LoadBalancer address is stormlb's. The test adds `loadbalancer-spread` (both edge pods, every web and api pod through the address) and `httproute-backends` (every web and api pod through the route, no web hop on `/api/`, route status `Accepted`/`ResolvedRefs`); `pod-replaced` kills a web pod under traffic on the edge, the address and the route; `scale-web` checks the route's pool follows 3→6→3.
- **feat:** test gaps: a failure caused by an unreleased platform piece names it (`"gap": "rustkube#321"` when pods are refused as root, `"gap": "stormlb#37"` for the address and the route); the LoadBalancer check fails as a gap instead of skipping.
- **docs:** the UID decision (option A, rustkube#321) and stormlb's role in README, ARCHITECTURE, STORMCOS-NOTES.
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
