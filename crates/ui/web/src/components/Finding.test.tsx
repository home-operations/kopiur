import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { Finding } from "./Finding";

describe("Finding", () => {
  it("renders what, why and fix with the fix on its own labelled plate", () => {
    render(
      <Finding
        title="no blocked or stuck work"
        what="Snapshot media/nightly-1 is parked on MoverPermitted=False"
        why="namespace media has not opted in to privileged movers"
        fix="annotate namespace media with kopiur.home-operations.com/allow-privileged-mover=true"
      />,
    );
    const finding = screen.getByRole("article", { name: "no blocked or stuck work" });
    expect(finding.querySelector(".finding__what")).toHaveTextContent("is parked on");
    expect(finding.querySelector(".finding__why")).toHaveTextContent("has not opted in");
    const fix = finding.querySelector(".finding__fix");
    expect(fix).toHaveTextContent("Fix");
    expect(fix).toHaveTextContent("annotate namespace media");
  });

  it("omits the lines it does not have rather than rendering empty plates", () => {
    render(<Finding what="cannot list secrets (RBAC)" why={null} fix={undefined} />);
    const finding = screen.getByRole("article");
    expect(finding.querySelector(".finding__what")).toHaveTextContent("cannot list secrets");
    expect(finding.querySelector(".finding__why")).toBeNull();
    expect(finding.querySelector(".finding__fix")).toBeNull();
    expect(finding.querySelector("h3")).toBeNull();
  });
});
