import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { problemBanner } from "../api/problem";
import { cssRules, readStyles } from "../testing/css";
import type { RepositoryGraph } from "../api/types";
import type { LaidOut } from "../components/topology/elk";
import { NODE_HEIGHT, NODE_WIDTH } from "../components/topology/elk";
import {
  BLOBSYNC_BACKEND,
  FIXTURE_EDGE_IDS,
  FIXTURE_GRAPH,
  FIXTURE_NODE_IDS,
  GENERATED_AT,
  GONE,
  MIRROR,
  NAS,
} from "../components/topology/fixture";
import type { TopologyLayoutState } from "../components/topology/layout";
import type { TopologyModel } from "../components/topology/model";
import {
  calledPaths,
  fetchMock,
  forbiddenProblem,
  jsonResponse,
  mockApi,
  mountApp,
  problemResponse,
} from "../test-utils";

/**
 * The layout is mocked, always.
 *
 * ELK is an asynchronous engine in a worker that jsdom does not have, and
 * where it puts a plate is not what this screen has to get right. What it has
 * to get right is the *graph*: that exactly the server's nodes and edges are
 * drawn, that a reference to something that does not exist is unmistakable,
 * that a value this bundle has never seen is never drawn as healthy, and that
 * the four states each say what happened. So the hook is replaced by a fake
 * that lays every node out in a column, and the assertions are about ids,
 * words and lamps.
 */
const mocks = vi.hoisted(() => ({
  layout: null as unknown as (model: TopologyModel | null) => TopologyLayoutState,
}));

vi.mock("../components/topology/layout", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../components/topology/layout")>();
  return {
    ...actual,
    useTopologyLayout: (model: TopologyModel | null) => mocks.layout(model),
  };
});

/** Every node in one column, every edge a straight line between two of them. */
function column(model: TopologyModel | null): TopologyLayoutState {
  if (model === null) {
    return { layout: null, problem: null, pending: false };
  }
  const nodes = new Map<string, { x: number; y: number; width: number; height: number }>();
  model.nodes.forEach((node, index) => {
    nodes.set(node.id, { x: 0, y: index * 100, width: NODE_WIDTH, height: NODE_HEIGHT });
  });
  const edges = new Map<string, { points: { x: number; y: number }[] }>();
  for (const edge of model.edges) {
    const from = nodes.get(edge.from);
    const to = nodes.get(edge.to);
    if (from !== undefined && to !== undefined) {
      edges.set(edge.id, {
        points: [
          { x: from.x + from.width, y: from.y },
          { x: to.x, y: to.y },
        ],
      });
    }
  }
  const layout: LaidOut = {
    width: NODE_WIDTH + 200,
    height: model.nodes.length * 100,
    nodes,
    edges,
  };
  return { layout, problem: null, pending: false };
}

const board = () => screen.findByRole("region", { name: "Topology" });

/** The board once the plates are on it — the region itself renders first, as a skeleton. */
async function drawnBoard(): Promise<HTMLElement> {
  const region = await board();
  await waitFor(() => {
    expect(region.querySelector("[data-node-id]")).not.toBeNull();
  });
  return region;
}
const plates = (region: HTMLElement) =>
  Array.from(region.querySelectorAll<HTMLElement>("[data-node-id]"));
const plate = (region: HTMLElement, id: string): HTMLElement => {
  const found = region.querySelector<HTMLElement>(`[data-node-id="${id}"]`);
  if (found === null) {
    throw new Error(`no plate for ${id}`);
  }
  return found;
};

beforeEach(() => {
  problemBanner.dismiss();
  mocks.layout = column;
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe("Topology", () => {
  it("draws exactly the node and edge sets the server sent, by id", async () => {
    mockApi({ "/api/v1/graph": jsonResponse(FIXTURE_GRAPH) });
    mountApp("/topology?namespace=media");
    const region = await drawnBoard();

    expect(
      plates(region)
        .map((el) => el.dataset.nodeId)
        .sort(),
    ).toEqual([...FIXTURE_NODE_IDS]);
    const drawn = Array.from(region.querySelectorAll<SVGGElement>("[data-edge-id]")).map(
      (el) => el.dataset.edgeId,
    );
    expect(drawn.sort()).toEqual([...FIXTURE_EDGE_IDS]);
    // The scope is on the wire, not merely in the URL.
    expect(calledPaths()).toContain("/api/v1/graph?namespace=media");
  });

  it("makes the dangling reference unmistakable, and marks nothing else that way", async () => {
    mockApi({ "/api/v1/graph": jsonResponse(FIXTURE_GRAPH) });
    mountApp("/topology");
    const region = await drawnBoard();

    const ghost = plate(region, GONE.id);
    expect(ghost.dataset.missing).toBe("true");
    expect(ghost).toHaveTextContent("referenced but not found");
    expect(ghost.dataset.health).toBe("failed");
    // The word is on the plate, not only in the drawer: the whole point is
    // that it is visible without clicking.
    expect(plates(region).filter((el) => el.dataset.missing === "true")).toHaveLength(1);
  });

  it("draws a health this bundle has never read as unknown, never as healthy", async () => {
    mockApi({ "/api/v1/graph": jsonResponse(FIXTURE_GRAPH) });
    mountApp("/topology");
    const region = await drawnBoard();

    const mirror = plate(region, MIRROR.id);
    expect(mirror.dataset.health).toBe("unknown");
    expect(mirror).toHaveTextContent("Unknown");
    expect(mirror).not.toHaveTextContent("Healthy");
  });

  it("renders an enum variant a newer server invented as itself, on both a node and an edge", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const future: RepositoryGraph = {
      generatedAt: GENERATED_AT,
      nodes: [
        {
          id: "SidecarRepository/media/edge",
          // A kind this bundle has never compiled against.
          kind: "sidecarRepository" as RepositoryGraph["nodes"][number]["kind"],
          name: "edge",
          label: "media/edge",
          health: "quiescent" as RepositoryGraph["nodes"][number]["health"],
          missing: false,
          allowsAllNamespaces: false,
          gates: [],
        },
        { ...NAS },
      ],
      edges: [
        {
          id: "MirrorOf/media/edge/media/nas",
          from: "SidecarRepository/media/edge",
          to: NAS.id,
          kind: "mirrorOf" as RepositoryGraph["edges"][number]["kind"],
          health: "healthy",
        },
      ],
    };
    mockApi({ "/api/v1/graph": jsonResponse(future) });
    mountApp("/topology");
    const region = await drawnBoard();

    const unknownPlate = plate(region, "SidecarRepository/media/edge");
    // The raw words, rendered — not dropped, and not dressed up as healthy.
    expect(unknownPlate).toHaveTextContent("sidecarRepository");
    expect(unknownPlate).toHaveTextContent("quiescent");
    expect(unknownPlate.dataset.health).toBe("unknown");
    // The relationship is still drawn, under the "kind we do not know" stroke.
    const edge = region.querySelector<SVGGElement>(
      '[data-edge-id="MirrorOf/media/edge/media/nas"]',
    );
    expect(edge?.dataset.edge).toBe("unknown");
    expect(warn).toHaveBeenCalled();
  });

  it("states the worst thing first over the board, and never goes green with a ghost on it", async () => {
    mockApi({ "/api/v1/graph": jsonResponse(FIXTURE_GRAPH) });
    mountApp("/topology");
    await board();

    const verdict = await screen.findByText(/does not exist/);
    const line = verdict.closest(".verdict");
    expect(line?.querySelector("[data-health]")?.getAttribute("data-health")).toBe("failed");
    expect(line).toHaveTextContent("3 repositories");
    expect(line).toHaveTextContent("2 replications");
  });

  it("shows the whole edge legend without any interaction", async () => {
    mockApi({ "/api/v1/graph": jsonResponse(FIXTURE_GRAPH) });
    mountApp("/topology");
    const legend = await screen.findByRole("region", { name: "What the lines mean" });

    for (const word of [
      "Snapshot replication",
      "Repository replication",
      "Seed",
      "Policy membership",
      "Allowed namespace",
    ]) {
      expect(within(legend).getByText(word)).toBeInTheDocument();
    }
    expect(within(legend).getByText(/copies every blob to a bare backend/)).toBeInTheDocument();
    // Not behind a disclosure: nothing in the legend is collapsed.
    expect(legend.querySelector("details")).toBeNull();
  });

  it("opens a plate's resource in the resource drawer, in the URL, and closes back to the board", async () => {
    mockApi({ "/api/v1/graph": jsonResponse(FIXTURE_GRAPH) });
    const { router } = mountApp("/topology?namespace=media");
    const region = await drawnBoard();
    const nas = plate(region, NAS.id);
    // A plate is a link to the resource, not a button that opens a board-only drawer.
    expect(nas.tagName).toBe("A");
    await userEvent.click(nas);
    const drawer = await screen.findByRole("dialog", { name: /media\/nas/ });
    expect(drawer).toHaveAttribute("data-kind", "repository");
    expect(router.state.location.search).toMatchObject({
      namespace: "media",
      inspect: "repository/media/nas",
    });
    await userEvent.keyboard("{Escape}");
    await waitFor(() => {
      expect(screen.queryByRole("dialog")).toBeNull();
    });
    expect(router.state.location.search).not.toHaveProperty("inspect");
    expect(document.activeElement).toBe(nas);
  });

  it("explains the ghost as what, why and fix, with what refers to it", async () => {
    mockApi({ "/api/v1/graph": jsonResponse(FIXTURE_GRAPH) });
    mountApp("/topology");
    const region = await drawnBoard();

    await userEvent.click(plate(region, GONE.id));
    const drawer = await screen.findByRole("dialog", { name: /gone/ });
    expect(
      await within(drawer).findByText("gone is referenced here but does not exist."),
    ).toBeInTheDocument();
    expect(drawer).toHaveTextContent("nowhere to land");
    expect(within(drawer).getByText("Fix")).toBeInTheDocument();
    const into = within(drawer).getByRole("region", { name: "Pointed at by" });
    expect(within(into).getByRole("link", { name: /orphaned/ })).toBeInTheDocument();
  });

  it("draws a resource plate as a plate, not as underlined link text", () => {
    const rule = cssRules(readStyles()).find((r) => r.selector === "a.topo-node");
    expect(rule?.body).toMatch(/text-decoration:\s*none/);
    expect(rule?.body).toMatch(/color:\s*inherit/);
  });

  it("leaves a backend, a namespace and a selector as plates, not controls", async () => {
    mockApi({ "/api/v1/graph": jsonResponse(FIXTURE_GRAPH) });
    mountApp("/topology");
    const region = await drawnBoard();
    for (const id of [BLOBSYNC_BACKEND.id, "Namespace/prod"]) {
      const inert = plate(region, id);
      expect(inert.tagName).toBe("DIV");
      expect(inert).toHaveAttribute("role", "group");
      expect(inert.querySelector("a, button")).toBeNull();
    }
  });

  it("sends an old ?node= link to the resource drawer, and drops one that is not a resource", async () => {
    mockApi({ "/api/v1/graph": jsonResponse(FIXTURE_GRAPH) });
    const one = mountApp("/topology?namespace=media&node=Repository%2Fmedia%2Fnas");
    await waitFor(() => {
      expect(one.router.state.location.search).toEqual({
        namespace: "media",
        inspect: "repository/media/nas",
      });
    });
  });

  it("drops an old ?node= link to something that is not a resource", async () => {
    mockApi({ "/api/v1/graph": jsonResponse(FIXTURE_GRAPH) });
    const two = mountApp("/topology?node=Backend%2Fmedia%2Fblobsync");
    await waitFor(() => {
      expect(two.router.state.location.search).toEqual({});
    });
  });

  it("shows a skeleton while the graph is loading", async () => {
    fetchMock.resetMocks();
    fetchMock.mockResponse(() => new Promise<Response>(() => undefined));
    mountApp("/topology");
    const region = await board();
    expect(within(region).getByRole("status", { busy: true })).toBeInTheDocument();
  });

  it("shows a skeleton while the layout is still running", async () => {
    mocks.layout = () => ({ layout: null, problem: null, pending: true });
    mockApi({ "/api/v1/graph": jsonResponse(FIXTURE_GRAPH) });
    mountApp("/topology");
    const region = await board();
    expect(await within(region).findByRole("status", { busy: true })).toBeInTheDocument();
    expect(region.querySelector("[data-node-id]")).toBeNull();
  });

  it("teaches the empty state rather than drawing an empty canvas", async () => {
    mockApi({
      "/api/v1/graph": jsonResponse({ nodes: [], edges: [], generatedAt: GENERATED_AT }),
    });
    mountApp("/topology?namespace=media");
    const region = await board();

    expect(await within(region).findByText("Nothing to draw in media")).toBeInTheDocument();
    expect(
      within(region).getByText(/Create a Repository or ClusterRepository/),
    ).toBeInTheDocument();
    // An empty scope is unknown, not green.
    expect(screen.getByText("Nothing to draw").closest(".verdict")).not.toBeNull();
    // No board, so no legend to explain lines that are not there.
    expect(screen.queryByRole("region", { name: "What the lines mean" })).toBeNull();
  });

  it("renders the not-permitted state for a 403, with nothing to retry", async () => {
    mockApi({
      "/api/v1/graph": problemResponse(
        forbiddenProblem("The topology was refused.", "/api/v1/graph"),
      ),
    });
    mountApp("/topology");
    const region = await board();

    expect(await within(region).findByRole("alert")).toHaveTextContent("kopiur-ui-viewer");
    expect(region.querySelector('[data-state="not-permitted"]')).not.toBeNull();
    expect(within(region).queryByRole("button", { name: "Retry" })).toBeNull();
  });

  it("renders the error state with a retry for any other failure", async () => {
    mockApi({
      "/api/v1/graph": new Response("<html>gateway</html>", {
        status: 502,
        headers: { "content-type": "text/html" },
      }),
    });
    mountApp("/topology");
    const region = await board();

    expect(await within(region).findByRole("alert")).toHaveTextContent("answered 502");
    expect(within(region).getByRole("button", { name: "Retry" })).toBeInTheDocument();
  });

  it("says why the board is not drawn when the layout itself fails", async () => {
    const { layoutProblem } = await import("../components/topology/layout");
    mocks.layout = () => ({
      layout: null,
      problem: layoutProblem("no layout algorithm 'layered'"),
      pending: false,
    });
    mockApi({ "/api/v1/graph": jsonResponse(FIXTURE_GRAPH) });
    mountApp("/topology");
    const region = await board();

    const alert = await within(region).findByRole("alert");
    expect(alert).toHaveTextContent("could not be laid out");
    expect(alert).toHaveTextContent("no layout algorithm 'layered'");
    // Deterministic: the same graph would fail the same way, so no retry.
    expect(within(region).queryByRole("button", { name: "Retry" })).toBeNull();
    expect(region.querySelector("[data-node-id]")).toBeNull();
  });
});

describe("Topology — kind identity", () => {
  it("stripes each plate in its object's kind, and leaves non-kopiur nodes unmarked", async () => {
    mockApi({ "/api/v1/graph": jsonResponse(FIXTURE_GRAPH) });
    mountApp("/topology?namespace=media");
    const region = await drawnBoard();
    expect(plate(region, "Repository/media/nas")).toHaveAttribute("data-kind", "repository");
    expect(plate(region, "ClusterRepository/shared")).toHaveAttribute(
      "data-kind",
      "cluster-repository",
    );
    expect(plate(region, "Policy/media/nightly")).toHaveAttribute("data-kind", "snapshot-policy");
    expect(plate(region, "Policy/media/nightly").querySelector(".kind-chip svg")).not.toBeNull();
    expect(plate(region, "Backend/media/blobsync")).not.toHaveAttribute("data-kind");
    expect(plate(region, "Namespace/prod")).not.toHaveAttribute("data-kind");
  });
});
