import { render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import type { GateDescriptor, GateSeverityView } from "../api/types";
import { bodyRows, nth } from "../test-utils";
import { GateList } from "./GateList";

const registry: GateDescriptor[] = [
  {
    scope: "Snapshot",
    condition: "RepositoryWritable",
    blockedStatus: "False",
    reason: "RepositoryReadOnly",
    severity: "warning",
  },
  {
    scope: "Snapshot,Restore",
    condition: "MoverPermitted",
    blockedStatus: "False",
    reason: "PrivilegedMoverNotPermitted",
    severity: "error",
  },
  {
    scope: "Repository,ClusterRepository",
    condition: "MassDeletionHeld",
    blockedStatus: "True",
    reason: "MassDeletionThresholdExceeded",
    severity: "error",
  },
];

describe("GateList", () => {
  it("renders the registry errors first, one row per gate, with severity as a lettered lamp", () => {
    render(<GateList gates={registry} />);
    const table = screen.getByRole("table", { name: "Structural gates" });
    const rows = bodyRows(table);
    expect(rows).toHaveLength(3);
    expect(
      rows.map(
        (row) =>
          within(row).getByText(/^(MassDeletionHeld|MoverPermitted|RepositoryWritable)$/)
            .textContent,
      ),
    ).toEqual(["MassDeletionHeld", "MoverPermitted", "RepositoryWritable"]);

    const mover = nth(rows, 1);
    const lamp = mover.querySelector(".health");
    expect(lamp).toHaveAttribute("data-health", "failed");
    expect(lamp).toHaveTextContent("Error");
    expect(lamp?.querySelector("svg")).not.toBeNull();
    // Polarity travels with the gate: this one blocks on False.
    expect(mover).toHaveTextContent("False");
    expect(mover).toHaveTextContent("PrivilegedMoverNotPermitted");
    // Both kinds it applies to, as separate label strips.
    expect(within(mover).getByText("Snapshot")).toHaveClass("label-strip__kind");
    expect(within(mover).getByText("Restore")).toHaveClass("label-strip__kind");

    const held = nth(rows, 0);
    expect(held).toHaveTextContent("True");
    const writable = nth(rows, 2);
    expect(writable.querySelector(".health")).toHaveAttribute("data-health", "degraded");
    expect(writable.querySelector(".health")).toHaveTextContent("Warning");
  });

  it("keeps a gate whose severity this bundle cannot read, lamped unknown with the raw word", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    render(
      <GateList
        gates={[
          {
            scope: "Snapshot",
            condition: "Future",
            blockedStatus: "True",
            reason: "NewInV2",
            severity: "critical" as GateSeverityView,
          },
        ]}
      />,
    );
    const lamp = screen.getByRole("table").querySelector(".health");
    expect(lamp).toHaveAttribute("data-health", "unknown");
    expect(lamp).toHaveTextContent("critical");
    warn.mockRestore();
  });
});
