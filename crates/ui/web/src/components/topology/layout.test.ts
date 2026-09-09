import { describe, expect, it } from "vitest";

import type { ElkNode } from "elkjs/lib/elk-api";

import { CLIENT_PROBLEM_PREFIX } from "../../api/problem";
import { NODE_HEIGHT, NODE_WIDTH } from "./elk";
import { FIXTURE_GRAPH, NAS } from "./fixture";
import { type LayoutRequest, type LayoutResponse, layoutProblem, layoutTopology } from "./layout";
import { topologyModel } from "./model";

/**
 * A worker that never runs ELK: the tests are about the transport — what is
 * posted, what is read back, and that the worker is always stopped — not
 * about the engine, which `elk.test.ts` pins on both sides.
 */
class FakeWorker {
  onmessage: ((event: MessageEvent<LayoutResponse>) => void) | null = null;
  onerror: ((event: ErrorEvent) => void) | null = null;
  readonly posted: LayoutRequest[] = [];
  terminated = 0;

  postMessage(message: LayoutRequest): void {
    this.posted.push(message);
  }

  terminate(): void {
    this.terminated += 1;
  }

  answer(response: LayoutResponse): void {
    this.onmessage?.(new MessageEvent<LayoutResponse>("message", { data: response }));
  }

  fail(message: string): void {
    this.onerror?.(new ErrorEvent("error", { message }));
  }
}

/** A fake worker plus the factory that hands it to `layoutTopology`. */
function fakeWorker(): { worker: FakeWorker; factory: () => Worker } {
  const worker = new FakeWorker();
  return { worker, factory: () => worker as unknown as Worker };
}

const model = topologyModel(FIXTURE_GRAPH);

/** An ELK answer that positions one node, for the read-back assertions. */
function laidOut(): ElkNode {
  return {
    id: "topology",
    width: 800,
    height: 400,
    children: [{ id: NAS.id, x: 24, y: 24, width: NODE_WIDTH, height: NODE_HEIGHT }],
    edges: [],
  };
}

describe("layoutTopology", () => {
  it("posts the ELK graph for the model and resolves the boxes the worker sends back", async () => {
    const { worker, factory } = fakeWorker();
    const running = layoutTopology(model, factory);
    expect(worker.posted).toHaveLength(1);
    // What goes over the wire is exactly `toElkGraph`'s output: every node,
    // by id, at the one plate size.
    const sent = worker.posted[0]?.graph;
    expect(sent?.children?.map((child) => child.id).sort()).toEqual(
      FIXTURE_GRAPH.nodes.map((node) => node.id).sort(),
    );

    worker.answer({ ok: true, graph: laidOut() });
    const layout = await running.result;
    expect(layout.width).toBe(800);
    expect(layout.nodes.get(NAS.id)).toEqual({
      x: 24,
      y: 24,
      width: NODE_WIDTH,
      height: NODE_HEIGHT,
    });
    expect(worker.terminated).toBe(1);
  });

  it("rejects with the worker's own reason when the engine refuses the graph", async () => {
    const { worker, factory } = fakeWorker();
    const running = layoutTopology(model, factory);
    worker.answer({ ok: false, message: "no layout algorithm 'layered'" });
    await expect(running.result).rejects.toThrow("no layout algorithm 'layered'");
    expect(worker.terminated).toBe(1);
  });

  it("rejects when the worker itself errors, and still stops it", async () => {
    const { worker, factory } = fakeWorker();
    const running = layoutTopology(model, factory);
    worker.fail("Script error");
    await expect(running.result).rejects.toThrow("Script error");
    expect(worker.terminated).toBe(1);
  });

  it("rejects a layout that lost a node rather than drawing plates at the origin", async () => {
    const { worker, factory } = fakeWorker();
    const running = layoutTopology(model, factory);
    worker.answer({
      ok: true,
      graph: { id: "topology", width: 800, height: 400, children: [{ id: NAS.id }] },
    });
    await expect(running.result).rejects.toThrow(NAS.id);
  });

  it("rejects with a stated reason when this browser cannot start a worker at all", async () => {
    const running = layoutTopology(model, () => {
      throw new Error("Worker is not defined");
    });
    await expect(running.result).rejects.toThrow(/could not start \(Worker is not defined\)/);
  });

  it("terminates the worker on cancel", () => {
    const { worker, factory } = fakeWorker();
    layoutTopology(model, factory).cancel();
    expect(worker.terminated).toBe(1);
  });
});

describe("layoutProblem", () => {
  it("is a client problem with what, why and a next step — never a bare status", () => {
    const problem = layoutProblem("out of memory");
    expect(problem.type).toBe(`${CLIENT_PROBLEM_PREFIX}layout`);
    expect(problem.what).toContain("could not be laid out");
    expect(problem.why).toContain("out of memory");
    expect(problem.fix).toContain("namespace");
    // Not a 403: a layout failure must never render as the not-permitted state.
    expect(problem.status).toBe(0);
  });
});
