import assert from "node:assert/strict";
import test from "node:test";
import { checkBoundaries } from "./check-boundaries.mjs";

function metadata(edges) {
  const names = [...new Set(edges.flatMap(([from, to]) => [from, to]))];
  return {
    workspace_members: names,
    packages: names.map(name => ({
      id: name, name,
      dependencies: edges.filter(([from]) => from === name).map(([, to, kind = null, target = null]) => ({ name: to, kind, target })),
    })),
  };
}

const policy = {
  layers: { engine: "harness", cli: "code", facade: "facade", sdk: "tick", host: "tickhost", ui: "ui" },
  allowed: { harness: ["harness"], code: ["code", "facade", "tick", "tickhost", "ui"], facade: ["harness", "tick"], tick: ["tick"], tickhost: ["tick", "tickhost", "ui"], ui: ["tick", "ui"] },
  exceptions: [],
};

test("a Harness dependency on Code fails with the offending edge", () => {
  assert.deepEqual(checkBoundaries(metadata([["engine", "cli"]]), policy), [
    "engine -> cli (normal): harness may not depend on code",
  ]);
});

test("a retired migration exception must be removed", () => {
  const transitional = { ...policy, exceptions: [{ from: "engine", to: "cli", kind: "normal", target: null, reason: "ticket 04" }] };
  assert.deepEqual(checkBoundaries(metadata([["engine", "engine"], ["cli", "facade"]]), transitional), [
    "Stale exception: engine -> cli (normal)",
  ]);
});

for (const [from, to] of [["engine", "host"], ["engine", "ui"], ["sdk", "engine"], ["ui", "cli"], ["cli", "engine"]]) {
  test(`${from} cannot bypass its layer through ${to}, including build and target-specific dev dependencies`, () => {
    for (const kind of [null, "build", "dev"]) {
      const errors = checkBoundaries(metadata([[from, to, kind, 'cfg(windows)']]), policy);
      assert.equal(errors.length, 1);
      assert.match(errors[0], new RegExp(`${from} -> ${to}`));
      assert.match(errors[0], /cfg\(windows\)/);
    }
  });
}

test("the facade and public Tick dependencies remain usable", () => {
  assert.deepEqual(checkBoundaries(metadata([["cli", "facade"], ["facade", "engine"], ["sdk", "sdk"]]), policy), []);
});

test("a migration exception permits only the recorded dependency kind and target", () => {
  const transitional = { ...policy, exceptions: [{ from: "engine", to: "cli", kind: "dev", target: null, reason: "Local adapter tests" }] };
  assert.deepEqual(checkBoundaries(metadata([["engine", "cli", "dev"]]), transitional), []);
  assert.deepEqual(checkBoundaries(metadata([["engine", "cli", "dev"], ["engine", "cli"]]), transitional), [
    "engine -> cli (normal): harness may not depend on code",
  ]);
});

test("new workspace packages must be classified", () => {
  assert.ok(checkBoundaries(metadata([["unclassified", "engine"]]), policy).includes("Unclassified workspace package: unclassified"));
});
