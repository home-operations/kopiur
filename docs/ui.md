# Web console (kopiur-ui)

`kopiur-ui` is an optional in-cluster web console for Kopiur: a fleet view of
your repositories, policies, schedules, snapshots, restores, maintenance and
replication, plus the actions you would otherwise run with `kubectl kopiur` —
take a snapshot now, start a restore, suspend a schedule, request maintenance,
browse the files inside a snapshot.

It is **off by default**. Turn it on with `ui.enabled: true`.

/// info | Not the same thing as `spec.server`

This page is about **Kopiur's own console**, which reads and writes Kopiur's
CRDs. It is not kopia's built-in web UI, which a `Repository` can run through
[`spec.server`](server.md) to serve a repository to kopia clients. The two solve
different problems and can coexist.

///

## What it is, and what it is not

**It is** a view onto Kopiur's Kubernetes objects, rendered for people who would
rather not read YAML, and a small set of actions that create or patch those same
objects.

**It is not** an authentication system. It has no login page, no user database,
no password, no session cookie of its own. It learns who you are from a header
that **your** authenticating proxy sets, and then it asks Kubernetes what you are
allowed to do. Everything it shows you and everything it does on your behalf is a
request the apiserver authorized *as you*.

**It is not** a second permission model, either. There is no admin flag, no
role dropdown, no per-user config inside the console. Access is Kubernetes RBAC
and nothing else, so the same bindings that govern `kubectl` govern the console.

## The trust model, in plain words

Three sentences, and every other rule on this page follows from them.

1. **Your proxy is the trust boundary.** It authenticates the person and tells
   the console who they are, in a header.
2. **The console impersonates that person.** Every apiserver call it makes on
   your behalf carries `Impersonate-User` and `Impersonate-Group` for you — it
   never reads the cluster "as itself" on your behalf.
3. **Kubernetes decides.** The apiserver authorizes the impersonated request. If
   you are not bound to a role, you get a refusal; the console cannot overrule it
   and does not try.

The consequence is worth stating on its own, because it is the thing people get
wrong: **anything that can reach the console's port and set the identity header
is whoever that header says.** That is why the console requires a shared secret
from the proxy, why the chart renders no Ingress, and why the Service is
`ClusterIP`.

```mermaid
flowchart LR
  U[Person in a browser] -->|TLS| I[Your Ingress / HTTPRoute]
  I --> P[Your auth proxy<br/>oauth2-proxy, Envoy Gateway, ...]
  P -->|user + groups headers<br/>X-Kopiur-Proxy-Token| C[kopiur-ui<br/>ClusterIP :8090]
  C -->|Impersonate-User<br/>Impersonate-Group| A[kube-apiserver]
  A -->|allowed / forbidden| C
```

## Identity and access

This is the part that takes the most care to set up, so it gets the most care
here. Getting someone from "signed in to my IdP" to "allowed to take a snapshot
in the console" involves **five hops**, and each one is controlled by exactly
one setting:

```mermaid
flowchart TB
  IdP["1. Your IdP<br/>(Authelia, Keycloak, Google, ...)<br/>issues a token with the user's groups"]
  P["2. Your proxy<br/>(oauth2-proxy, Envoy Gateway, ...)<br/>copies user + groups into headers"]
  R["3. kopiur-ui reads the headers<br/>ui.auth.userHeader / groupsHeader<br/>ui.auth.groupsFormat"]
  F["4. kopiur-ui filters the groups<br/>ui.auth.allowedGroups (optional)"]
  K["5. Kubernetes RBAC decides<br/>ui.rbac.bindings, or your own ClusterRoleBinding"]
  IdP --> P --> R --> F --> K
```

If a hop is misconfigured, everything after it gets the wrong input, and the
symptom usually shows up at hop 5 as **Forbidden**. So when access is wrong,
work through the hops in order — [Check it worked](#check-it-worked) does
exactly that.

### Three settings, three different jobs

Most confusion comes from mixing these up:

| Job | Setting | What it does | What it does **not** do |
|---|---|---|---|
| **Read** the groups | `ui.auth.groupsHeader`, `ui.auth.groupsFormat` (+ `groupsSeparator`) | Tells the console which header holds the groups and how that header is encoded. | Grant or restrict anything. |
| **Restrict** the groups | `ui.auth.allowedGroups` | Optional. Drops every group *not* on the list before it reaches Kubernetes. Use it when your IdP sends dozens of groups and only a few matter. | **Grant access.** Putting `Admins` here gives Admins nothing. |
| **Grant** access | `ui.rbac.bindings`, or a ClusterRoleBinding you write | Says what a group (or user) may do: view, operate, or both. | — this is the only setting that grants anything. |

So "how do I let my `Admins` group use the console?" is answered by the third
row, not the second:

```yaml
ui:
  rbac:
    bindings:
      - role: user # view + operate backups
        groups: [Admins]
```

### Step 1: tell the console how your proxy sends identity

Set `ui.auth.userHeader` and `ui.auth.groupsHeader` to the header names your
proxy uses, and `ui.auth.groupsFormat` to how it encodes the group list:

| Your proxy | Typical headers | `groupsFormat` | What the groups header looks like |
|---|---|---|---|
| oauth2-proxy | `X-Forwarded-User`, `X-Forwarded-Groups` | `delimited` (the default), `groupsSeparator: ","` | `Admins,Domain Users` |
| Envoy Gateway `SecurityPolicy` (`claimToHeaders`) | whatever you name them, e.g. `x-user-username`, `x-user-groups` | `base64Json` | `WyJBZG1pbnMiLCJEb21haW4gVXNlcnMiXQ==` |
| A proxy that sends a raw JSON array | — | `json` | `["Admins","Domain Users"]` |
| A proxy that uses another separator | — | `delimited`, `groupsSeparator: ";"` | `Admins;Domain Users` |

/// warning | Envoy Gateway base64-encodes the groups claim

Envoy's `claimToHeaders` copies a plain string claim as-is, but encodes any
**list or object** claim as base64 JSON — and there is no option to change that.
`groups` is a list, so with Envoy Gateway you must set
`ui.auth.groupsFormat: base64Json`. If you leave it at `delimited`, the console
reads the whole blob as **one** group named something like `WyJEb21h…`, which
matches no binding. The console logs a warning naming this setting the first
time it sees such a value.

///

Group names are used exactly as sent: case-sensitive, and spaces inside a name
(`Domain Users`) are kept. Surrounding whitespace is trimmed.

The header names must also be ones **only your proxy can set**. That is what the
shared token is for — see [The proxy shared secret](#the-proxy-shared-secret).

Complete, working wiring for both common proxies is in
[Deploying it](#deploying-it).

### Step 2: optionally, restrict which groups count

Leave `ui.auth.allowedGroups` empty unless you have a reason. Set it, and:

- every group **not** on the list is dropped before the console talks to
  Kubernetes (the user menu has a collapsed **N groups not forwarded** line
  that opens to their names, so it is never silent);
- the same list is pinned into the console's own RBAC, so Kubernetes refuses
  any other group even if the console's parsing were somehow fooled.

Every group you bind in step 3 must also be on this list, or it is dropped
before Kubernetes can match the binding. The chart refuses to render a
`ui.rbac.bindings` entry that would be dropped this way.

### Step 3: grant access

Nobody can use the console until you bind a role — see
[Enabling it grants nobody anything](#enabling-it-grants-nobody-anything). Do it
in your values:

```yaml
ui:
  rbac:
    bindings:
      - role: user # view + operate: snapshots, restores, suspend, maintenance
        groups: [Admins]
      - role: viewer # read-only
        groups: [Domain Users]
        users: [alice] # one person, by the name your proxy puts in userHeader
```

Each entry becomes one ClusterRoleBinding to the role `<release>-ui-<role>` —
`kopiur-ui-user` when the Helm release is named `kopiur`, as everywhere on this
page. The roles are
described in [The four roles](#the-four-roles). `browse` is deliberately not
offered here — it grants repository credentials, so you bind it by hand, per
namespace.

Or write the ClusterRoleBinding yourself, if you keep RBAC outside the Kopiur
release — [The four roles](#the-four-roles) has the manifest.

### Check it worked

Open the console and click your name at the bottom of the sidebar. The panel
shows exactly what the console read and what Kubernetes allows:

| What you see | What it means | Fix |
|---|---|---|
| **Groups** lists your IdP groups, and actions are permitted | Everything works. | — |
| **Groups** shows one long unreadable value, e.g. `WyJEb21h…` | The groups header is base64 JSON but is being read as `delimited`. | `ui.auth.groupsFormat: base64Json` (Envoy Gateway). |
| **Groups** shows only `system:authenticated` | No groups arrived. Either the proxy is not sending the groups header, or `ui.auth.groupsHeader` names a different header. | Check the header name; request the `groups` scope from your IdP. |
| **N groups not forwarded** (open it) names the group you expected | It arrived, but `ui.auth.allowedGroups` dropped it. | Add it to `allowedGroups`, or empty `allowedGroups`. |
| Groups look right, but every screen says **Forbidden** | No role is bound to those groups — or one was just added: permission answers are cached for up to `ui.limits.sarTtl` (60s). | Add a `ui.rbac.bindings` entry (step 3), wait a minute, reload. |
| Every request fails with `invalid-header-value` | A header is not in the format configured. The error names the header and the format. | Set `groupsFormat` to what your proxy sends (step 1). |

To check the Kubernetes side without the console, ask the apiserver directly as
the identity the console will impersonate (substitute your user and groups;
`system:authenticated` is always added):

```console
$ kubectl auth can-i create snapshots.kopiur.home-operations.com -A \
    --as=jane --as-group=Admins --as-group=system:authenticated
yes
$ kubectl get clusterrolebindings -l app.kubernetes.io/component=ui
```

`allowedGroups` never blocks `system:authenticated`: the console always adds it,
and the chart always allows it.

## Enabling it grants nobody anything

This is the first surprise, and it is deliberate.

Installing the console creates the roles; it binds **nobody** to them unless you
list people in `ui.rbac.bindings`. Who may operate your backups is not a chart
default. So without that, the first person to sign in sees a console that loads
and then reports **Forbidden on every screen**.

That is the correct first experience, not a broken install. Fix it by binding a
role — [Step 3](#step-3-grant-access). A console that worked for the first person through the door
would be a console that worked for everyone who could reach the proxy.

## The four roles

The chart renders the first three roles while `ui.enabled` and
`ui.rbac.userRoles` are both true, and `kopiur-ui-browse` only with
`ui.rbac.browseRole: true`. Bind the first three with `ui.rbac.bindings`, or by
hand; `kopiur-ui-browse` is always bound by hand.

The names assume the Helm release is called `kopiur`. For another release name
they are `<release>-ui-viewer`, `<release>-ui-editor`, `<release>-ui-user` and
`<release>-ui-browse` — check with `kubectl get clusterroles | grep ui-`.

| Role | What it grants | Bind it |
|---|---|---|
| `kopiur-ui-viewer` | `get`/`list`/`watch` on the nine Kopiur CRDs, `get`/`list` on CRDs, `list` on events. Read-only. | ClusterRoleBinding |
| `kopiur-ui-editor` | The action buttons: create/delete `Snapshot`, create `Restore`, `patch` on the seven patched kinds, `list` PVCs. | ClusterRoleBinding |
| `kopiur-ui-user` | **Both of the above**, by aggregation. This is the one to bind for an operator. | ClusterRoleBinding |
| `kopiur-ui-browse` | The in-snapshot file browser. **Not** aggregated into `kopiur-ui-user` — see the warning below. | **RoleBinding, per namespace** |

There is also a small namespaced `kopiur-ui-doctor` Role in the operator's own
namespace, which lets doctor's "controller running" check list the operator
Deployment. Without it that one check degrades to a warning; nothing else
changes.

Bind `kopiur-ui-user` cluster-wide, not per namespace: the console is a fleet
view, and a viewer scoped to one namespace would report a healthy cluster while
another namespace burned.

```yaml
--8<-- "deploy/examples/45-web-ui.yaml:bindings"
```

### Granting browse grants that namespace's repository credentials

/// danger | Read this before setting `ui.rbac.browseRole: true`

Browsing the files inside a snapshot runs a **session pod** — a mover Job — and
`pods/exec`s kopia into it. That pod loads the repository's credentials from its
own environment, and Kubernetes RBAC **cannot narrow `pods/exec create` to one
pod**.

So anyone you bind `kopiur-ui-browse` to in a namespace can exec into that
session pod and run `env` to print the repository password and backend keys.
**Granting browse in a namespace grants that namespace's repository
credentials.**

(The role grants **both** `create` and `get` on `pods/exec`, because Kubernetes
authorizes a subresource by HTTP method and an exec over WebSocket is a `GET`
upgrade. Neither verb narrows the grant — both are the same power.)

Bind it per namespace, with a `RoleBinding`, to people you would hand those
credentials to anyway. Never with a `ClusterRoleBinding` — that is every
namespace's credentials at once.

///

This is why the role is shipped separately instead of being folded into
`kopiur-ui-user`: a grant with that consequence has to be a deliberate act, not
something that arrives with the read-only role.

The same is true of `kubectl kopiur ls`/`cat`/`download` — see
[Inspecting & browsing snapshots](cli/browse.md#the-security-model). It is the
same session-pod mechanism and the same blast radius.

#### The one mitigation RBAC cannot express

`ui.rbac.execPolicy.enabled: true` renders a `ValidatingAdmissionPolicy` that
narrows `pods/exec` CONNECT, for the subjects you list, to pods named
`kopiur-browse-*` running `/usr/local/bin/kopia`. That turns the console's closed
command set into one the **apiserver** enforces.

It stops `env` and a shell. It does not stop someone from running kopia commands
against that repository, and it does not un-grant anything above. Treat it as a
useful narrowing, not as permission to bind browse more widely.

/// warning | Not yet exercised against a live apiserver

The policy is off by default and its CEL expression has never been evaluated by a
real apiserver in our test suite. It **fails closed** — a wrong expression breaks
browsing rather than exposing anything — but that is the only reassurance we can
give it today. Enable it in a non-production cluster first and confirm browsing
still works.

///

## The proxy shared secret

In header mode the console additionally requires a shared token, sent by the
proxy as `X-Kopiur-Proxy-Token` and compared in constant time against
`ui.auth.proxySecret`.

The reason is the trust model's consequence again. Identity headers are only
trustworthy if nothing *else* can set them, and in a cluster plenty of things
can: any pod that can reach the Service, and anyone who can use the apiserver's
`services/proxy` subresource. Without a token, such a caller sends
`X-Forwarded-User: alice` and is authorized as Alice — including as an Alice
bound to `kopiur-ui-editor`.

You create the Secret and configure the proxy to send it; the chart only mounts
it.

```yaml
--8<-- "deploy/examples/45-web-ui.yaml:proxy-secret"
```

### The five configurations the chart refuses

Each of these renders a console that *works*, and quietly lets the wrong person
act as someone else. So the chart `fail`s at render time rather than shipping
them, and each message names the value to set:

1. **`ui.auth.userHeader` set, `ui.auth.proxySecret.existingSecret` empty** —
   header mode with nothing proving the headers came from your proxy. Override
   with `ui.auth.acknowledgeNoProxySecret: true` only when something else (a mesh
   with mTLS, a `NetworkPolicy`) genuinely closes that hole.
2. **No identity source at all** — neither a header nor an anonymous identity.
   Every request would then run as the console's own ServiceAccount, which can
   impersonate anyone.
3. **`ui.auth.anonymous.enabled` with no `ui.auth.anonymous.user`** — anonymous
   mode with nobody to be.
4. **`ui.auth.anonymous.fallback` with no anonymous identity configured** — a
   header-less request would have nobody to fall back to.
5. **A published Service plus the acknowledgement** — `ui.service.type` is not
   `ClusterIP` while `ui.auth.acknowledgeNoProxySecret` is true and a user header
   is configured. That is an identity-header console on the network with nothing
   to prove a request came from your proxy.

/// warning | The fifth refusal only sees the one route the chart controls

`ui.service.type` is the only publication route the chart can observe. Keep
`ui.service.type: ClusterIP` and point your **own** Ingress, Gateway
`HTTPRoute`, or even a `kubectl port-forward` at the Service, and you have
exactly the same open console — by a route the chart cannot see and will not
refuse.

So read `ui.auth.acknowledgeNoProxySecret: true` as a statement: *"I have closed
this another way."* If you have not, the acknowledgement protects nothing. The
honest closures are a proxy shared secret (best — and it makes the
acknowledgement unnecessary), or a `NetworkPolicy` / service-mesh policy that
genuinely admits only your proxy. `ui.networkPolicy.enabled` renders the latter
for you.

///

The chart also refuses three configurations that would be *inert* rather than
unsafe: `ui.enabled` with `installScope: namespaced` (impersonation and the
console's cluster-wide reads cannot be expressed in a namespaced Role, so it
would 403 on its first request), `ui.networkPolicy.enabled` with an empty
`proxySelector` (a policy admitting no peer), and `ui.rbac.execPolicy.enabled`
with no `subjects` (a policy matching nobody while appearing to restrict exec).

## Anonymous mode

Set `ui.auth.anonymous.enabled: true` with a `user` (and optionally `groups`) and
every request runs as that one fixed identity, with no header read at all. The
console's impersonate rule is then pinned by name to exactly that identity, so it
cannot become anyone else even if header parsing were fooled.

This is legitimate in two situations:

- **An outer layer has already authenticated everyone identically** — a
  single-user homelab behind an authenticating reverse proxy that does not
  forward identity, say. Everyone who gets through is the same person anyway.
- **A read-only demo or kiosk**, where the fixed identity is bound only to
  `kopiur-ui-viewer`.

It is not legitimate as a shortcut past configuring your proxy: bind that
identity to `kopiur-ui-editor` and everyone who reaches the console can delete
snapshots as one anonymous user, with nothing in the audit trail to tell them
apart.

`ui.auth.anonymous.fallback: true` is the third mode: header mode that serves a
*header-less* request as the anonymous identity instead of answering 401. It is
off by default and never implicit, because a silent downgrade from "the proxy
said who you are" to "everyone is this fixed user" is precisely the failure the
flag exists to make deliberate. The
`kopiur_ui_requests_total{identity_source=...}` metric makes downgrades visible.

## How a browse session works, and what it costs

1. You open a snapshot's file browser and press **start session**. That is a
   `POST`, because it writes to your cluster.
2. The console creates a `Job` named `kopiur-browse-<repo>-<hash>` in the
   snapshot's namespace, running the same mover image the operator uses, which
   connects to the repository **read-only** and idles.
3. Listing a directory or downloading a file `pods/exec`s kopia inside that pod —
   as you, not as the console.
4. The session is keyed by repository and **shared**: a second browser tab, or
   another person browsing the same repository, attaches to the same pod rather
   than starting a second one.
5. It expires after `ui.session.ttl` (default `15m`) idle, and the cluster
   garbage-collects the finished Job shortly after. **Stop session** ends it
   immediately.

The costs, so nothing is a surprise: one pod and one repository connection per
repository being browsed, for as long as someone is browsing plus the TTL; a
`Job` object in the snapshot's namespace; and a first-request delay of a few
seconds while the pod schedules and connects (`ui.session.readyTimeout`, default
`300s`, bounds that wait).

Reads never start a session. `GET …/tree` on a snapshot with no session answers
`409` and tells you to start one — creating a pod is a real cost and takes a
deliberate action.

### Browsing a ClusterRepository from another namespace

The session pod loads the repository's credentials from its own namespace,
because a pod cannot load a Secret from anywhere else. A `ClusterRepository`
usually keeps its Secret in the operator's namespace, so for a snapshot in an app
namespace the session asks for a **copy**, and the operator writes it. The
console and the person browsing never touch the Secret.

The operator copies it only under the gates a backup's
[credential projection](movers.md) passes, checked again against the live
objects:

- the `ClusterRepository` sets `spec.credentialProjection.allowed: true`;
- the run that took the snapshot opted in, through its `SnapshotPolicy`'s
  `spec.credentialProjection.enabled: true` (a snapshot taken before that stays
  unbrowsable this way: browsing never widens what the backup was allowed);
- the snapshot's namespace is in the repository's `allowedNamespaces`;
- the chart grants the operator Secret writes
  (`features.credentialProjection.enabled`).

The copy is named `<session job>-creds-<n>` and belongs to the session Job, so it
is deleted with the session: when you stop it, when it expires, or with the
repository. When a gate is closed the console says which, and the session Job
carries a Warning Event saying the same. The operator checks a refused session
again every 15 seconds while it lives, so fixing the gate and retrying lets it
start without ending the session first. If you would rather not copy credentials
at all, `kubectl kopiur browse --local` reads from your own machine instead.

## Caps, and what a user sees when they hit one

The console is bounded everywhere it fans out, so one person cannot exhaust it
for everyone. Each cap answers with a specific problem document rather than a
timeout or a crash.

| Value | Default | What happens at the limit |
|---|---|---|
| `ui.session.maxExecPerIdentity` | 4 | `429` `too-many-requests` naming the per-identity cap. |
| `ui.session.maxExecGlobal` | 64 | `429` `too-many-requests` naming the process-wide cap. |
| `ui.session.maxStarts` | 4 | Concurrent session *creations*; further starts queue. |
| `ui.download.maxBytes` | 1 GiB | `413` `download-too-large`, checked **before** streaming starts. |
| `ui.download.chunkTimeout` | `60s` | A transfer that makes no progress for this long is abandoned. Not a total-transfer budget — a real restore outlives any fixed deadline. |
| `ui.limits.maxManifestBytes` | 64 MiB | `422` `directory-too-large` (one directory) or `catalog-too-large` (the snapshot's catalog). A million-entry directory must fail as a refusal, not an OOM kill. |
| `ui.limits.snapshotListCap` | 5000 | `422` `list-too-large` when a listing's **filter** matched more rows than this. A cap rather than a silent truncation — a snapshots table that quietly dropped rows would be read as "these are all my backups". |

`ui.limits.sarTtl` (default `60s`) is how long a `SubjectAccessReview` answer is
reused. Short by design: a revoked binding must stop hiding behind the cache
quickly.

## What the console never does

- **It never reads Secrets.** Its ClusterRole has no `secrets` rule at all. That
  is the point of impersonation: credentials reach a session pod through the
  Job's environment, never through the console. A copy a session needs is
  written by the operator, never by the console.
- **It never execs outside a session pod.** `pods/exec` is not in the console's
  own ClusterRole; the exec is performed *as you*, under `kopiur-ui-browse`.
- **It never creates an Ingress**, in any mode. Your edge is yours.
- **It never reads inbound `Impersonate-*` headers.** A caller cannot ask to be
  impersonated as someone else; only the configured identity headers are read.
- **It never impersonates a `system:` principal.** `system:masters` is refused
  unconditionally, and every other `system:` user or group is rejected except
  `system:authenticated`.
- **It never logs credentials.** Identity middleware strips `Cookie`,
  `Authorization` and `X-Forwarded-Access-Token` before any handler sees them,
  request tracing records no headers, and pod log tails quoted in error responses
  are redacted of `AWS_`/`KEY`-like tokens.

## Deploying it

The full worked example is `deploy/examples/45-web-ui.yaml`. It holds two
complete variants — pick the one that matches your proxy:

- [With oauth2-proxy](#with-oauth2-proxy) — a separate proxy Deployment in front
  of the console; groups arrive comma-separated.
- [With Envoy Gateway](#with-envoy-gateway) — Envoy's own OIDC login, no extra
  Deployment; groups arrive base64-encoded.

Either way, the proxy is **your** wiring. Kopiur owns the console Deployment,
its `ClusterIP` Service, the console's ServiceAccount and impersonation
ClusterRole, and the four human roles. You own the proxy, the token Secret, the
edge, the TLS certificate, and the decision about who gets which role.

### With oauth2-proxy

The Helm values:

```yaml
--8<-- "deploy/examples/45-web-ui.yaml:values"
```

Create the shared token in both namespaces, as shown in
[The proxy shared secret](#the-proxy-shared-secret). Then the proxy, which logs the user in and sets the headers:

```yaml
--8<-- "deploy/examples/45-web-ui.yaml:oauth2-proxy"
```

Point your edge at the **proxy**, never at the console Service:

```yaml
--8<-- "deploy/examples/45-web-ui.yaml:ingress"
```

or, with Gateway API:

```yaml
--8<-- "deploy/examples/45-web-ui.yaml:httproute"
```

Then bind roles — [Step 3](#step-3-grant-access).

### With Envoy Gateway

Envoy Gateway can be the authenticating proxy itself: a `SecurityPolicy` on the
console's `HTTPRoute` logs the user in with OIDC (Authelia here; any OIDC
provider works) and copies ID-token claims into headers with `claimToHeaders`.
There is no proxy Deployment, and the route points straight at the console.

Two things differ from oauth2-proxy, and both are handled below:

1. **The groups header is base64 JSON**, because Envoy encodes every list claim
   that way. Hence `groupsFormat: base64Json`.
2. **Envoy injects the shared token itself**, from a Kubernetes Secret, with an
   `HTTPRouteFilter` (`credentialInjection`) that the `HTTPRoute` references.
   The token stays in the Secret and never appears in the route manifest. This
   needs **Envoy Gateway v1.4 or later**; for older versions see
   [the fallback](#older-envoy-gateway-token-in-the-route) below.

The Helm values, including the bindings that grant `Admins` and `Domain Users`
their access:

```yaml
--8<-- "deploy/examples/45-web-ui.yaml:envoy-values"
```

The shared token. One Secret serves both sides — the console reads it through
`ui.auth.proxySecret`, and Envoy injects it — so its key must be `credential`,
the only key Envoy Gateway reads, and `ui.auth.proxySecret.key` says so:

```yaml
--8<-- "deploy/examples/45-web-ui.yaml:envoy-proxy-secret"
```

The route, the filter that injects the token, and the login. Each
`claimToHeaders` header must match the corresponding `ui.auth.*Header` value
above:

```yaml
--8<-- "deploy/examples/45-web-ui.yaml:envoy-gateway"
```

/// note | How the credential injection is wired

- **Secret key `credential`.** Envoy Gateway reads only that key and sends its
  value as the whole header value — store the bare token, with no `Bearer `
  in front.
- **Same namespace.** Keep the Secret in the namespace of the `HTTPRoute` and
  the `HTTPRouteFilter` (`kopiur-system` here). A Secret elsewhere needs a
  `ReferenceGrant` from that namespace.
- **`header: X-Kopiur-Proxy-Token`** — without it Envoy writes the token into
  `Authorization`.
- **`overwrite: true`** — without it (the default is `false`) Envoy keeps an
  `X-Kopiur-Proxy-Token` the *browser* sent instead of replacing it.
- **It fails closed.** If the Secret or its `credential` key is missing, Envoy
  Gateway marks the filter invalid and answers the route with a `500`; it never
  forwards a request without the token.

///


/// warning | The claims must be in the ID token, every time

Envoy reads the **ID token** (the cookie the `oidc` block stores) and writes a
header only for a claim that token actually contains. Two consequences:

- A token without `groups` reaches the console with no groups, so the user menu
  shows only `system:authenticated` and every screen is **Forbidden**.
- If a claim is missing, a header of the same name sent by the *browser* is not
  replaced. The ID token must always carry every claim you map.

**Authelia** (4.39+) puts `groups`, `email` and `preferred_username` only in the
userinfo endpoint by default, not in the ID token. Add a claims policy and use it
on the client you register for the console (Authelia's `configuration.yml`):

```yaml
identity_providers:
  oidc:
    claims_policies:
      kopiur-ui:
        id_token: [groups, email, preferred_username]
    clients:
      - client_id: kopiur-ui # = client-id in the kopiur-ui-oidc Secret
        client_name: Kopiur
        client_secret: "$pbkdf2-sha512$..." # hash of client-secret in that Secret
        claims_policy: kopiur-ui
        redirect_uris: [https://backups.example.com/oauth2/callback]
        scopes: [openid, profile, email, groups]
        authorization_policy: two_factor
```

Other IdPs have an equivalent "include in ID token" setting for each claim.

///

#### Older Envoy Gateway: token in the route

Before v1.4, Envoy Gateway cannot inject a credential from a Secret. Use this
route **instead of** the `HTTPRoute` and `HTTPRouteFilter` above; the
`SecurityPolicy` is unchanged. Its `RequestHeaderModifier` `set` replaces any
`X-Kopiur-Proxy-Token` the browser sent — but a route filter cannot read a
Secret, so the token is written into the `HTTPRoute` itself. Treat that
manifest as a secret (SOPS, or templated from your secret store), and keep the
value identical to the Secret's `credential`:

```yaml
--8<-- "deploy/examples/45-web-ui.yaml:envoy-route-header-modifier"
```


/// warning | The console image is published separately, and starts out private

`kopiur-ui` ships as its own container image, `ghcr.io/home-operations/kopiur-ui`.
A container registry creates a new package **private** on its first publish, so
the first release that ships the console needs a human to make that package
public and link it to the repository.

The symptom if that step is missed is misleading: the release pipeline is green —
it pushed the image successfully — and installs fail at image pull with an
**authorization error**, which reads like a problem with *your* credentials
rather than with the package's visibility. If `ui.enabled: true` gives you
`ImagePullBackOff` with an auth error on a fresh release, check the package's
visibility before you check your pull secrets.

///

## Which values you will actually change

Most of `ui.*` you can leave alone. These are the ones that matter:

| Value | Why you would change it |
|---|---|
| `ui.enabled` | Turn the console on. Requires `installScope: cluster`. |
| `ui.auth.userHeader` / `groupsHeader` | Match what your proxy emits. oauth2-proxy emits `X-Forwarded-User` / `X-Forwarded-Groups`. |
| `ui.auth.groupsFormat` | How the groups header is encoded: `delimited` (default, oauth2-proxy), `base64Json` (Envoy Gateway `claimToHeaders`), or `json`. See [Step 1](#step-1-tell-the-console-how-your-proxy-sends-identity). |
| `ui.auth.groupsSeparator` | Only for `delimited`: `,` for oauth2-proxy; some proxies use `;` or a pipe. |
| `ui.auth.proxySecret.existingSecret` | The Secret holding your shared token. Required in header mode. |
| `ui.auth.allowedGroups` | Optional. **Restricts** which groups are forwarded to Kubernetes; never grants. Enforced twice — the console filters, and the same list is pinned as `resourceNames` on the impersonate rule. |
| `ui.rbac.bindings` | **Grants** access: one entry per role (`user`, `viewer`, `editor`) and the groups/users that get it. Empty by default. See [Step 3](#step-3-grant-access). |
| `ui.rbac.browseRole` | The file browser. Read the warning above first. |
| `ui.networkPolicy.enabled` + `proxySelector` | Make "only the proxy can reach it" true at the network layer. |
| `ui.session.ttl` | How long an idle browse session pod lives. |
| `ui.download.maxBytes` | Raise it if people legitimately download large files through the console. |
| `ui.cache.enabled` | Leave on. Off means every read is an impersonated `LIST`, and removes the console's read grants from its ClusterRole entirely. |

## Troubleshooting, by problem type

Every refusal the console produces is an RFC 7807 document with a stable
`type` URN, and that URN is the thing a user can actually read off a failed
request — the browser shows it, and the SPA surfaces it. Find yours here.

| `type` | Status | What it means, and what to do |
|---|---|---|
| `urn:kopiur:problem:no-identity` | 401 | No identity header reached the console. The proxy is not setting `ui.auth.userHeader`, or something is reaching the console without going through the proxy. |
| `urn:kopiur:problem:proxy-secret` | 401 | The identity headers arrived but the shared token was missing or wrong. Check that the proxy sends `X-Kopiur-Proxy-Token`, and that its value matches the Secret named by `ui.auth.proxySecret`. |
| `urn:kopiur:problem:forbidden-principal` | 403 | The header named a `system:` principal (or `system:masters`). The console refuses these unconditionally. Your proxy is asserting an identity it should not. |
| `urn:kopiur:problem:too-many-groups` | 400 | More than 64 groups in the groups header. Narrow what the proxy forwards, or set `ui.auth.allowedGroups`. |
| `urn:kopiur:problem:invalid-header-value` | 400 | An identity header could not be used: it carried control or non-ASCII bytes, a value exceeded 512 bytes, or the groups header was not in the configured `ui.auth.groupsFormat` (the message names the format — usually Envoy Gateway's base64 groups with `groupsFormat` left at `delimited`, or the reverse). See [Step 1](#step-1-tell-the-console-how-your-proxy-sends-identity). |
| `urn:kopiur:problem:forbidden` | 403 | **The apiserver refused the impersonated request.** This is normal RBAC: bind `kopiur-ui-user` (or `-viewer`/`-editor`) to the user or group — with `ui.rbac.bindings` or your own binding. If you did, open the user menu and follow [Check it worked](#check-it-worked): the group may not be arriving, or `allowedGroups` may be dropping it. The `why` field carries the apiserver's own words, naming the verb and resource. |
| `urn:kopiur:problem:csrf` | 403 | A mutating request arrived without the `X-Kopiur-Request: 1` marker, without a JSON `Content-Type`, or from another site. If you are scripting against the API, send the header — or use `kubectl kopiur`, which writes the same CRs. |
| `urn:kopiur:problem:session-required` | 409 | You asked to read inside a snapshot with no browse session running. Start one — reads never create pods. |
| `urn:kopiur:problem:too-many-requests` | 429 | A concurrency cap; the message names which. Retry, or raise `ui.session.maxExecPerIdentity` / `maxExecGlobal`. |
| `urn:kopiur:problem:list-too-large` | 422 | The filter matched more rows than `ui.limits.snapshotListCap`. Narrow by repository or policy, or raise the cap. Nothing was truncated — that is the point. |
| `urn:kopiur:problem:invalid-query` | 400 | A query parameter this endpoint does not accept, usually a typo. Named in the message. A silently-ignored filter would hand you the unnarrowed answer as if it were narrowed. |
| `urn:kopiur:problem:directory-too-large` / `catalog-too-large` | 422 | The directory listing or snapshot catalog exceeded `ui.limits.maxManifestBytes`. Raise it, or browse a narrower path. |
| `urn:kopiur:problem:download-too-large` | 413 | The file is larger than `ui.download.maxBytes`. Raise it, or restore with a `Restore` CR instead. |
| `urn:kopiur:problem:download-size-unknown` | 422 | kopia reported no size for the entry, so the cap cannot be checked. Not the same as a zero-byte file. |
| `urn:kopiur:problem:kind-not-installed` | 501 | The Kopiur CRDs are missing or outdated on this cluster. |
| `urn:kopiur:problem:timeout` | 504 | A bounded wait expired — most often a browse session pod that was not ready within `ui.session.readyTimeout`. The work may still be running; retry. |
| `urn:kopiur:problem:upstream` | 502 | The apiserver, the session pod, or kopia failed or was unreachable. Usually retryable. If the `why` says `cannot get resource "pods/exec"`, it is not retryable and not a missing binding: the bound role grants `create` on `pods/exec` but not `get`, and an exec over WebSocket is a `GET` upgrade. Use the chart's `kopiur-ui-browse`, or add `get` to your hand-written role. |
| `urn:kopiur:problem:not-wired` | 500 | A kopiur-ui bug: a handler ran without the state it needs. Please report it. |

### The console pod will not become Ready

`/readyz` on the ops port (`ui.opsPort`, default `8091`) names the subsystem
holding it back rather than just failing:

The body is `ok` when it is ready, or `not ready: <reason>[, <reason>]` when it is
not — plus any non-blocking notes in parentheses (`(web: placeholder)` means the
pod is serving a placeholder SPA bundle, which is worth knowing and never worth
refusing traffic over).

| Reason | Meaning |
|---|---|
| `impersonation-unavailable` | The console's ServiceAccount may not `impersonate` on `users`/`groups`, so every request would 403 with something the caller cannot fix. Check that `ui.rbac` rendered the console's ClusterRole and that nothing trimmed the `impersonate` rule. |
| `cache-not-ready` | The reflector stores are still filling. Normal for a few seconds after start, longer on a large cluster. |
| `cache-sync-timed-out` | The stores did not fill within the startup budget. The pod recovers without a restart if the sync lands later; if it never does, the ServiceAccount probably cannot `list`/`watch` the Kopiur kinds. |
| `cache-watch-ended` | A watch stopped and nothing is refreshing the stores. Restart the pod. |

/// note | Everyone is forbidden and nothing is wrong

If `/readyz` says `ok`, the pod is Ready, and every screen says Forbidden —
that is the expected state of a freshly-enabled console with no bindings. See
[Enabling it grants nobody anything](#enabling-it-grants-nobody-anything).

///

## See also

- [RBAC reference](rbac.md) — every rule the console's ServiceAccount and the
  four human roles carry.
- [Helm chart values](configuration.md) — the full `ui.*` surface.
- [Inspecting & browsing snapshots](cli/browse.md) — the same session-pod
  mechanism from the CLI, including `--local`.
- [Web UI (kopia server)](server.md) — kopia's own UI, a different thing.
