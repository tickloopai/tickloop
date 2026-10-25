import assert from "node:assert/strict";
import test from "node:test";
import { checkAgentAccessBoundary } from "./check-agent-access.mjs";

test("a Harness dependency cannot bring in the Code AgentAccess contract", () => {
  const packages = [
    { name: "tickloop-harness-runtime", dependencies: [{ name: "adapter", kind: "dev" }] },
    { name: "adapter", dependencies: [{ name: "tickloop-code-access", kind: null }] },
    { name: "tickloop-code-access", dependencies: [] },
    { name: "tickloop-code-console", dependencies: [{ name: "tickloop-code-access", kind: null }] },
  ].map(pkg => ({ ...pkg, id: pkg.name }));
  const metadata = { packages, workspace_members: packages.map(pkg => pkg.id) };
  assert.deepEqual(checkAgentAccessBoundary(metadata), [
    "tickloop-harness-runtime -> adapter -> tickloop-code-access: Harness must not depend on AgentAccess",
  ]);
});
