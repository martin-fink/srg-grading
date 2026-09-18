# Hardening rollout for the small class

The application changes are intended for eight students and four assignments, with
one continuous control worker. They bound expensive feedback work while preserving
receipt-time eligibility and final submission capture. See [operations](operations.md)
for the exact budgets and recovery commands.

## Upgrade

1. Back up PostgreSQL and referenced artifacts. Apply migration 0007 and the updated
   database grants as the owner before upgrading the web and control services.
2. Upgrade web, control worker and executor together. The runner must support Linux
   `PR_SET_DUMPABLE` and inherited resource limits; supervisor protection fails closed
   if the kernel rejects the operation. No student source is trusted as a supervisor.
3. Run `gradingctl sync` with the operator credential to disable Actions on existing
   repositories. New repositories keep Actions disabled before student invitations.
4. Verify source templates fit the 512-file / 8 MiB student budget. Per-command
   execution now defaults to 30 seconds; trusted graders may request another limit
   with `grading-run --timeout-seconds`, within the assignment's overall deadline.
5. Update grade-import consumers for `grade_state` and `provisional_points`. The CSV
   `points` column contains only final scores. Failed private runs can be retried
   with an audited `gradingctl retry-private` command without changing grader code.
6. Review alerts for queue age, failed source capture, staging/artifact/database
   capacity, and pending repository locks. Receipt/task audit metadata is retained;
   source and grading budgets do not impose a total database-size quota.

Source-fetch API budgets are process-local and reset on restart. Keep one continuous
control worker for this class. Authentication cleanup is periodic in the web service.
Keep the executor staging root dedicated to its configured namespace and policy.

## Deployment changes and live acceptance

Deployment resources live in `doctor-cluster-config/modules/grading/`, outside this
repository. Application commits do not install or prove the following protections.
Use instructor-controlled accounts and disposable Jobs in a test namespace; record
the image digest, node/runtime version and observed result for each case.

| Boundary | Required deployment behavior | Acceptance case |
| --- | --- | --- |
| Network | Deny student and controller Pod ingress/egress; enforce with the actual CNI. Restrict the worker listener to the executor host. | Attempts to reach the internet, DNS, node services, metadata endpoints, other Pods and the worker API fail for both IPv4 and IPv6 where enabled. |
| Mounts and credentials | Admission permits only approved images, gVisor, expected UID/mount combinations, and restricted security contexts. Deny privileged Pods, host namespaces, hostPath, service-account tokens, extra capabilities and unrelated PVC paths. | A student process cannot read grader/control files, other runs, host files, or credentials. An intentionally disallowed Pod is rejected at admission. |
| Supervisor | The real runtime enforces non-dumpability and separates student output from the supervisor's result channel. | Attempt to open the supervisor's `/proc/PID/fd/1` and memory fails. A killed or otherwise abnormal supervisor cannot publish a successful execution envelope. Signals may cause a failed test, never trusted forged output. |
| Processes and resources | Set kubelet Pod PID limits and node/system reservations, namespace resource quotas, storage monitoring and log rotation. Retain gVisor and per-Pod CPU/memory/storage limits. | In disposable sandboxes, fork/thread, descriptor, memory, output and disk exhaustion leave the node and another student's workload healthy. Validate the inherited 128-process and 256-descriptor hard limits. |
| HTTP | Apply per-client limits and connection/body-read timeouts at the trusted reverse proxy. Reject client attempts to spoof forwarded addresses. Keep internal routes off the public proxy. | Flood login/submission/report endpoints from a test client; observe 429/503 and bounded resources while another client remains usable. |
| GitHub | Actions disabled on every student repository; no inherited organization secrets or general-purpose self-hosted runner access; organization base access remains none. | Push an added workflow using a student account. It does not execute. Existing repositories show Actions disabled after synchronization. |
| Private grading | Trusted scripts check execution exit status, and private logs remain instructor-only. | A submission passes public inputs but loops, allocates excessively or floods output on private input. The grader receives a failed execution and applies its scoring policy. A controller failure stays unresolved; an audited retry preserves its original baseline and revision. |
| Cleanup | Staging reconciler can list all relevant Pods and delete only abandoned/completed staging. | Restart the executor mid-run and delay Pod deletion. Mounted staging survives; finished/orphaned staging is eventually removed once safe. A Kubernetes API outage causes retention rather than deletion. |
| Recovery | Encrypted off-host backups include the database, referenced artifacts and necessary configuration. | Restore into an isolated environment, verify artifact digests and all final grade records, and verify that no restored worker contacts production. |

Treat missing acceptance evidence as an unverified deployment boundary. The local
tests establish application behavior, not isolation properties of the live cluster.
