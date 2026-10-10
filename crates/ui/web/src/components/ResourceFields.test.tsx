import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";

import { jsonResponse, mockApi, renderWithClient } from "../test-utils";
import {
  NamespaceField,
  PolicyField,
  RepositoryField,
  type RepositoryChoice,
} from "./ResourceFields";

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

    // The empty choice clears it.
    await user.click(button);
    await user.click(
      within(screen.getByRole("dialog")).getByRole("button", { name: /^all namespaces/ }),
    );
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
  it("lists each policy name once, with the namespaces that have it", async () => {
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
      const [value, setValue] = useState("");
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
    expect(screen.getByRole("button", { name: "Policy: nightly" })).toBeInTheDocument();
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

  function Repo() {
    const [value, setValue] = useState<RepositoryChoice>({ name: "", kind: "", namespace: "" });
    return (
      <>
        <RepositoryField
          id="r"
          label="Repository"
          value={value}
          onChange={setValue}
          emptyLabel="any repository"
        />
        <output aria-label="choice">{JSON.stringify(value)}</output>
      </>
    );
  }

  it("sets the kind and namespace with the name, since a name alone may be ambiguous", async () => {
    mockApi({ "/api/v1/repositories": REPOS });
    renderWithClient(<Repo />);
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "Repository: any repository" }));
    const panel = screen.getByRole("dialog");
    await user.click(await within(panel).findByRole("button", { name: /^shared/ }));
    expect(JSON.parse(screen.getByLabelText("choice").textContent)).toEqual({
      name: "shared",
      kind: "cluster-repository",
      namespace: "",
    });
    await user.click(screen.getByRole("button", { name: "Repository: shared" }));
    await user.click(
      await within(screen.getByRole("dialog")).findByRole("button", { name: /^nas/ }),
    );
    expect(JSON.parse(screen.getByLabelText("choice").textContent)).toEqual({
      name: "nas",
      kind: "repository",
      namespace: "media",
    });
  });
});
