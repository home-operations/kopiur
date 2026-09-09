import type { ElkNode } from "elkjs/lib/elk-api";
import { describe, expect, it } from "vitest";

import { CLIENT_PROBLEM_PREFIX } from "../../api/problem";
import { NODE_HEIGHT, NODE_WIDTH, fromElkLayout } from "./elk";
import { FIXTURE_GRAPH, NAS } from "./fixture";
import { type LayoutEngine, layoutProblem, layoutTopology } from "./layout";
import { topologyModel } from "./model";

/**
 * An engine that never runs ELK: the tests are about the transport — what is
 * handed to the engine, what is read back, and that the worker is always
 * stopped — not about the layout itself, which `elk.test.ts` pins on both
 * sides.
 */
class FakeEngine implements LayoutEngine {
  readonly given: ElkNode[] = [];
  terminated = 0;
  private settle: ((graph: ElkNode) => void) | null = null;
  private refuse: ((cause: unknown) => void) | null = null;

  layout(graph: ElkNode): ReturnType<LayoutEngine["layout"]> {
    this.given.push(graph);
    return new Promise<ElkNode>((resolve, reject) => {
      this.settle = resolve;
      this.refuse = reject;
    });
  }

  terminateWorker(): void {
    this.terminated += 1;
  }

  answer(graph: ElkNode): void {
    this.settle?.(graph);
  }

  fail(cause: unknown): void {
    this.refuse?.(cause);
  }
}

function fakeEngine(): { engine: FakeEngine; factory: () => LayoutEngine } {
  const engine = new FakeEngine();
  return { engine, factory: () => engine };
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
  it("hands the engine the ELK graph for the model and resolves the boxes it answers with", async () => {
    const { engine, factory } = fakeEngine();
    const running = layoutTopology(model, factory);
    expect(engine.given).toHaveLength(1);
    // What goes to the engine is exactly `toElkGraph`'s output: every node,
    // by id, at the one plate size.
    expect(engine.given[0]?.children?.map((child) => child.id).sort()).toEqual(
      FIXTURE_GRAPH.nodes.map((node) => node.id).sort(),
    );

    engine.answer(laidOut());
    const layout = await running.result;
    expect(layout).toEqual(fromElkLayout(laidOut()));
    expect(engine.terminated).toBe(1);
  });

  it("rejects with the engine's own reason when it refuses the graph, and still stops it", async () => {
    const { engine, factory } = fakeEngine();
    const running = layoutTopology(model, factory);
    engine.fail(new Error("no layout algorithm 'layered'"));
    await expect(running.result).rejects.toThrow("no layout algorithm 'layered'");
    expect(engine.terminated).toBe(1);
  });

  it("rejects a layout that lost a node rather than drawing plates at the origin", async () => {
    const { engine, factory } = fakeEngine();
    const running = layoutTopology(model, factory);
    engine.answer({ id: "topology", width: 800, height: 400, children: [{ id: NAS.id }] });
    await expect(running.result).rejects.toThrow(NAS.id);
  });

  it("rejects with a stated reason when this browser cannot start a worker at all", async () => {
    const running = layoutTopology(model, () => {
      throw new Error("Worker is not defined");
    });
    await expect(running.result).rejects.toThrow(/could not start \(Worker is not defined\)/);
  });

  it("terminates the worker on cancel", () => {
    const { engine, factory } = fakeEngine();
    layoutTopology(model, factory).cancel();
    expect(engine.terminated).toBe(1);
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
