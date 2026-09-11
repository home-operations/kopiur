// vitest setup, run once per test file (see vite.config.ts `test.setupFiles`).
import "@testing-library/jest-dom/vitest";
import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";

afterEach(() => {
  cleanup();
});
