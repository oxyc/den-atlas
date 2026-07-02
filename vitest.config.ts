import { defineConfig } from "vitest/config";

// Plain node runtime (no workerd): the core is runtime-agnostic Web Fetch, and the dataset is injected
// into `handleAtlas` as bytes, so tests need no filesystem. Coverage is v8, gated ≥90% on the logic
// modules; the runtime/IO entrypoints (server, worker, dataset loader) are smoke-tested by the Docker
// healthcheck + the deploy doc, not vitest.
export default defineConfig({
  test: {
    environment: "node",
    include: ["test/**/*.test.ts"],
    coverage: {
      provider: "v8",
      include: ["src/**/*.ts"],
      exclude: ["src/server.ts", "src/worker.ts", "src/dataset.ts"],
      reporter: ["text", "lcov"],
      thresholds: { lines: 90, functions: 90, statements: 90 },
    },
  },
});
