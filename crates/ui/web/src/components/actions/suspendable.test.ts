import { describe, expect, it } from "vitest";

import { CAPABILITY_LABELS } from "../capabilities";
import {
  SUSPENDABLE,
  type SuspendableKind,
  suspendReviewNamespace,
  suspendable,
} from "./suspendable";

describe("the suspendable kind table", () => {
  it("carries exactly the six tokens the handler parses", () => {
    // `crates/ui/src/actions/mod.rs::SUSPENDABLE_KINDS`. A seventh kind added
    // there without a row here would ship a control nothing can reach.
    expect(Object.keys(SUSPENDABLE).sort()).toEqual([
      "cluster-repository",
      "policy",
      "replication",
      "repository",
      "schedule",
      "snapshot-replication",
    ]);
  });

  it("names a capability that actually exists on /me", () => {
    for (const kind of Object.keys(SUSPENDABLE) as SuspendableKind[]) {
      expect(CAPABILITY_LABELS[suspendable(kind).capability]).toBeTypeOf("string");
    }
  });

  it("nests a schedule's suspend under spec.schedule and leaves every other kind flat", () => {
    // `kopiur_ops::suspend::patch_for` — a confirmation that named
    // `spec.suspend` for a schedule would point at a field that is not there.
    expect(suspendable("schedule").path).toBe("spec.schedule.suspend");
    for (const kind of Object.keys(SUSPENDABLE) as SuspendableKind[]) {
      if (kind !== "schedule") {
        expect(suspendable(kind).path).toBe("spec.suspend");
      }
    }
  });

  it("reviews only the cluster-scoped kind with no namespace", () => {
    expect(suspendReviewNamespace("policy", "media")).toBe("media");
    expect(suspendReviewNamespace("replication", "prod")).toBe("prod");
    // A namespaced review of patchClusterRepositories would report a
    // RoleBinding grant that cannot authorize a cluster-scoped write.
    expect(suspendReviewNamespace("cluster-repository", "media")).toBeUndefined();
  });
});
