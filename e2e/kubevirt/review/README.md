# KubeVirt PoC review probes

Probe scripts written while reviewing the KubeVirt compute-driver PoC. They run
against a gateway started by `e2e/kubevirt/demo.sh` (without `--demo`) and
need `KUBECONFIG` for the kvpoc cluster and `OPENSHELL_CLI` pointing at the
`openshell` CLI registered against that gateway.

| Script | What it demonstrates |
|---|---|
| `t1-delete-window.sh` | Fence deleted before the VMI exits; launcher egress open during shutdown (gateway topology) |
| `t2-start-while-running.sh` | `sandbox start` on a Ready sandbox; stop returns before the VMI is gone |
| `t4-guest-probes.sh` | Escape probes from inside the guest as the workload user |
| `t5-concurrency.sh` | Concurrent create/delete, delete while bootstrapping, orphan fences |
| `t7-cycles.sh` | Repeated stop/start cycles and leak counts |
| `t9-dedicated-disruption.sh` | Deleting the dedicated supervisor Pod / Service under a running sandbox |
| `t10-workspace-symlink.sh` | Root `chown -R` over the workload-controlled workspace (and workspace writability) |
| `t11-fence-checks.sh` | `run-demo.sh` step 6 "selective" check is vacuous; additive NetworkPolicy reopens egress |
| `t12-reconcile.sh` | Running-state fence reconciler (M1) and whether the namespace default-deny floor is restored |
| `t13-restart-identity.sh` | Repeated stop/start persists a parseable runtime identity; claimed image vs the VM spec image |
| `t14-forged-label-ingress.sh` | L2: whether forged pair/workload labels reach the guest (guest TCP OutRsts) or the supervisor on 5500 |
| `t15-digest-pin.sh` | Digest pinning after first boot: pinned spec, restart after a retag boots the original digest, a new create gets the new one |
