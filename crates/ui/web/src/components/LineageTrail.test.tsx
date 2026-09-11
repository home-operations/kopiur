import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { renderWithRouter } from "../test-utils";
import { LineageTrail } from "./LineageTrail";

describe("LineageTrail", () => {
  it("says a snapshot written in place is neither a copy nor a source", async () => {
    renderWithRouter(
      <LineageTrail
        name="nightly-29"
        namespace="media"
        lineage={{ copiedFromRepository: null, sourceManifestId: null, copies: [] }}
      />,
    );
    expect(await screen.findByText(/written where it stands/)).toBeInTheDocument();
    expect(screen.queryByRole("list", { name: "Replication lineage" })).not.toBeInTheDocument();
  });

  it("names the upstream repository and its manifest id without pretending to link them", async () => {
    renderWithRouter(
      <LineageTrail
        name="copy-1"
        namespace="offsite"
        lineage={{
          copiedFromRepository: "media/nas",
          sourceManifestId: "k9f2",
          copies: [],
        }}
      />,
    );
    const trail = await screen.findByRole("list", { name: "Replication lineage" });
    const source = within(trail).getByText("media/nas").closest("li");
    expect(source).toHaveAttribute("data-step", "source");
    expect(source).toHaveTextContent("k9f2");
    // The source Snapshot resource may live in another cluster, so there is
    // nothing here that could be linked honestly.
    expect(within(source as HTMLElement).queryByRole("link")).not.toBeInTheDocument();
  });

  it("links every copy, because those are Snapshot resources this caller listed", async () => {
    renderWithRouter(
      <LineageTrail
        name="nightly-29"
        namespace="media"
        lineage={{
          copiedFromRepository: null,
          sourceManifestId: null,
          copies: [{ namespace: "offsite", name: "copy-1" }],
        }}
      />,
    );
    expect(await screen.findByRole("link", { name: "copy-1" })).toBeInTheDocument();
    expect(screen.queryByText(/Nothing has been replicated out/)).not.toBeInTheDocument();
  });

  it("marks the subject as the current step of the trail", async () => {
    renderWithRouter(
      <LineageTrail
        name="nightly-29"
        namespace="media"
        lineage={{
          copiedFromRepository: "media/nas",
          sourceManifestId: "k9f2",
          copies: [],
        }}
      />,
    );
    const subject = (await screen.findByText("nightly-29")).closest("li");
    expect(subject).toHaveAttribute("data-step", "subject");
    expect(subject).toHaveAttribute("aria-current", "step");
  });

  it("says nothing has been copied out when the trail has only an upstream hop", async () => {
    renderWithRouter(
      <LineageTrail
        name="copy-1"
        namespace="offsite"
        lineage={{ copiedFromRepository: "media/nas", sourceManifestId: "k9f2", copies: [] }}
      />,
    );
    expect(
      await screen.findByText(/Nothing has been replicated out of this snapshot yet/),
    ).toBeInTheDocument();
  });
});
