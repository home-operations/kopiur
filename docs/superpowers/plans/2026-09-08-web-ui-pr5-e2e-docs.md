# Kopiur web UI — M5: end-to-end tests and documentation

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** prove the whole UI works against a real cluster under a real identity, and document the trust model plainly enough that an operator can deploy it without misunderstanding what they are granting.

**Architecture:** the e2e harness already reaches in-cluster HTTP through the apiserver Service proxy. The UI test drives that path with `X-Forwarded-User` / `X-Forwarded-Groups` plus the proxy shared secret, binds the generated human roles to two test subjects, and asserts both what a permitted user can do and what an unpermitted one cannot. The docs then say, in words, exactly what the RBAC and the session pods mean.

**Spec:** `/home/perf3ct/.claude/plans/please-help-me-plan-floating-marble.md`, section "PR5 — M5 e2e + M6 docs". The "Security model" section is the source for every claim the docs make.

**Branch:** the final milestone on `feat/web-ui-pr2-backend`. When it is green, the whole feature opens as **one** pull request.

## Global Constraints

- **Never touch a real cluster.** The harness pins an isolated kubeconfig and uses throwaway kind clusters (`scripts/with-kind.sh`, `target/e2e/`). If no kind cluster is available in the environment, say so and stop — do not fall back to the developer's kubeconfig.
- **The apiserver strips `Authorization`, `Impersonate-*` and the configured `X-Remote-*` request headers before proxying.** Use `X-Forwarded-*`; if they do not survive, fall back to an in-cluster curl Job rather than weakening the UI's header configuration.
- **Every assertion names its own failure.** An e2e assertion that fails with "expected true, got false" costs an hour; assert with a message that says which subject, which route, and what was expected.
- **Docs state consequences, not features.** Wherever a grant has a consequence the reader would not guess — most of all that granting browse grants the namespace's repository credentials — say it in the sentence that grants it.

---

### Task 1: The e2e test

**Files:** create `crates/e2e/tests/ui.rs` (feature `e2e`, `#[ignore]`), extend `crates/e2e/src/` with any helper the test needs (follow the existing builders), and the diagnostics dump.

- [ ] **Fixtures:** a repository with a policy and at least one succeeded snapshot containing `a.txt` and a `sub/` directory; a `ClusterRoleBinding` of `kopiur-ui-user` and a namespaced `RoleBinding` of `kopiur-ui-browse` to `e2e-editor`; `e2e-nobody` bound to nothing.
- [ ] **Identity and CSRF:**
  - no identity headers → 401 `application/problem+json`;
  - identity with a wrong proxy token → 401, and the body's `type` names the proxy secret;
  - `X-Forwarded-User: system:masters` → 403 forbidden-principal (the deny-list holds end to end, not just in a unit test).
- [ ] **Reads as the editor:** `/graph` contains the fixture repository as Healthy with its policy edge; `/snapshots?policy=…` returns only that policy's rows; `/repositories/{kindPath}/{name}` resolves for both a `Repository` and a `ClusterRepository`; `/me?namespace=…` reports the capabilities the bindings actually grant.
- [ ] **Browse:** `GET …/tree` before any session → 409 with `urn:kopiur:problem:session-required`; `POST …/session` → 201 and **exactly one** session Job exists; `GET …/tree` lists `a.txt` and `sub`; `GET …/file?path=a.txt` returns the exact bytes with `Content-Length` and an RFC 5987 `Content-Disposition`; `DELETE …/session` → 204 and the Job is gone.
- [ ] **Actions:** snapshot-now as the editor → 201, the created CR carries origin `manual` and reaches Succeeded; the same POST without `X-Kopiur-Request` → 403; as `e2e-nobody` both the action and the list → 403 whose `why` carries the apiserver's own text and whose `fix` names the role to bind.
- [ ] **Doctor:** the report renders and `CredentialsPresent` is a Warn under the e2e fixture.
- [ ] Extend the diagnostics dump with the UI pod's logs so a failure is debuggable from CI output alone.
- [ ] Verify: `KOPIUR_E2E_BINS=ui KOPIUR_E2E_UI=1 mise run //crates/e2e:test`. Record the real output in the report; if the environment has no kind cluster, say so plainly rather than claiming a pass.
- [ ] Commit: `test(e2e): drive the web UI through the apiserver proxy`.

---

### Task 2: Documentation

**Files:** create `docs/ui.md` and `deploy/examples/45-web-ui.yaml`; update `docs/rbac.md`, `docs/configuration.md`, `docs/install.md`, `docs/server.md` (cross-link), `docs/cli/browse.md` (shared sessions), the chart README source, and `CLAUDE.md`.

- [ ] **`docs/ui.md`** covers, in this order: what the UI is and is not; the trust model in plain words (the proxy is the boundary, the UI impersonates, Kubernetes RBAC decides); the four roles and how to bind each, with the browse role's consequence stated outright; the proxy shared secret and why header mode without one is refused; anonymous mode and when it is legitimate; how a browse session works and what it costs; the caps and what a user sees when they hit one; what the UI never does (never reads Secrets, never execs outside a session pod, never creates an Ingress); and troubleshooting organized by problem `type` URN, since that is what a user can actually read off a failed request.
- [ ] **`deploy/examples/45-web-ui.yaml`**: a values excerpt plus a user-owned oauth2-proxy example that injects the identity headers and the shared secret, with both an Ingress and an HTTPRoute variant, and the role bindings. It is an example of wiring the user owns — make that explicit.
- [ ] **`CLAUDE.md`**: add the three new crates to the workspace layout, the new mise tasks, the rule that TypeScript never hand-declares an API shape, and correct the stale Rust version line.
- [ ] Verify: `mise run docs helm-docs-check`; read `docs/ui.md` once as if you were an operator who has never seen Kopiur and fix anything that assumes context.
- [ ] Commit: `docs: the web UI, its trust model, and how to deploy it`.

---

### Task 3: Whole-feature verification

- [ ] Full gate on the branch: `cargo test --workspace --locked`, `mise run clippy fmt-check phase-check wiring-check gen-check complexity-check helm-lint helm-test helm-docs-check`, `mise run ui-check ui-build`, and `KOPIUR_E2E_BINS=cli mise run //crates/e2e:test` to prove the CLI is still behavior-identical after the ops extraction.
- [ ] Confirm `.mise/mise.lock` carries only the deliberate pnpm addition.
- [ ] Write the pull-request description: what the feature is, the trust model in five sentences, the milestone list, what is deliberately out of scope (live updates, per-user session isolation, non-impersonation auth), and the rulings a reviewer should know about.
- [ ] **Do not push and do not open the pull request** until the user is asked. Present the summary and the accumulated rulings first.

## Risks

| Risk | Mitigation |
|---|---|
| The apiserver proxy drops the identity headers | detected by the first test; fall back to an in-cluster curl Job |
| A flaky session Job makes the browse tests noisy | assert on the Job count and wait on readiness with the harness's existing helpers, never a fixed sleep |
| Docs overstate what the UI protects | every claim traces to the security-model section; the browse consequence is stated where the grant is |
| The e2e cannot run in this environment | say so; CI's `ui` shard is the backstop |
