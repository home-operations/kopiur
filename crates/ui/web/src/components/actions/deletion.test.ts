import { describe, expect, it } from "vitest";

import { deletionConsequence } from "./deletion";

describe("deletionConsequence", () => {
  it("says the kopia snapshot is destroyed only for Delete", () => {
    const deleted = deletionConsequence("Delete");
    expect(deleted.destructive).toBe(true);
    expect(deleted.text).toContain("kopia snapshot delete");

    for (const safe of ["Retain", "Orphan"]) {
      expect(deletionConsequence(safe).destructive).toBe(false);
    }
    expect(deletionConsequence("Retain").text).toContain("stays in the repository");
    expect(deletionConsequence("Orphan").text).toContain("without contacting the repository");
  });

  it("never renders an absent policy as Delete", () => {
    for (const absent of [null, undefined, ""]) {
      const none = deletionConsequence(absent);
      expect(none.policy).toBe("not set");
      expect(none.destructive).toBe(false);
      expect(none.text).toContain("the operator decides at deletion time");
      // The trap this exists to close: filling the blank in either direction
      // tells somebody the opposite of the truth.
      expect(none.text).not.toMatch(/^The finalizer runs/);
    }
  });

  it("reports a policy this bundle does not know as the operator's own word", () => {
    const strange = deletionConsequence("ArchiveThenDelete");
    expect(strange.policy).toBe("ArchiveThenDelete");
    expect(strange.destructive).toBe(false);
    expect(strange.text).toContain("does not recognise");
    expect(strange.text).toContain("ArchiveThenDelete");
  });
});
