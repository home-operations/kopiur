import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import type { DirEntryView, DirListing, SessionInfo } from "../../api/types";
import { bodyRows, nth, renderWithRouter } from "../../test-utils";
import { DirTable } from "./DirTable";

const MIB = 1024 * 1024;

const SESSION: SessionInfo = {
  namespace: "media",
  job: "kopiur-browse-nas",
  pod: "kopiur-browse-nas-abcde",
  reused: true,
  expiresAt: "2026-06-11T12:30:00Z",
  downloadMaxBytes: 100 * MIB,
  manifestMaxBytes: 32 * MIB,
};

const ENTRIES: DirEntryView[] = [
  { name: "log", kind: "dir", size: 40960, mtime: "2026-06-10T09:00:00Z", mode: "drwxr-xr-x" },
  { name: "app.log", kind: "file", size: 2048, mtime: "2026-06-11T08:15:00Z", mode: "-rw-r--r--" },
  { name: "core.img", kind: "file", size: 512 * MIB, mtime: null, mode: "-rw-------" },
  { name: "unsized", kind: "file", size: null, mtime: null, mode: null },
  { name: "latest", kind: "symlink", size: null, mtime: null, mode: "lrwxrwxrwx" },
  { name: "sock", kind: { other: { raw: "S" } }, size: null, mtime: null, mode: null },
];

function listing(over: Partial<DirListing> = {}): DirListing {
  return {
    path: "var",
    entries: ENTRIES,
    total: ENTRIES.length,
    offset: 0,
    limit: 500,
    session: SESSION,
    ...over,
  };
}

function mount(over: Partial<DirListing> = {}, scope?: string) {
  renderWithRouter(
    <DirTable namespace="media" name="nightly-1" listing={listing(over)} scope={scope} />,
  );
  const path = over.path ?? "var";
  return screen.findByRole("table", {
    name: path.length > 0 ? `Entries in ${path}` : "Entries in the snapshot",
  });
}

function rowFor(table: HTMLElement, name: string): HTMLElement {
  const row = bodyRows(table).find((candidate) => within(candidate).queryByText(name) !== null);
  if (row === undefined) {
    throw new Error(`no row for ${name}`);
  }
  return row;
}

describe("DirTable", () => {
  it("lists every entry in kopia's own order, whatever kind it is", async () => {
    const rows = bodyRows(await mount());
    expect(rows).toHaveLength(6);
    // Not sorted: the server paginates over the manifest order, so reordering
    // a window would imply it reordered the directory.
    expect(nth(rows, 0)).toHaveTextContent("log");
    expect(nth(rows, 5)).toHaveTextContent("sock");
  });

  it("names each kind in a word beside its icon", async () => {
    const table = await mount();
    expect(rowFor(table, "log")).toHaveTextContent("Directory");
    expect(rowFor(table, "app.log")).toHaveTextContent("File");
    expect(rowFor(table, "latest")).toHaveTextContent("Symlink");
    // EntryKind's fallback is `Other { raw }` — kopia's own letter survives.
    expect(rowFor(table, "sock")).toHaveTextContent("Other (S)");
  });

  it("links a directory into itself and never offers to download one", async () => {
    const table = await mount();
    expect(screen.getByRole("link", { name: "log" })).toHaveAttribute(
      "href",
      "/snapshots/media/nightly-1/browse?path=var%2Flog",
    );
    expect(within(rowFor(table, "log")).queryByText("Download")).toBeNull();
  });

  it("renders a downloadable file as an ANCHOR to …/file, not a button", async () => {
    // The whole design of the control: a fetch would buffer the file in the
    // tab's heap; an anchor is a navigation the browser streams to disk.
    await mount();
    const link = screen.getByRole("link", { name: "Download app.log" });
    expect(link.tagName).toBe("A");
    expect(link).toHaveAttribute(
      "href",
      "/api/v1/snapshots/media/nightly-1/file?path=var%2Fapp.log",
    );
    // And no `download` attribute: its value would override the server's
    // sanitised Content-Disposition filename.
    expect(link).not.toHaveAttribute("download");
    expect(screen.queryByRole("button", { name: "Download app.log" })).toBeNull();
  });

  it("disables an oversized file with the size, the limit and the way out — before any click", async () => {
    await mount();
    const refused = screen.getByRole("button", { name: /Download core\.img/ });
    expect(refused).toHaveAttribute("aria-disabled", "true");
    expect(refused).toHaveAccessibleDescription(/core\.img is 512\.0 MiB/);
    expect(refused).toHaveAccessibleDescription(/100\.0 MiB/);
    expect(refused).toHaveAccessibleDescription(/kubectl kopiur download/);
    // The sentence is also the native title, since a ledger clips the
    // tooltip a disabled control would otherwise carry.
    expect(refused).toHaveAttribute("title", expect.stringContaining("512.0 MiB"));
    // It is a button, so there is no href to follow even by keyboard.
    expect(screen.queryByRole("link", { name: /Download core\.img/ })).toBeNull();
  });

  it("writes the short reason in the cell, where no hover is needed to read it", async () => {
    // `.ledger-scroll .button[data-reason]::after { content: none }` removes
    // the floating tooltip inside a ledger, so the word has to be on screen.
    const table = await mount();
    expect(rowFor(table, "core.img")).toHaveTextContent("over 100.0 MiB");
    expect(rowFor(table, "unsized")).toHaveTextContent("size unknown");
    expect(rowFor(table, "latest")).toHaveTextContent("a link");
    expect(rowFor(table, "sock")).toHaveTextContent("not a file");
  });

  it("disables a file with no recorded size, a symlink and an Other entry, each with its reason", async () => {
    await mount();
    expect(screen.getByRole("button", { name: /Download unsized/ })).toHaveAccessibleDescription(
      /records no size for unsized/,
    );
    expect(screen.getByRole("button", { name: /Download latest/ })).toHaveAccessibleDescription(
      /symbolic link/,
    );
    expect(screen.getByRole("button", { name: /Download sock/ })).toHaveAccessibleDescription(
      /neither a file nor a directory/,
    );
  });

  it("keeps a refused entry in the listing rather than hiding it", async () => {
    // "This backup does not contain that" is the one answer this screen must
    // never give by accident.
    const table = await mount();
    for (const name of ["core.img", "unsized", "latest", "sock"]) {
      expect(rowFor(table, name)).toBeInTheDocument();
    }
  });

  it("renders an absent size and an absent mode as the empty cell, never as zero", async () => {
    const table = await mount();
    const cells = within(rowFor(table, "unsized")).getAllByRole("cell");
    expect(nth(cells, 2)).toHaveTextContent("-");
    expect(nth(cells, 4)).toHaveTextContent("-");
    // A directory's size is its subtree total, which kopia records in `summ`.
    expect(nth(within(rowFor(table, "log")).getAllByRole("cell"), 2)).toHaveTextContent("40.0 KiB");
  });

  it("builds a root-level file's path without a leading separator", async () => {
    await mount({ path: "", entries: [nth(ENTRIES, 1)], total: 1 }, "media");
    expect(screen.getByRole("link", { name: "Download app.log" })).toHaveAttribute(
      "href",
      "/api/v1/snapshots/media/nightly-1/file?path=app.log",
    );
  });
});
