import assert from "node:assert/strict";
import test from "node:test";
import { checkAgentAccessBoundary } from "./check-agent-access.mjs";

test("a Harness dependency cannot bring in the Code AgentAccess contract", () => {
  const packages = [
    { name: "tickloop-agent-runtime", dependencies: [{ name: "adapter", kind: "dev" }] },
    { name: "adapter", dependencies: [{ name: "tickloop-agent-access", kind: null }] },
    { name: "tickloop-agent-access", dependencies: [] },
    { name: "tickloop-agent-console", dependencies: [{ name: "tickloop-agent-access", kind: null }] },
  ].map(pkg => ({ ...pkg, id: pkg.name }));
  const metadata = { packages, workspace_members: packages.map(pkg => pkg.id) };
  assert.deepEqual(checkAgentAccessBoundary(metadata), [
    "tickloop-agent-runtime -> adapter -> tickloop-agent-access: Harness must not depend on AgentAccess",
  ]);
});
