import assert from "node:assert/strict";
import test from "node:test";
import { compareApi, compareApiGroup } from "./check-public-api.mjs";

test("a relocated type keeps both exports and every signature across its package group", () => {
  const group = {
    aliases: { "packages::Token": "oauth::Token", "oauth::data::Token": "oauth::Token" },
    exports: { packages: ["packages::Token"], oauth: ["oauth::Token"] },
  };
  const before = {
    packages: "pub struct packages::Token\npub packages::Token::expires_at: u64\n",
    oauth: "pub use oauth::Token\npub fn oauth::refresh(&packages::Token)\n",
  };
  const after = {
    packages: "pub use packages::Token\n",
    oauth: "pub struct oauth::Token\npub oauth::Token::expires_at: u64\npub fn oauth::refresh(&oauth::data::Token)\n",
  };
  assert.deepEqual(compareApiGroup(before, after, group), { removed: [], added: [] });
  assert.ok(compareApiGroup(before, { ...after, packages: "" }, group).removed.includes("export packages::Token from packages"));
  assert.deepEqual(compareApiGroup(before, { ...after, oauth: after.oauth.replace("expires_at: u64", "expires_at: u32") }, group), {
    removed: ["pub oauth::Token::expires_at: u64"], added: ["pub oauth::Token::expires_at: u32"],
  });
});

test("an API removal fails with the removed signature", () => {
  assert.deepEqual(compareApi("pub fn tick_sdk::open()\npub struct tick_sdk::State\n", "pub struct tick_sdk::State\n"), {
    removed: ["pub fn tick_sdk::open()"], added: [],
  });
});

test("additions and changed signatures also fail the frozen API contract", () => {
  assert.deepEqual(compareApi("pub fn tick_sdk::read() -> u32\n", "pub fn tick_sdk::read() -> u64\npub fn tick_sdk::write()\n"), {
    removed: ["pub fn tick_sdk::read() -> u32"],
    added: ["pub fn tick_sdk::read() -> u64", "pub fn tick_sdk::write()"],
  });
});

test("ordering and a final newline do not change the API", () => {
  assert.deepEqual(compareApi("pub struct A\npub struct B\n", "pub struct B\npub struct A"), { removed: [], added: [] });
});
