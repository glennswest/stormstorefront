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
root"). Whether stormcos takes on OpenShift's UID-range assignment (SCC is an
open scope question, rustkube#70) is the owner's decision; until it is
decided and done, these pods do not start as written. See the issue for the
options.

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
header. What stormlb does with it:

- an **exact hostname** match (no wildcards), the first rule's first
  `backendRef`, resolved to the Service's clusterIP;
- **no TLS termination** for a pod's route: the original's
  `termination: edge` has no equivalent yet; the route is plain HTTP;
- no path matching (the edge does the path routing anyway).

The hostname is `storefront.storm1.g8.lo`; the wildcard `*.storm1.<zone>`
resolves to the node. Until microdns#12 (wildcard answers' owner name),
strict resolvers may drop the answer: `curl -H 'Host: storefront.storm1.g8.lo'
http://<node>/` proves the path meanwhile, and the test does exactly that.

The **LoadBalancer** Service takes an address from the site's pool
(install-config `networking.loadBalancerPool`, stormcos#215): Cilium's
LB-IPAM assigns it and the node answers ARP for it. A machine without a pool
leaves `edge-lb` `<pending>`, as on CRC without MetalLB; the test reports that
check `skip`, never pass.

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

## 11. Storage: stormblock-csi

Both PVCs use the default StorageClass, `stormblock` (stormblock-csi,
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
