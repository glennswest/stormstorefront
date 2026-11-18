# CLAUDE.md — stormstorefront

The owner's [openshift-storefront-example](https://github.com/glennswest/openshift-storefront-example)
on stormcos: one project (`storefront`), edge/web/api/postgres/cache pods,
every pod more than one container, restricted, offline, all Rust. It is the
workload every test machine runs to prove projects, multi-container pods,
storage, networking and `sc` behave like OpenShift (issue #1).

## Version

`0.1.0` — defined in `Cargo.toml` (root package) and `test/Cargo.toml`;
`VERSION` mirrors it. All three must match.

## Build, test, ship

Never build on the stormcentral VM. Push, then:

```bash
sc-build                         # cargo build --locked && cargo test --locked
sc-build deploy/check.sh         # + clippy -D warnings, validate, test crate
```

One image, `stormstorefront` (component kind `binaries`: `/bin/stormstorefront`,
the role by argument). Pods say `image: stormstorefront` and
`command: ["/bin/stormstorefront", "<role>"]`: a registry image carries no
image config, so `command` is required.

When an issue's work is pushed and sc-build passes:
`stormcentral component build stormstorefront --no-wait --url http://stormcentral.g8.lo`.

## Layout

```
src/main.rs        role dispatch (edge, reloader, validate-config, web, content-sync,
                   content-init, exporter, api, access-log, cache-warmer, wait-for-deps,
                   store, backup, cache, sleep, validate)
src/http.rs        minimal HTTP/1.1 server + client (std only, thread per connection)
src/edge.rs        L7 edge: path routing, per-pod balancing over headless DNS, health checks, reload
src/web.rs         static site + /api/ proxy + loopback stub_status
src/content.rs     renders the site (content-init / content-sync)
src/api.rs         the API (src/api/api.py's endpoints, plus orders backed by the store)
src/store.rs       the "postgres" tier: a durable JSON store on the PVC
src/cache.rs       the "redis" tier: a RESP key-value server; src/resp.rs is the client
src/exporter.rs    loopback stats -> Prometheus text
src/sidecars.rs    access-log, cache-warmer, wait-for-deps, backup, reloader helpers
src/validate.rs    the offline manifest checks (scripts/validate.sh runs them)
manifests/         numbered for apply order; kustomization.yaml ties them together
test/              stormstorefront-test: /test short|medium|long (test standard)
docs/              ARCHITECTURE.md, CONFIGURATION.md, STORMCOS-NOTES.md, research/
```

## Work plan

### #1 — the storefront on stormcos (P0)
- [x] Rust binary with every role (src/), 29 unit tests
- [x] manifests (same topology as the original, stormcos substitutions)
- [x] validate (Rust) + scripts/validate.sh, smoke-test.sh, deploy.sh, make-secrets.sh, Makefile
- [x] test container: short, medium, long (test/)
- [x] docs: README diagram, ARCHITECTURE, STORMCOS-NOTES, what the test proves
- [x] sc-build green with `deploy/check.sh` (clippy -D warnings); Cargo.lock committed
- [x] UID decision (master, 2026-11-18): A — the platform assigns UIDs as restricted-v2
      (rustkube#321, P0). The app pins nothing; the test names rustkube#321 as the gap
      when pods are refused as root, and passes as written once a release carries it.
- [x] stormlb as the edge (master, 2026-11-18; stormlb#37; 4a3d07d, sc-build deploy/check.sh green): LoadBalancer address from stormlb,
      HTTPRoute as the backend config (`/` → web, `/api/` → api, weights, readiness as health),
      test proves spread over both edge pods and every web/api pod (LB address and route),
      a web pod killed with no failed request on either, the route's pool following 3→6→3,
      the route's status read back; where stormlb is not there the test names stormlb#37.
- [ ] BLOCKED (item proposed --after rustkube#321 and stormlb#37, 2026-11-18): the medium test cannot pass on a node until a
      release carries rustkube#321 (UID) and stormlb#37 (address, route rules); it reports them as gaps.
- [ ] registry entry `stormstorefront` (kind binaries) — asked of the master (note, 2026-11-18; re-sent with the exact command); proof wording: test report, not namespaces/storefront (asked too)
- [ ] image build, test run on pvetest, shipped with proof
