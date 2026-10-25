import assert from "node:assert/strict";
import test from "node:test";
import { readFileSync } from "node:fs";
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


test("Harness cannot depend on local Tickhost or package storage in any dependency kind", () => {
  const actual = JSON.parse(readFileSync(new URL("./dependency-boundaries.json", import.meta.url), "utf8"));
  const harness = Object.entries(actual.layers).filter(([, layer]) => ["harness", "facade"].includes(layer));
  for (const [from] of harness) {
    for (const to of ["tickloop-tickhost", "tickloop-tickhost-connect", "tickloop-tick-packages"]) {
      for (const kind of ["normal", "build", "dev"]) {
        const edgePolicy = { ...actual, exceptions: actual.exceptions.filter(edge => edge.from === from && edge.to === to && edge.kind === kind) };
        assert.ok(checkBoundaries(metadata([[from, to, kind === "normal" ? null : kind]]), edgePolicy).some(error => error.startsWith(`${from} -> ${to} (`)), `${from} must reject ${to} (${kind})`);
      }
    }
  }
});

test("Code uses the Harness facade for every dependency kind", () => {
  const actual = JSON.parse(readFileSync(new URL("./dependency-boundaries.json", import.meta.url), "utf8"));
  const code = Object.entries(actual.layers).filter(([, layer]) => layer === "code");
  const harness = Object.entries(actual.layers).filter(([, layer]) => layer === "harness");
  for (const [from] of code) {
    for (const [to] of harness) {
      for (const kind of ["normal", "build", "dev"]) {
        const edgePolicy = { ...actual, exceptions: actual.exceptions.filter(edge => edge.from === from && edge.to === to && edge.kind === kind) };
        assert.ok(checkBoundaries(metadata([[from, to, kind === "normal" ? null : kind]]), edgePolicy).some(error => error.startsWith(`${from} -> ${to} (`)), `${from} must use the facade instead of ${to} (${kind})`);
      }
    }
  }
});
