import assert from "node:assert/strict";
import test from "node:test";
import { compareApi } from "./check-public-api.mjs";

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
