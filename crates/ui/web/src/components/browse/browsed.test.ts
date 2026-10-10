import { afterEach, describe, expect, it, vi } from "vitest";

import { browseStatus, browsed } from "./browsed";

const STORED = "kopiur.browse.snapshot";

afterEach(() => {
  browsed.reset();
  window.sessionStorage.clear();
  vi.restoreAllMocks();
});

describe("browsed", () => {
  it("remembers the snapshot browsed for the tab, and tells its listeners", () => {
    const heard = vi.fn();
    const stop = browsed.subscribe(heard);
    browsed.browse("media/nightly-1");
    expect(browsed.get().snapshot).toBe("media/nightly-1");
    expect(window.sessionStorage.getItem(STORED)).toBe("media/nightly-1");
    expect(heard).toHaveBeenCalled();
    stop();
  });

  it("reads back what the tab stored, and only a snapshot address", () => {
    window.sessionStorage.setItem(STORED, "media/nightly-1");
    expect(browsed.stored()).toBe("media/nightly-1");
    window.sessionStorage.setItem(STORED, "not an address");
    expect(browsed.stored()).toBeNull();
  });

  it("works on without storage", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    browsed.browse("media/nightly-1");
    expect(browsed.get().snapshot).toBe("media/nightly-1");
    expect(browsed.stored()).toBeNull();
  });

  it("keeps the outcome of the last start, per snapshot", () => {
    browsed.started("media/nightly-1", false);
    expect(browsed.get().failedStart).toBe("media/nightly-1");
    browsed.started("media/nightly-1", true);
    expect(browsed.get().failedStart).toBeNull();
  });
});

describe("browseStatus", () => {
  it("is idle with nothing browsed, or nothing running", () => {
    expect(browseStatus({ snapshot: null, failedStart: null }, false, false).state).toBe("idle");
    expect(
      browseStatus({ snapshot: "media/nightly-1", failedStart: null }, false, false).state,
    ).toBe("idle");
  });

  it("is active while a session runs, whatever failed before", () => {
    const status = browseStatus(
      { snapshot: "media/nightly-1", failedStart: "media/nightly-1" },
      true,
      false,
    );
    expect(status.state).toBe("active");
    expect(status.words).toBe("A browse session is running on media/nightly-1");
  });

  it("is failed when the last start failed, or the session could not be read", () => {
    expect(
      browseStatus({ snapshot: "media/nightly-1", failedStart: "media/nightly-1" }, false, false)
        .words,
    ).toBe("The last browse session on media/nightly-1 failed");
    expect(
      browseStatus({ snapshot: "media/nightly-1", failedStart: null }, false, true).state,
    ).toBe("failed");
    // A failure on a snapshot no longer browsed is not this one's.
    expect(
      browseStatus({ snapshot: "media/nightly-2", failedStart: "media/nightly-1" }, false, false)
        .state,
    ).toBe("idle");
  });
});
