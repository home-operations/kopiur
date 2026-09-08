# Feature permissions

A few of Kopiur's features need the operator to **write `Secret`s** in namespaces it manages. That is a broader privilege than a backup operator needs day to day, so those writes are **off by default**, and each one is gated behind a single Helm flag.

This page explains which features those are, which flag maps to which feature, the security trade-off, and how to recognize and fix the error you get if you turn a feature on in a custom resource but forget the matching flag.

/// info | The mental model

The operator **always** has read-only access to `Secret`s. It has to, in order to resolve your repository credentials.

The flags on this page only grant the **write** verbs, meaning `create`, `patch` and `delete`, and only for the feature you opt into.

A feature is configured in **two places**: the CRD field that turns it on, such as `spec.server`, and the Helm flag that grants the operator the RBAC to act on it. You need both, because the chart cannot know at install time which features your custom resources will eventually use.

///

## The two flags

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:316:333"
```

| CRD field you set… | …needs this Helm flag | Grants the operator `secrets` |
| --- | --- | --- |
| `spec.credentialProjection` on a `SnapshotPolicy` / `Restore` / `Maintenance` | `features.credentialProjection.enabled` | `create`, `patch`, `delete` |
| `spec.seed.credentialProjection` on a `Repository` / `ClusterRepository` (migrate-mode seeding) | `features.credentialProjection.enabled` | `create`, `patch`, `delete` |
| `spec.server` on a `Repository` / `ClusterRepository` (the kopia web-UI) | `features.kopiaUi.enabled` | `create`, `patch`, `delete` |

Both default to `false`. With both off, the operator's `secrets` access is read-only, which is exactly what a plain backup deployment needs.

### Why each feature needs it

- **Credential projection** ([Movers → projection](movers.md#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos)) copies a repository's credential `Secret` into each mover Job's namespace, so a shared `ClusterRepository` whose `Secret` lives in one place works across many workload namespaces.

  The operator therefore needs to **create** and **patch** `Secret`s in those namespaces, and to **delete** them, because a copy only needs to live as long as a mover Job can still read it. Kopiur reclaims each copy once the run that needed it has finished, rather than leaving live repository credentials sitting in your app namespaces. The `ownerReference` on the copy is a backstop for the case where the whole custom resource is removed; it is not the cleanup mechanism, because a `Snapshot` is kept for your entire retention window and waiting for garbage collection would mean waiting months.

- **Seeding from another repository** ([Repositories → `seed`](repositories.md#seed--initialize-a-new-repository-from-a-replica)) is the same mechanism aimed at a different Secret.

  A migrate-mode seed opens **two** repositories from one bootstrap pod, and the source repository's credentials usually live in another namespace. `seed.credentialProjection.enabled` copies them into the seeding Job's namespace for the run, and the copies are reclaimed when the seed finishes. Without the flag the bootstrap fails closed, with a message naming both the CRD field and the install flag, rather than launching a Job that cannot authenticate.

- **The kopia web UI server** ([Web UI](server.md)) creates a generated-auth `Secret` for the UI login, when `auth: generate`. For a `ClusterRepository` it also mirrors the repository's credentials into the server's namespace, because the Deployment's `envFrom` only reads the local namespace. Both are **deleted** on teardown or on a namespace migration, so this feature needs `create`, `patch` and `delete`.

## Enabling a feature

Turn on the flag for the feature you use, in Helm values:

```yaml
features:
  credentialProjection:
    enabled: true   # if you use spec.credentialProjection
  kopiaUi:
    enabled: true   # if you use spec.server
```

Or on the command line:

```console
$ helm upgrade kopiur oci://ghcr.io/home-operations/charts/kopiur \
    --reuse-values --set features.kopiaUi.enabled=true
```

/// warning | A real blast-radius trade-off

`create` and `delete` **cannot be scoped to a `Secret` name**. The Kubernetes authorizer cannot match a name at create time, and projected and mirrored names are derived per run.

So enabling a flag lets the operator write, and for `kopiaUi` also delete, a `Secret` in **any namespace it manages**. That is the price of the convenience.

Leave a flag `false` and the operator simply cannot perform that feature's writes; you manage the `Secret`s yourself instead. A projected or mirrored copy in namespace `X` is readable by anything that can already read `Secret`s in `X`, which is no different from placing it there by hand.

///

/// warning | Don't disable `features.kopiaUi.enabled` while a `spec.server` is live

Tearing down a server, or letting a `ClusterRepository` finalizer clean one up, **deletes** the generated-auth `Secret` and the mirrored credentials. If you revoke the flag first, that cleanup gets a `403` and the leftover `Secret`s linger.

Remove `spec.server` from the repositories first, let the operator tear the server down, then disable the flag.

///

## Symptom → fix

If you enable a feature in a custom resource while the matching flag is still `false`, RBAC forbids the operator's write and the affected resource degrades with a clear, actionable condition. It does **not** crash, and it heals the moment you grant the flag.

Here is what you will see:

```console
$ kubectl describe snapshotpolicy app-backup -n apps
...
  Message: the operator is not permitted to write the projected credentials
           Secret `app-backup-...-creds-0` in namespace `apps` (HTTP 403).
           Credential projection needs cluster-wide `secrets` create/patch RBAC.
           Fix: set `features.credentialProjection.enabled: true` in the Helm
           chart, or disable `spec.credentialProjection` ...
```

```console
$ kubectl describe repository nas-primary -n apps
...
  Message: the operator is not permitted to write the kopia web-UI Secret
           `nas-primary-kopia-ui-auth` in namespace `apps` (HTTP 403). The kopia
           web-UI server (`spec.server`) needs `secrets` create/patch/delete RBAC.
           Fix: set `features.kopiaUi.enabled: true` in the Helm chart, or remove
           `spec.server` ...
```

| Symptom | Cause | Fix |
| --- | --- | --- |
| `SnapshotPolicy`/`Restore`/`Maintenance` status shows a `403` about a *projected credentials Secret* | `spec.credentialProjection` is on but `features.credentialProjection.enabled` is `false` | Set `features.credentialProjection.enabled: true` (or manage the `Secret` yourself and drop `spec.credentialProjection`) |
| `Repository`/`ClusterRepository` bootstrap fails naming `spec.seed.credentialProjection` and the install flag | `spec.seed.credentialProjection.enabled` is on but `features.credentialProjection.enabled` is `false` | Set `features.credentialProjection.enabled: true` (or put the source repository's Secrets in the seeding Job's namespace yourself and drop the block) |
| `Repository`/`ClusterRepository` status shows a `403` about a *kopia web-UI Secret* | `spec.server` is set but `features.kopiaUi.enabled` is `false` | Set `features.kopiaUi.enabled: true` (or remove `spec.server`) |

After you grant the flag, the operator re-reconciles and the condition clears on its own. No restart or manual retrigger is needed.

## See also

- [Movers, RBAC & credentials](movers.md) covers the full credential-projection model.
- [Web UI (kopia server)](server.md) is the `spec.server` feature guide.
- [Helm chart values](configuration.md#feature-permissions) is where these flags live.
