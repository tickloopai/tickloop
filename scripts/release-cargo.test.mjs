import assert from "node:assert/strict";
import test from "node:test";
import { auditFiles, packageFiles, publishImmutable, validateMetadata } from "./release-cargo.mjs";

const response = (status, value) => ({ status, ok: status === 200, json: async () => ({ version: value }) });

test("public crate audit rejects private paths, duplicate names and missing launcher files", () => {
  assert.doesNotThrow(() => auditFiles(packageFiles));
  for (const files of [[...packageFiles, "agent/runtime/src/lib.rs"], [...packageFiles, ".env"], [...packageFiles, "README.md"], packageFiles.filter(file => file !== "src/lib.rs")]) assert.throws(() => auditFiles(files));
});

test("metadata must describe one dependency-free launcher at the product version", () => {
  const value = { packages: [{ name: "tickloop", version: "0.2.5", dependencies: [], targets: ["tickloop", "tl", "tickhost", "tick-reader"].map(name => ({ name, kind: ["bin"] })) }], workspace_members: ["tickloop"] };
  assert.equal(validateMetadata(value, "0.2.5"), "0.2.5");
  assert.throws(() => validateMetadata(value, "0.2.6"));
  const modified = structuredClone(value); modified.packages[0].dependencies.push({ name: "tickloop-agent-runtime" });
  assert.throws(() => validateMetadata(modified, "0.2.5"));
});

test("a matching published version is verified without reuploading", async () => {
  let uploads = 0;
  assert.equal(await publishImmutable("0.2.5", "checksum", () => uploads++, async () => response(200, { checksum: "checksum", yanked: false })), "already-published");
  assert.equal(uploads, 0);
});

test("a different or yanked version never triggers a second upload", async () => {
  for (const published of [{ checksum: "other", yanked: false }, { checksum: "checksum", yanked: true }]) {
    let uploads = 0;
    await assert.rejects(publishImmutable("0.2.5", "checksum", () => uploads++, async () => response(200, published)));
    assert.equal(uploads, 0);
  }
});

test("registry errors fail closed, while a new upload is checked afterwards", async () => {
  let uploads = 0;
  await assert.rejects(publishImmutable("0.2.5", "checksum", () => uploads++, async () => response(403)));
  assert.equal(uploads, 0);
  let reads = 0;
  assert.equal(await publishImmutable("0.2.5", "checksum", () => uploads++, async () => ++reads === 1 ? response(404) : response(200, { checksum: "checksum", yanked: false })), "published");
  assert.equal(uploads, 1);
  await assert.rejects(publishImmutable("0.2.5", "checksum", () => {}, async () => response(200, { checksum: "different" })));
});

test("release workflow publishes the public crate only after application archives are public", async () => {
  const { readFileSync } = await import("node:fs");
  const workflow = readFileSync(new URL("../.github/workflows/release.yml", import.meta.url), "utf8");
  const job = workflow.slice(workflow.indexOf("\n  cargo-registry:\n"));
  assert.match(job, /needs: publish/);
  assert.match(job, /CARGO_REGISTRY_TOKEN: \$\{\{ secrets\.CARGO_REGISTRY_TOKEN \}\}/);
  assert.match(job, /node scripts\/release-cargo\.mjs --publish/);
  assert.match(job, /cargo install --locked --registry crates-io --version/);
  assert.ok(job.indexOf("Verify installation against the published application") < job.indexOf("Publish only the installer crate"));
  assert.doesNotMatch(job, /cargo publish --workspace/);
});

test("a post-upload checksum mismatch fails without retrying the upload", async () => {
  let reads = 0;
  let uploads = 0;
  await assert.rejects(publishImmutable("0.2.5", "checksum", () => uploads++, async () => ++reads === 1 ? response(404) : response(200, { checksum: "different", yanked: false })));
  assert.equal(uploads, 1);
  assert.equal(reads, 2);
});
