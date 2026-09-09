import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import type { RepositoryDetail as RepositoryDetailData } from "../api/types";
import { renderWithRouter } from "../test-utils";
import { RepositoryDetail } from "./RepositoryDetail";

/** A repository the operator has barely touched: everything optional absent. */
const bare: RepositoryDetailData = {
  summary: {
    kind: "Repository",
    kindPath: "repository",
    name: "fresh",
    namespace: "media",
    phase: null,
    health: "unknown",
    backend: "Filesystem",
    mode: "ReadWrite",
    serverBacked: false,
    suspended: false,
    snapshotCount: null,
    totalSizeBytes: null,
    indexBlobCount: null,
    lastObservedAt: null,
    serverEndpoint: null,
    allowedNamespaceCount: null,
  },
  identityCluster: null,
  catalog: null,
  health: null,
  server: null,
  seed: null,
  maintenance: null,
  gates: [],
  conditions: [],
  policies: [],
  replicationsOut: [],
  replicationsIn: [],
  sessions: [],
};

describe("RepositoryDetail with nothing reported yet", () => {
  it("never lights the healthy lamp over a repository with no phase", async () => {
    renderWithRouter(<RepositoryDetail detail={bare} />);
    const verdict = await screen.findByRole("status", { name: "Repository verdict" });
    expect(verdict.querySelector(".verdict__lamp")).toHaveAttribute("data-health", "unknown");
    expect(verdict).toHaveTextContent("has written no phase");
  });

  it("says no catalog scan has run rather than showing zeroes", async () => {
    renderWithRouter(<RepositoryDetail detail={bare} />);
    const catalog = await screen.findByRole("region", { name: "Catalog" });
    expect(catalog).toHaveTextContent("No catalog scan has been recorded");
    expect(catalog).not.toHaveTextContent("0");
  });

  it("says no probe has run, which is not the same as a passing probe", async () => {
    renderWithRouter(<RepositoryDetail detail={bare} />);
    expect(await screen.findByRole("region", { name: "Health probe" })).toHaveTextContent(
      "No probe result has been recorded",
    );
  });

  it("explains what an ungoverned repository loses by having no maintenance", async () => {
    renderWithRouter(<RepositoryDetail detail={bare} />);
    const maintenance = await screen.findByRole("region", { name: "Maintenance" });
    expect(maintenance).toHaveTextContent("No Maintenance resource governs this repository");
    expect(maintenance).toHaveTextContent("grows without bound");
  });

  it("says every mover talks to the backend itself when no server fronts it", async () => {
    renderWithRouter(<RepositoryDetail detail={bare} />);
    expect(await screen.findByRole("region", { name: "Access" })).toHaveTextContent(
      "No repository server is running",
    );
  });

  it("says an unreconciled repository is unreconciled, not healthy", async () => {
    renderWithRouter(<RepositoryDetail detail={bare} />);
    expect(await screen.findByRole("region", { name: "Conditions" })).toHaveTextContent(
      "has not been reconciled — not that it is healthy",
    );
  });

  it("draws no gates section when nothing is holding the repository back", async () => {
    renderWithRouter(<RepositoryDetail detail={bare} />);
    await screen.findByRole("region", { name: "Catalog" });
    expect(screen.queryByRole("region", { name: "Gates holding this repository" })).toBeNull();
  });

  it("renders no actions region when the caller passed none", async () => {
    renderWithRouter(<RepositoryDetail detail={bare} />);
    await screen.findByRole("region", { name: "Catalog" });
    expect(screen.queryByRole("region", { name: "Actions" })).toBeNull();
  });

  it("renders the actions and the per-session control the route hands it", async () => {
    const withSession: RepositoryDetailData = {
      ...bare,
      sessions: [
        {
          namespace: "media",
          job: "kopiur-browse-fresh-1",
          pod: null,
          reused: true,
          expiresAt: "2099-01-01T00:00:00Z",
          downloadMaxBytes: 10,
          manifestMaxBytes: 10,
        },
      ],
    };
    renderWithRouter(
      <RepositoryDetail
        detail={withSession}
        actions={<button type="button">Suspend</button>}
        renderSessionAction={(session) => <span>stop {session.job}</span>}
      />,
    );
    expect(await screen.findByRole("region", { name: "Actions" })).toHaveTextContent("Suspend");
    const sessions = screen.getByRole("region", { name: "Browse sessions" });
    expect(within(sessions).getByText("stop kopiur-browse-fresh-1")).toBeInTheDocument();
    expect(sessions).toHaveTextContent("reaped in");
  });
});
