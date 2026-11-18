# stormcos-specific notes

The original's [OPENSHIFT-NOTES.md](https://github.com/glennswest/openshift-storefront-example/blob/main/docs/OPENSHIFT-NOTES.md)
lists the rules `restricted-v2` imposes and the traps that come with them.
This is the same list for stormcos: what carries over unchanged, what
differs, and what the platform does not do yet.

## 1. Never pin a UID — and who assigns one

The manifests set only:

```yaml
securityContext:
  runAsNonRoot: true
  seccompProfile:
    type: RuntimeDefault
```

and never `runAsUser`, `runAsGroup` or `fsGroup`. `stormstorefront validate`
fails if any appears, and the binary tolerates any UID (it writes only to its
volumes).

**What differs.** On OpenShift the `restricted-v2` SCC gives each namespace a
UID range (`openshift.io/sa.scc.uid-range`) and runs every container as a UID
from it, with an `fsGroup` that makes volumes writable. On stormcos today
nothing assigns a UID: the kubelet runs a container as the pod's
`runAsUser`, else the image's `User`, else **0** — and a registry image (this
one included) carries no `User`. With `runAsNonRoot: true` the kubelet then
refuses the container ("runAsNonRoot is set and the container would run as
root").

**Decided (master, 2026-11-18, option A on #1):** the platform assigns UIDs as
`restricted-v2` does — each namespace gets a UID and supplemental-group
range, and admission sets `runAsUser`/`fsGroup` from it (rustkube#321). The
manifests stay as written. Until a release carries rustkube#321 the test's
deploy steps fail as a **gap** naming it (`"gap": "rustkube#321"`), and pass
unchanged once it is there.

## 2. Every listening port is above 1024

Unchanged. The edge listens on 8080 and 8404, the web tier on 8080, the API
on 8081, the store on 5432, the cache on 6379, the exporters on 9113, 9187
and 9121, the loopback stats listeners on 5433, 6380 and 8081. `:80` appears
only as the LoadBalancer Service's port. `validate` checks every
`containerPort`, and the edge refuses a privileged `listen` in its config.

## 3. HTTPRoute vs. Service type=LoadBalancer

The original's OpenShift **Route** is a Gateway API **HTTPRoute** here:
stormcos's router is stormlb on every node's :80
(stormcos `deploy/manifests/85-routes.yaml`), which demuxes on the `Host`
header. stormlb is the edge on stormcos (stormlb#37): the route's rules are
its **backend config** — `/` → `web:8080`, `/api/` → `api:8081`, each with a
`weight`, a request timeout, and each backend's health taken from its pods'
readiness. So the route reaches web and api directly; the LoadBalancer
address reaches the edge pods, which route the same way themselves. Two
frontends, one set of pods.

- `/` → web is the first rule on purpose: a router that only reads the first
  rule's first `backendRef` (stormlb before #37) still serves everything,
  because web proxies `/api/` to api itself;
- the router runs on the host, which Cilium admits to local pods as host
  traffic, so the default-deny NetworkPolicies need no hole for it (web and
  api still admit only the edge from other pods);
- **no TLS termination** for a pod's route: the original's
  `termination: edge` has no equivalent yet; the route is plain HTTP.

What the test asks of stormlb (a **gap** naming stormlb#37 where it is not
there yet): `/` spread over every web pod and `/api/` over every api pod
with no web hop, the route's `status.parents[].conditions` showing
`Accepted` and `ResolvedRefs`, a web pod killed with no failed request
through the route or the address, and the route's pool following web
3→6→3.

The hostname is `storefront.storm1.g8.lo`; the wildcard `*.storm1.<zone>`
resolves to the node. Until microdns#12 (wildcard answers' owner name),
strict resolvers may drop the answer: `curl -H 'Host: storefront.storm1.g8.lo'
http://<node>/` proves the path meanwhile, and the test does exactly that.

The **LoadBalancer** Service takes an address from the node's `lb_pool`,
which stormlb allocates and writes to `status.loadBalancer.ingress`
(stormlb#37), its backend pool the edge pods' endpoints, followed live, with
`externalTrafficPolicy: Local` keeping the client address. The test requires
both edge pods to take the address's traffic and every web and api pod to be
reached through it. Until stormlb#37 is released `edge-lb` stays `<pending>`
and those checks fail as a gap naming stormlb#37, never pass.

## 4. Pod Security Admission labels

Unchanged. The namespace carries `pod-security.kubernetes.io/enforce:
restricted`; rustkube's PodSecurity admission enforces a subset of the
restricted level from it.

## 5. `shareProcessNamespace` in the edge pod

Kept, for the same reason: the reloader signals the edge (SIGHUP, found by
its command line in `/proc`). The edge also re-reads its live file every two
seconds, so the reload does not depend on the shared PID namespace; the test
reports which path delivered it.

## 6. `readOnlyRootFilesystem` is free here

Every container sets it to `true`: nothing here writes outside its volumes
(the original needs exceptions for nginx and PostgreSQL). Note
stormpump#108: stormcos mounts a container's root writable regardless today.

## 7. ConfigMap mounts are read-only — the reload pattern

Unchanged: the ConfigMap is mounted read-only at `/etc/edge/src`, the init
container validates and copies it to the `emptyDir` at `/etc/edge/live`, the
edge reads the live copy, and the reloader replaces it on a change. No
`subPath` anywhere (`validate` refuses one): a `subPath` mount never sees the
kubelet's update. The test edits the ConfigMap and waits for every edge pod
to report the reload.

## 8. Balancing across pods, not a ClusterIP

Unchanged: the edge resolves `web-headless` and `api-headless`, one record
per ready pod, 12 slots each. Bare names resolve in the pod's own project
(`<name>.<POD_NAMESPACE>.svc.cluster.local`), so the same config serves
`storefront` and the test's project.

## 9. Images: offline and ours

Every container is `image: stormstorefront`: the node runs the release's
image of that name (or `stormstorefront:<build id>`, fetched once from forge
into the node's registry). A registry image carries no OCI config, so every
container says its `command`; `validate` refuses a missing one, an image that
is not ours, and an exec probe or hook that runs anything but the binary.

## 10. Monitoring: ironprom, not the Prometheus Operator

The original's ServiceMonitors and PrometheusRule (CRDs) become a ConfigMap,
`storefront-monitoring`, holding a Prometheus-format `scrape.yml`
(kubernetes_sd over the project's endpoints: every `metrics` port, and the
API's `/api/metrics`) and `rules.yml`. ironprom reads both formats. The
node's ironprom takes its config from stormcos
`deploy/metrics/ironprom-node.yml`; adding these jobs there is how the
storefront gets scraped on a node. "The monitoring stack" in the
NetworkPolicies is a namespace labelled `storm.io/policy-group: monitoring`;
the node's own ironprom scrapes from the host network, which Cilium admits.

## 11. Storage: the stormblock driver

Both PVCs use the default StorageClass, `stormblock` (the built-in stormblock driver,
`WaitForFirstConsumer`, `reclaimPolicy: Delete`). Deleting the claims
deletes their volumes; the test then checks that no PersistentVolume and no
engine volume (`storage.storm.io/v1`) still names a claim, a container or the
project — the traceability rule (stormblock#649).

## 12. Projects

A project is a namespace (stormcos#1020, the OpenShift model). `sc projects`
lists `storefront`; everything the example makes is in it, and deleting the
project deletes it all. The test deploys into a project of its own per run
(the runner's `extra_namespaces`), so runs never meet and a run leaves
nothing behind.

## What is intentionally *not* here

As in the original: no DeploymentConfig, no ImageStream/BuildConfig, no
Operator, no Helm. Plain manifests plus kustomize.
