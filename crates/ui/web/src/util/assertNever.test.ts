import { describe, expect, it, vi } from "vitest";

import { assertNever, unknownVariant } from "./assertNever";
import type { EntryKind, Health } from "../api/types";

describe("assertNever", () => {
  it("throws, naming the value it was handed", () => {
    expect(() => assertNever("surprise" as never)).toThrow(/surprise/);
    expect(() => assertNever({ other: { raw: "socket" } } as never, "EntryKind")).toThrow(
      /EntryKind.*socket/,
    );
  });
});

describe("unknownVariant", () => {
  it("renders an unrecognised string variant as itself, without throwing", () => {
    // `Health` has no `{ unknown: { raw } }` object — its fallback is the
    // plain string "unknown", and a newer server may send a string this
    // bundle has never seen. Rendering it beats throwing.
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    expect(unknownVariant("archived" as never, "Health")).toBe("archived");
    expect(warn).toHaveBeenCalledOnce();
    warn.mockRestore();
  });

  it("renders an unrecognised object variant by its raw payload when it has one", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    expect(unknownVariant({ other: { raw: "socket" } } as never, "EntryKind")).toBe("socket");
    expect(unknownVariant({ unknown: { raw: "Archiving" } } as never)).toBe("Archiving");
    expect(unknownVariant({ future: { detail: 1 } } as never)).toBe('{"future":{"detail":1}}');
    warn.mockRestore();
  });

  it("is the exhaustive default arm for a heterogeneous union", () => {
    // The shape crates/ui-model/src/lib.rs prescribes: narrow the string arm
    // first, then the object's single key. This compiles only while every
    // literal is named.
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const label = (kind: EntryKind): string => {
      if (typeof kind === "string") {
        switch (kind) {
          case "file":
            return "File";
          case "dir":
            return "Directory";
          case "symlink":
            return "Symlink";
          default:
            return unknownVariant(kind, "EntryKind");
        }
      }
      return kind.other.raw;
    };
    expect(label("dir")).toBe("Directory");
    expect(label({ other: { raw: "socket" } })).toBe("socket");

    const health = (h: Health): string => {
      switch (h) {
        case "healthy":
        case "degraded":
        case "failed":
        case "suspended":
        case "pending":
        case "unknown":
          return h;
        default:
          return unknownVariant(h, "Health");
      }
    };
    expect(health("unknown")).toBe("unknown");
    warn.mockRestore();
  });
});
