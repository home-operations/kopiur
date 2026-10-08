import { act, renderHook } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { useExiting } from "./useExiting";

interface Item {
  id: string;
}
interface Props {
  value: Item | null;
}

const same = (a: Item, b: Item) => a.id === b.id;
const start = (): Props => ({ value: { id: "a" } });

describe("useExiting", () => {
  it("keeps showing the last value while it leaves, until told it has gone", () => {
    const { result, rerender } = renderHook(({ value }) => useExiting(value, same), {
      initialProps: start(),
    });
    expect(result.current).toMatchObject({ shown: { id: "a" }, leaving: false });
    rerender({ value: null });
    expect(result.current).toMatchObject({ shown: { id: "a" }, leaving: true });
    act(() => {
      result.current.exited();
    });
    expect(result.current).toMatchObject({ shown: null, leaving: false });
  });

  it("shows the newest copy of a value, and settles on a fresh copy of the same one", () => {
    let renders = 0;
    const { result, rerender } = renderHook(
      ({ value }) => {
        renders += 1;
        return useExiting(value, same);
      },
      { initialProps: start() },
    );
    const copy = { id: "a" };
    const before = renders;
    rerender({ value: copy });
    expect(result.current.shown).toBe(copy);
    expect(renders - before).toBe(1);
    rerender({ value: { id: "b" } });
    expect(result.current).toMatchObject({ shown: { id: "b" }, leaving: false });
  });

  it("stops leaving when a value comes back before the exit finished", () => {
    const { result, rerender } = renderHook(({ value }) => useExiting(value, same), {
      initialProps: start(),
    });
    rerender({ value: null });
    rerender({ value: { id: "b" } });
    expect(result.current).toMatchObject({ shown: { id: "b" }, leaving: false });
  });
});
