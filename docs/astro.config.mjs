import starlight from "@astrojs/starlight";
import { defineConfig } from "astro/config";
import mermaid from "astro-mermaid";
import starlightLinksValidator from "starlight-links-validator";

export default defineConfig({
  site: "https://kopiur.home-operations.com",
  // `cargo doc` has no workspace index page; `mise run rustdoc` copies its output
  // into public/rustdoc.
  redirects: { "/rustdoc": "/rustdoc/kopiur_api/" },
  integrations: [
    // Must precede starlight so its markdown transform sees the mermaid fences first.
    mermaid(),
    starlight({
      title: "Kopiur",
      description: "A Kopia-native Kubernetes backup operator written in Rust on kube-rs.",
      social: [
        {
          icon: "github",
          label: "GitHub",
          href: "https://github.com/home-operations/kopiur",
        },
      ],
      // Shiki has no PromQL grammar.
      expressiveCode: { shiki: { langAlias: { promql: "txt" } } },
      editLink: {
        baseUrl: "https://github.com/home-operations/kopiur/edit/main/docs/",
      },
      sidebar: [
        "index",
        {
          label: "Concepts",
          items: ["concepts/how-kopia-works", "concepts/why-kopiur"],
        },
        {
          label: "User guide",
          items: [
            "getting-started",
            "install",
            "upgrade",
            "gitops",
            {
              label: "kubectl plugin",
              items: [
                "cli",
                "cli/backup-restore",
                "cli/browse",
                "cli/operations",
                "cli/migrate-volsync",
              ],
            },
            "examples",
            "walkthrough",
            "configuration",
            "repositories",
            {
              label: "Backend configuration",
              items: [
                "backends",
                "backends/s3",
                "backends/azure",
                "backends/gcs",
                "backends/b2",
                "backends/filesystem",
                "backends/sftp",
                "backends/webdav",
                "backends/rclone",
                "backends/gdrive",
              ],
            },
            "backups",
            "copy-methods",
            "stream-sources",
            "access-modes",
            "restores",
            "replication",
            "snapshot-replication",
            "movers",
            "feature-permissions",
            "permissions",
            "security-context",
            "maintenance",
            "repository-health",
            "ui",
            "server",
            "troubleshooting",
          ],
        },
        {
          label: "Scenarios",
          items: [
            "scenarios",
            "scenarios/protect-stateful-app",
            "scenarios/recover-lost-data",
            "scenarios/disaster-recovery",
            "scenarios/migrate-across-clusters",
            "scenarios/adopt-existing-repo",
            "scenarios/verification-drills",
            "scenarios/point-in-time-rollback",
            "scenarios/clone-app-to-namespace",
            "scenarios/shared-repository-multi-cluster",
            "scenarios/dr-with-replicated-repository",
          ],
        },
        {
          label: "Reference",
          items: [
            "field-reference",
            {
              label: "CRD reference",
              items: [
                "reference/crds",
                "reference/crds/repository",
                "reference/crds/cluster-repository",
                "reference/crds/snapshot-policy",
                "reference/crds/snapshot",
                "reference/crds/snapshot-schedule",
                "reference/crds/restore",
                "reference/crds/maintenance",
                "reference/crds/repository-replication",
                "reference/crds/snapshot-replication",
                "reference/crds/shared-types",
              ],
            },
            "rbac",
            "api-reference",
          ],
        },
        { label: "Rust API ↗", link: "/rustdoc/" },
        {
          label: "Developer",
          items: [
            "dev/api-conventions",
            "dev/design-rationale",
            "dev/observability",
            "dev/watch-and-reconcile",
          ],
        },
        {
          label: "Architecture Decision Records",
          items: [
            "adr/0003-kopiur-rust-operator",
            "adr/0004-breaking-crd-and-field-renames",
            "adr/0005-crd-feature-improvements",
            "adr/0006-mass-deletion-protection",
            "adr/0007-repository-circuit-breaker",
            "adr/0001-onedr0p-kopia-operator",
            "adr/0002-bo0tzz-kopia-operator",
          ],
        },
      ],
      plugins: [starlightLinksValidator({ exclude: ["/rustdoc/", "/rustdoc/**"] })],
    }),
  ],
});
