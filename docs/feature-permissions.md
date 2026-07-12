# Feature permissions

A few of Kopiur's features need the operator to **write `Secret`s** in namespaces
it manages. Because that's a broader privilege than a backup operator needs for
its everyday job, those writes are **off by default** and each is gated behind a
single Helm flag. This page explains which features those are, the exact
flag↔feature mapping, the security trade-off, and how to recognise (and fix) the
error you get if you turn a feature on in a CR but forget the matching flag.

/// info | The mental model

The operator **always** has read-only access to `Secret`s — it has to, to resolve
your repository credentials. The flags on this page only grant the **write** verbs
(`create`/`patch`/`delete`), and only for the feature you opt into. A feature is
configured in **two places**: the CRD field that turns it on (e.g.
`spec.server`), and the Helm flag that grants the operator the RBAC to act on it.
Both are needed; the chart can't infer at install time which features your CRs
will eventually use.

///

## The two flags

```yaml
--8<-- "deploy/helm/kopiur/values.yaml:239:253"
```

| CRD field you set… | …needs this Helm flag | Grants the operator `secrets` |
| --- | --- | --- |
| `spec.credentialProjection` on a `SnapshotPolicy` / `Restore` / `Maintenance` | `features.credentialProjection.enabled` | `create`, `patch`, `delete` |
| `spec.server` on a `Repository` / `ClusterRepository` (the kopia web-UI) | `features.kopiaUi.enabled` | `create`, `patch`, `delete` |

Both default to `false`. With both off, the operator's `secrets` access is
read-only — exactly what a vanilla backup deployment needs.

### Why each feature needs it

- **Credential projection** ([Movers → projection](movers.md#let-kopiur-project-the-credentials-secret-recommended-for-shared-repos))
  copies a repository's credential `Secret` into each mover Job's namespace, so a
  shared `ClusterRepository` whose `Secret` lives in one place "just works" across
  many workload namespaces. The operator therefore needs to **create** and
  **patch** `Secret`s in those namespaces — and to **delete** them, because a copy
  lives only as long as a mover Job can still read it. Kopiur reclaims each copy
  once the run that needed it has finished, rather than leaving live repository
  credentials lying around in your app namespaces. The `ownerReference` on the copy
  is a backstop for the case where the whole CR is removed; it is not the cleanup
  mechanism (a `Snapshot` is retained for your entire retention window, so waiting
  for GC would mean waiting months).
- **kopia web-UI server** ([Web UI](server.md)) creates a generated-auth `Secret`
  for the UI login (`auth: generate`), and for a `ClusterRepository` also mirrors
  the repository's credentials into the server's namespace (because the
  Deployment's `envFrom` is namespace-local). Both are **deleted** on teardown or
  a namespace migration, so this feature needs `create` + `patch` + `delete`.

## Enabling a feature

Turn on the flag for the feature you use (Helm values):

```yaml
features:
  credentialProjection:
    enabled: true   # if you use spec.credentialProjection
  kopiaUi:
    enabled: true   # if you use spec.server
```

…or on the command line:

```console
$ helm upgrade kopiur deploy/helm/kopiur \
    --reuse-values --set features.kopiaUi.enabled=true
```

/// warning | A real blast-radius trade-off

`create`/`delete` **cannot be scoped to a `Secret` name** (the Kubernetes
authorizer can't match a name at create time, and projected/mirrored names are
derived per-run), so enabling a flag lets the operator write — and, for
`kopiaUi`, delete — a `Secret` in **any namespace it manages**. That's the price
of the convenience. Leave a flag `false` and the operator simply can't perform
that feature's writes; you manage the `Secret`s yourself instead. A projected or
mirrored copy in namespace `X` is readable by anything that can already read
`Secret`s in `X` — no different from placing it there by hand.

///

/// warning | Don't disable `features.kopiaUi.enabled` while a `spec.server` is live

Tearing down a server (or letting a `ClusterRepository` finalizer clean one up)
**deletes** the generated-auth `Secret` and the mirrored credentials. If you
revoke the flag first, that cleanup `403`s and the leftover `Secret`s linger.
Remove `spec.server` from the repositories first, let the operator tear the
server down, then disable the flag.

///

## Symptom → fix

If you enable a feature in a CR but the matching flag is still `false`, the
operator's write is **forbidden by RBAC** and the affected resource degrades with
a clear, actionable condition (it does **not** crash — it heals the moment you
grant the flag). What you'll see:

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
| `Repository`/`ClusterRepository` status shows a `403` about a *kopia web-UI Secret* | `spec.server` is set but `features.kopiaUi.enabled` is `false` | Set `features.kopiaUi.enabled: true` (or remove `spec.server`) |

After you grant the flag, the operator re-reconciles and the condition clears on
its own — no restart or manual retrigger needed.

## See also

- [Movers, RBAC & credentials](movers.md) — the full credential-projection model.
- [Web UI (kopia server)](server.md) — the `spec.server` feature guide.
- [Helm chart values](configuration.md#feature-permissions) — where these flags live.
