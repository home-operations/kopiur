import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";

import { jsonResponse, mockApi, renderWithClient } from "../test-utils";
import { NamespaceField, PolicyField, RepositoryField } from "./ResourceFields";

const NAMESPACES = jsonResponse([
  { name: "billing", objects: 3 },
  { name: "media", objects: 2 },
]);

function Harness({ onSubmit = () => undefined }: { onSubmit?: () => void }) {
  const [value, setValue] = useState("");
  return (
    <form
      aria-label="Filters"
      onSubmit={(event) => {
        event.preventDefault();
        onSubmit();
      }}
    >
      <NamespaceField
        id="ns"
        label="Namespace"
        value={value}
        onChange={setValue}
        emptyLabel="all namespaces"
        hint="empty means every namespace"
      />
      <output aria-label="value">{value}</output>
    </form>
  );
}

describe("NamespaceField", () => {
  it("is a field-shaped button that says its value, described by its hint", async () => {
    mockApi({ "/api/v1/namespaces": NAMESPACES });
    renderWithClient(<Harness />);
    const button = await screen.findByRole("button", { name: "Namespace: all namespaces" });
    expect(button).toHaveAttribute("type", "button");
    expect(button).toHaveAccessibleDescription("empty means every namespace");
    expect(button).toHaveAttribute("data-empty", "true");
  });

  it("picks from the namespaces the caller may see, then closes and refocuses", async () => {
    mockApi({ "/api/v1/namespaces": NAMESPACES });
    renderWithClient(<Harness />);
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "Namespace: all namespaces" }));
    const panel = screen.getByRole("dialog", { name: "Choose the namespace" });
    expect(within(panel).getByRole("searchbox", { name: "Filter namespaces" })).toHaveFocus();
    await user.click(await within(panel).findByRole("button", { name: /^media/ }));
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.getByLabelText("value")).toHaveTextContent("media");
    const button = screen.getByRole("button", { name: "Namespace: media" });
    expect(button).toHaveFocus();

    // Clear empties it; there is no "all namespaces" row to pick.
    await user.click(button);
    const again = screen.getByRole("dialog");
    expect(within(again).queryByRole("button", { name: /^all namespaces/ })).toBeNull();
    await user.click(within(again).getByRole("button", { name: "Clear" }));
    expect(screen.getByLabelText("value")).toBeEmptyDOMElement();
  });

  it("takes a namespace it cannot list when it is typed, and Enter never submits the form", async () => {
    mockApi({ "/api/v1/namespaces": NAMESPACES });
    const onSubmit = vi.fn();
    renderWithClient(<Harness onSubmit={onSubmit} />);
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "Namespace: all namespaces" }));
    const panel = screen.getByRole("dialog");
    await within(panel).findByRole("button", { name: /^billing/ });
    await user.type(within(panel).getByRole("searchbox"), "prod");
    expect(within(panel).getByRole("button", { name: /^prod.*not listed/ })).toBeInTheDocument();
    await user.keyboard("{Enter}");
    expect(onSubmit).not.toHaveBeenCalled();
    expect(screen.getByLabelText("value")).toHaveTextContent("prod");
  });

  it("takes the one listed match on Enter", async () => {
    mockApi({ "/api/v1/namespaces": NAMESPACES });
    renderWithClient(<Harness />);
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "Namespace: all namespaces" }));
    const panel = screen.getByRole("dialog");
    await within(panel).findByRole("button", { name: /^billing/ });
    await user.type(within(panel).getByRole("searchbox"), "bil{Enter}");
    expect(screen.getByLabelText("value")).toHaveTextContent("billing");
  });
});

describe("PolicyField", () => {
  it("lists each policy name once, with the namespaces that have it, and takes several", async () => {
    mockApi({
      "/api/v1/policies": jsonResponse([
        {
          namespace: "media",
          name: "nightly",
          repositories: [],
          multiRepo: false,
          suspended: false,
        },
        {
          namespace: "infra",
          name: "nightly",
          repositories: [],
          multiRepo: false,
          suspended: false,
        },
        {
          namespace: "media",
          name: "hourly",
          repositories: [],
          multiRepo: false,
          suspended: false,
        },
      ]),
    });
    function Policy() {
      const [value, setValue] = useState<string[]>([]);
      return (
        <PolicyField
          id="p"
          label="Policy"
          value={value}
          onChange={setValue}
          emptyLabel="any policy"
        />
      );
    }
    renderWithClient(<Policy />);
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "Policy: any policy" }));
    const panel = screen.getByRole("dialog", { name: "Choose the policy" });
    const nightly = await within(panel).findByRole("button", { name: /^nightly/ });
    expect(nightly).toHaveTextContent("media, infra");
    expect(within(panel).getAllByRole("button", { name: /^nightly/ })).toHaveLength(1);
    await user.click(nightly);
    await user.click(within(panel).getByRole("button", { name: /^hourly/ }));
    expect(screen.getByRole("button", { name: "Policy: nightly, hourly" })).toBeInTheDocument();
  });
});

describe("RepositoryField", () => {
  const REPOS = jsonResponse([
    {
      kind: "Repository",
      kindPath: "repository",
      name: "nas",
      namespace: "media",
      health: "healthy",
      mode: "direct",
      serverBacked: false,
      suspended: false,
    },
    {
      kind: "ClusterRepository",
      kindPath: "cluster-repository",
      name: "shared",
      health: "healthy",
      mode: "direct",
      serverBacked: false,
      suspended: false,
    },
  ]);

  function Repo({ initial = [] }: { initial?: string[] }) {
    const [value, setValue] = useState(initial);
    return (
      <>
        <RepositoryField
          id="r"
          label="Repository"
          value={value}
          onChange={setValue}
          emptyLabel="any repository"
        />
        <output aria-label="choice">{value.join(" | ")}</output>
      </>
    );
  }

  it("chooses each repository by the key that carries its kind and namespace", async () => {
    mockApi({ "/api/v1/repositories": REPOS });
    renderWithClient(<Repo />);
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "Repository: any repository" }));
    const panel = screen.getByRole("dialog");
    await user.click(await within(panel).findByRole("button", { name: /^shared/ }));
    await user.click(within(panel).getByRole("button", { name: /^nas/ }));
    expect(screen.getByLabelText("choice")).toHaveTextContent(
      "ClusterRepository/shared | Repository/media/nas",
    );
    expect(screen.getByRole("button", { name: "Repository: shared, nas" })).toBeInTheDocument();
  });

  it("reads a chosen key as its name before the list is fetched", async () => {
    mockApi({ "/api/v1/repositories": REPOS });
    renderWithClient(<Repo initial={["Repository/media/nas"]} />);
    expect(await screen.findByRole("button", { name: "Repository: nas" })).toBeInTheDocument();
  });
});
