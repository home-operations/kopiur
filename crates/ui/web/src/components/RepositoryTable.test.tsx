import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import type { RepositorySummary } from "../api/types";
import { bodyRows, nth, renderWithRouter } from "../test-utils";
import { RepositoryTable } from "./RepositoryTable";

const nas: RepositorySummary = {
  kind: "Repository",
  kindPath: "repository",
  name: "nas",
  namespace: "media",
  phase: "ready",
  health: "healthy",
  backend: "S3",
  mode: "ReadWrite",
  serverBacked: false,
  suspended: false,
  snapshotCount: 412,
  totalSizeBytes: 987654321,
  indexBlobCount: 17,
  lastObservedAt: null,
  serverEndpoint: null,
  allowedNamespaceCount: null,
};

const shared: RepositorySummary = {
  kind: "ClusterRepository",
  kindPath: "cluster-repository",
  name: "shared",
  namespace: null,
  phase: "degraded",
  health: "degraded",
  backend: "Filesystem",
  mode: "ReadOnly",
  serverBacked: true,
  suspended: false,
  snapshotCount: null,
  totalSizeBytes: null,
  indexBlobCount: null,
  lastObservedAt: null,
  serverEndpoint: "https://kopia.internal:51515",
  allowedNamespaceCount: 3,
};

function table() {
  return screen.findByRole("table", { name: "Repositories" });
}

describe("RepositoryTable", () => {
  it("shows the kind, name, namespace, health, phase, mode and storage of each row", async () => {
    renderWithRouter(<RepositoryTable repositories={[nas]} />);
    const row = nth(bodyRows(await table()), 0);
    expect(row).toHaveTextContent("Repository");
    expect(row).toHaveTextContent("nas");
    expect(row).toHaveTextContent("media");
    expect(row.querySelector(".health")).toHaveTextContent("Healthy");
    expect(row).toHaveTextContent("Ready");
    expect(row).toHaveTextContent("ReadWrite");
    expect(row).toHaveTextContent("412");
    expect(row).toHaveTextContent("941.9 MiB");
    expect(row).toHaveTextContent("17");
  });

  it("links with the summary's kindPath, never a segment derived from the display kind", async () => {
    renderWithRouter(<RepositoryTable repositories={[nas, shared]} />);
    expect(await screen.findByRole("link", { name: "nas" })).toHaveAttribute(
      "href",
      "/repositories/repository/nas?namespace=media",
    );
    // "ClusterRepository" would give `/repositories/ClusterRepository/shared`;
    // the kebab segment is the server's own `kindPath`.
    expect(await screen.findByRole("link", { name: "shared" })).toHaveAttribute(
      "href",
      "/repositories/cluster-repository/shared",
    );
  });

  it("renders the never-written last-observed stamp as 'not reported', not as blank", async () => {
    renderWithRouter(<RepositoryTable repositories={[nas]} />);
    const row = nth(bodyRows(await table()), 0);
    expect(within(row).getByText("not reported")).toBeInTheDocument();
  });

  it("says how a server-backed repository is reached and how many namespaces it admits", async () => {
    renderWithRouter(<RepositoryTable repositories={[shared]} />);
    const row = nth(bodyRows(await table()), 0);
    expect(row).toHaveTextContent("Repository server");
    expect(row).toHaveTextContent("3 namespaces");
  });

  it("marks a suspended repository as taking no new backups", async () => {
    renderWithRouter(
      <RepositoryTable repositories={[{ ...nas, suspended: true, health: "suspended" }]} />,
    );
    const row = nth(bodyRows(await table()), 0);
    expect(row.querySelector(".health")).toHaveTextContent("Suspended");
  });

  it("renders a phase from a newer operator as the raw word rather than as healthy", async () => {
    renderWithRouter(
      <RepositoryTable
        repositories={[{ ...nas, phase: { unknown: { raw: "Reconciling" } }, health: "unknown" }]}
      />,
    );
    const row = nth(bodyRows(await table()), 0);
    expect(row).toHaveTextContent("Reconciling");
    expect(row.querySelector(".health")).toHaveTextContent("Unknown");
  });

  it("keeps a count of zero as zero — an absent count is the one that is unknown", async () => {
    renderWithRouter(
      <RepositoryTable repositories={[{ ...nas, snapshotCount: 0, totalSizeBytes: 0 }]} />,
    );
    const row = nth(bodyRows(await table()), 0);
    expect(row).toHaveTextContent("0");
    expect(row).toHaveTextContent("0 B");
  });
});
