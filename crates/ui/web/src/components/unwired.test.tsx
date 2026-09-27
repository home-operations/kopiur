import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { NotReported } from "./NotReported";
import { NOT_REPORTED, UNWIRED_FIELDS, unwiredReason } from "./unwired";

describe("unwired status fields", () => {
  it("names the seven CRD paths the wiring ratchet records as never written", () => {
    // The list is `crates/xtask/wiring-allowlist.yaml`'s section (c) note. If a
    // controller starts writing one, the ratchet fails first and this test is
    // the second place to change.
    expect(Object.values(UNWIRED_FIELDS)).toEqual([
      "Repository.status.storageStats.lastObservedAt",
      "Maintenance.status.quick.nextScheduledAt",
      "Maintenance.status.full.nextScheduledAt",
      "RepositoryReplication.status.nextScheduledAt",
      "RepositoryReplication.status.lastReplicatedBytes",
      "RepositoryReplication.status.lastReplicatedBlobs",
      "Snapshot.status.stats.bytesNew",
    ]);
  });

  it("says no controller writes the field, naming the CRD path", () => {
    const reason = unwiredReason("repositoryLastObserved");
    expect(reason).toContain("Repository.status.storageStats.lastObservedAt");
    expect(reason).toContain("No controller writes");
  });

  it("renders the words rather than a blank or a zero", () => {
    render(<NotReported field="repositoryReplicationBytes" />);
    const cell = screen.getByText(NOT_REPORTED);
    expect(cell).toHaveTextContent("not reported");
    expect(cell).not.toHaveTextContent("0");
  });

  it("carries the reason as the value's accessible description, not only a tooltip", () => {
    const { container } = render(<NotReported field="maintenanceQuickNextRun" />);
    expect(container).toHaveTextContent("Maintenance.status.quick.nextScheduledAt");
    expect(screen.getByText(NOT_REPORTED)).toHaveAttribute(
      "title",
      expect.stringContaining("No controller writes"),
    );
  });

  it("takes a caller's own reason for a field the wire simply does not carry", () => {
    render(<NotReported reason="SnapshotReplication publishes no next run." />);
    expect(screen.getByText(NOT_REPORTED)).toHaveAttribute(
      "title",
      "SnapshotReplication publishes no next run.",
    );
  });
});
