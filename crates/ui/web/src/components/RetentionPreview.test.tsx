import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import type { RetentionCandidate, RetentionPlan } from "../api/types";
import { bodyRows, nth, renderWithRouter } from "../test-utils";
import { RetentionPreview } from "./RetentionPreview";

const NOW = new Date("2026-09-10T09:00:00Z");

function candidate(over: Partial<RetentionCandidate> = {}): RetentionCandidate {
  return {
    namespace: "media",
    name: "nightly-29",
    endTime: "2026-09-09T01:04:00Z",
    kept: true,
    rules: ["keepDaily slot 1"],
    pinned: false,
    subject: false,
    ...over,
  };
}

function plan(over: Partial<RetentionPlan> = {}): RetentionPlan {
  return {
    buckets: [
      {
        key: "",
        candidates: [
          candidate({ name: "nightly-29", subject: true }),
          candidate({ name: "nightly-28", rules: ["keepDaily slot 2"] }),
          candidate({ name: "nightly-01", kept: false, rules: [] }),
        ],
      },
    ],
    policy: { namespace: "media", name: "nightly" },
    computedAt: "2026-09-10T08:59:00Z",
    unbounded: false,
    ...over,
  };
}

describe("RetentionPreview", () => {
  it("renders one group per bucket key, each with its own candidates", async () => {
    renderWithRouter(
      <RetentionPreview
        now={NOW}
        plan={plan({
          buckets: [
            { key: "pvc/media-data", candidates: [candidate({ name: "a", subject: true })] },
            { key: "pvc/media-config", candidates: [candidate({ name: "b" })] },
          ],
        })}
      />,
    );
    expect(
      await screen.findByRole("region", { name: "Bucket pvc/media-data" }),
    ).toBeInTheDocument();
    expect(screen.getByRole("region", { name: "Bucket pvc/media-config" })).toBeInTheDocument();
    // Buckets are independent, so they are never merged into one list.
    expect(screen.getAllByRole("table")).toHaveLength(2);
  });

  it("names the single bucket of an un-fanned policy rather than showing an empty heading", async () => {
    renderWithRouter(<RetentionPreview plan={plan()} now={NOW} />);
    expect(
      await screen.findByRole("heading", { name: /every snapshot of this policy/i }),
    ).toBeInTheDocument();
  });

  it("keeps the wire order, which is the selection kernel's own: newest first", async () => {
    renderWithRouter(<RetentionPreview plan={plan()} now={NOW} />);
    const rows = bodyRows(await screen.findByRole("table", { name: /Candidates in/ }));
    expect(nth(rows, 0)).toHaveTextContent("nightly-29");
    expect(nth(rows, 2)).toHaveTextContent("nightly-01");
  });

  it("puts the holding rule on the row, visible without a hover", async () => {
    renderWithRouter(<RetentionPreview plan={plan()} now={NOW} />);
    const rows = bodyRows(await screen.findByRole("table", { name: /Candidates in/ }));
    // Plain text in a cell — not a title attribute, not a tooltip.
    expect(nth(rows, 1)).toHaveTextContent("keepDaily slot 2");
    expect(within(nth(rows, 1)).getByText("keepDaily slot 2")).toBeVisible();
  });

  it("lists every rule holding a pinned snapshot, not just the pin", async () => {
    renderWithRouter(
      <RetentionPreview
        now={NOW}
        plan={plan({
          buckets: [
            {
              key: "",
              candidates: [
                candidate({ pinned: true, rules: ["pinned", "keepDaily slot 1"], subject: true }),
              ],
            },
          ],
        })}
      />,
    );
    const rows = bodyRows(await screen.findByRole("table", { name: /Candidates in/ }));
    expect(nth(rows, 0)).toHaveTextContent("pinned, keepDaily slot 1");
  });

  it("highlights the subject among its competitors", async () => {
    renderWithRouter(<RetentionPreview plan={plan()} now={NOW} />);
    const rows = bodyRows(await screen.findByRole("table", { name: /Candidates in/ }));
    expect(nth(rows, 0)).toHaveAttribute("data-subject", "true");
    expect(nth(rows, 0)).toHaveAttribute("aria-current", "true");
    expect(nth(rows, 0)).toHaveTextContent("this snapshot");
    expect(nth(rows, 1)).not.toHaveAttribute("data-subject");
  });

  it("marks kept and pruned rows apart without calling a prune a failure", async () => {
    renderWithRouter(<RetentionPreview plan={plan()} now={NOW} />);
    const rows = bodyRows(await screen.findByRole("table", { name: /Candidates in/ }));
    expect(within(nth(rows, 0)).getByText("Kept")).toBeInTheDocument();
    const pruned = nth(rows, 2);
    expect(within(pruned).getByText("Pruned")).toBeInTheDocument();
    expect(pruned.querySelector(".health")).not.toHaveAttribute("data-health", "failed");
    expect(pruned).toHaveTextContent(/No rule keeps it/);
  });

  it("is loud, once, when the subject itself is the row being dropped", async () => {
    renderWithRouter(
      <RetentionPreview
        now={NOW}
        plan={plan({
          buckets: [
            {
              key: "",
              candidates: [
                candidate({ name: "nightly-29" }),
                candidate({ name: "nightly-01", kept: false, rules: [], subject: true }),
              ],
            },
          ],
        })}
      />,
    );
    const verdict = await screen.findByRole("status", { name: "Retention verdict" });
    expect(verdict).toHaveTextContent(/no longer be restorable/i);
    expect(verdict.querySelector(".verdict__lamp")).toHaveAttribute("data-health", "degraded");
  });

  it("renders an unbounded plan as no retention configured, never as a prune", async () => {
    renderWithRouter(
      <RetentionPreview
        now={NOW}
        plan={plan({
          unbounded: true,
          buckets: [
            {
              key: "",
              candidates: [
                candidate({ name: "a", kept: true, rules: [], subject: true }),
                candidate({ name: "b", kept: true, rules: [] }),
              ],
            },
          ],
        })}
      />,
    );
    const note = await screen.findByText(/No GFS retention is configured/);
    expect(note).toBeInTheDocument();
    const rows = bodyRows(screen.getByRole("table", { name: /Candidates in/ }));
    for (const row of rows) {
      expect(row).not.toHaveTextContent(/prun/i);
      expect(within(row).getByText("Kept")).toBeInTheDocument();
    }
  });

  it("says when the plan was computed, because a verdict is about a moment", async () => {
    renderWithRouter(<RetentionPreview plan={plan()} now={NOW} />);
    expect(await screen.findByRole("status", { name: "Retention verdict" })).toHaveTextContent(
      /computed .* ago/,
    );
  });

  it("counts kept against the whole population so the reader can check the arithmetic", async () => {
    renderWithRouter(<RetentionPreview plan={plan()} now={NOW} />);
    expect(await screen.findByText(/2 of 3 snapshots are kept/)).toBeInTheDocument();
  });
});
