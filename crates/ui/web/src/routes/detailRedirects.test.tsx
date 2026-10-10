import { screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { fetchMock, mountApp } from "../test-utils";

/**
 * The four kinds that once had a page of their own now open in the resource
 * drawer. Their old addresses — bookmarks, pasted links — still land: on the
 * kind's list, with the drawer open on the same object.
 */
async function landsOn(path: string) {
  fetchMock.mockResponse(() => new Promise<Response>(() => undefined));
  const { router } = mountApp(path);
  await waitFor(() => {
    expect(router.state.status).toBe("idle");
  });
  return router.state.location;
}

describe("old detail addresses", () => {
  it("open a policy in the drawer over the policies list", async () => {
    const at = await landsOn("/policies/media/nightly");
    expect(at.pathname).toBe("/policies");
    expect(at.search).toEqual({ inspect: "snapshot-policy/media/nightly" });
    expect(await screen.findByRole("dialog", { name: /nightly/ })).toBeInTheDocument();
  });

  it("open a snapshot, dropping the retention flag the page once read", async () => {
    const at = await landsOn("/snapshots/media/nightly-29?retention=open&namespace=media");
    expect(at.pathname).toBe("/snapshots");
    expect(at.search).toEqual({ namespace: "media", inspect: "snapshot/media/nightly-29" });
  });

  it("open a restore", async () => {
    const at = await landsOn("/restores/media/recover-db");
    expect(at.pathname).toBe("/restores");
    expect(at.search).toEqual({ inspect: "restore/media/recover-db" });
  });

  it("open a repository in the namespace the address named", async () => {
    const at = await landsOn("/repositories/repository/nas?namespace=media");
    expect(at.pathname).toBe("/repositories");
    expect(at.search).toEqual({ namespace: "media", inspect: "repository/media/nas" });
  });

  it("open a ClusterRepository, which has no namespace", async () => {
    const at = await landsOn("/repositories/cluster-repository/shared");
    expect(at.pathname).toBe("/repositories");
    expect(at.search).toEqual({ inspect: "cluster-repository/shared" });
  });

  it("land on the list with nothing open when the address cannot name one object", async () => {
    // A namespaced repository with no namespace, and a kind segment that is
    // not one: the list, not a guess and not a crash.
    for (const path of ["/repositories/repository/nas", "/repositories/bogus/nas"]) {
      const at = await landsOn(path);
      expect(at.pathname).toBe("/repositories");
      expect(at.search).not.toHaveProperty("inspect");
    }
  });
});
