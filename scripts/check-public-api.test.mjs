import assert from "node:assert/strict";
import test from "node:test";
import { compareApi, compareApiGroup } from "./check-public-api.mjs";

function tokenRustdoc(crate, definitionPath) {
  return {
    root: 0,
    index: {
      0: { name: crate, inner: { module: { items: [1] } } },
      1: { visibility: "public", inner: { use: { name: "Token", id: 2, is_glob: false } } },
    },
    paths: {
      2: { crate_id: definitionPath.startsWith(`${crate}::`) ? 0 : 1, path: definitionPath.split("::"), kind: "struct" },
    },
  };
}

const tokenGroup = {
  aliases: { "packages::Token": "oauth::Token", "oauth::data::Token": "oauth::Token" },
  exports: { packages: ["packages::Token"], oauth: ["oauth::Token"] },
};
const tokenBefore = {
  packages: "pub struct packages::Token\npub packages::Token::expires_at: u64\n",
  oauth: "pub use oauth::Token\npub fn oauth::refresh(&packages::Token)\n",
};
const tokenAfter = {
  packages: "pub use packages::Token\n",
  oauth: "pub struct oauth::Token\npub oauth::Token::expires_at: u64\npub fn oauth::refresh(&oauth::data::Token)\n",
};
const tokenDocuments = {
  baseline: { packages: tokenRustdoc("packages", "packages::Token"), oauth: tokenRustdoc("oauth", "packages::Token") },
  current: { packages: tokenRustdoc("packages", "oauth::data::Token"), oauth: tokenRustdoc("oauth", "oauth::data::Token") },
};

test("an independent same-name struct cannot replace the packages token re-export", () => {
  const group = {
    aliases: { "tick_packages::OAuthToken": "tick_oauth::OAuthToken" },
    exports: { packages: ["tick_packages::OAuthToken"], oauth: ["tick_oauth::OAuthToken"] },
  };
  const before = {
    packages: "pub struct tick_packages::OAuthToken\npub tick_packages::OAuthToken::expires_at: u64\n",
    oauth: "pub use tick_oauth::OAuthToken\n",
  };
  const after = {
    packages: "pub struct tick_packages::OAuthToken\n",
    oauth: "pub struct tick_oauth::OAuthToken\npub tick_oauth::OAuthToken::expires_at: u64\n",
  };
  assert.throws(() => compareApiGroup(before, after, group), /independent definitions.*OAuthToken/);
});

test("a relocated type keeps both exports and every signature across its package group", () => {
  assert.deepEqual(compareApiGroup(tokenBefore, tokenAfter, tokenGroup, tokenDocuments), { removed: [], added: [] });
  assert.ok(compareApiGroup(tokenBefore, { ...tokenAfter, packages: "" }, tokenGroup, tokenDocuments).removed.includes("export packages::Token from packages"));
  assert.deepEqual(compareApiGroup(tokenBefore, { ...tokenAfter, oauth: tokenAfter.oauth.replace("expires_at: u64", "expires_at: u32") }, tokenGroup, tokenDocuments), {
    removed: ["pub oauth::Token::expires_at: u64"], added: ["pub oauth::Token::expires_at: u32"],
  });
});

test("a same-name re-export of an unrelated type cannot borrow the oauth signatures", () => {
  const documents = {
    ...tokenDocuments,
    current: { ...tokenDocuments.current, packages: tokenRustdoc("packages", "unrelated::Token") },
  };
  assert.throws(() => compareApiGroup(tokenBefore, tokenAfter, tokenGroup, documents), /unverified equivalent paths.*packages::Token.*oauth::Token/);
});

test("equivalent paths require rustdoc identity evidence on both sides", () => {
  for (const label of ["baseline", "current"]) {
    assert.throws(() => compareApiGroup(tokenBefore, tokenAfter, tokenGroup, { ...tokenDocuments, [label]: {} }),
      new RegExp(`${label}: unverified equivalent paths`));
  }
});

test("a private path naming a separate type cannot be normalized to the public token", () => {
  const oauth = tokenRustdoc("oauth", "oauth::actual::Token");
  oauth.paths[3] = { crate_id: 0, path: ["oauth", "data", "Token"], kind: "struct" };
  const documents = {
    ...tokenDocuments,
    current: { packages: tokenRustdoc("packages", "oauth::actual::Token"), oauth },
  };
  assert.throws(() => compareApiGroup(tokenBefore, tokenAfter, tokenGroup, documents),
    /unverified equivalent paths oauth::data::Token and oauth::Token/);
});

test("neither public token export may disappear after a relocation", () => {
  for (const pkg of ["packages", "oauth"]) {
    const after = { ...tokenAfter, [pkg]: tokenAfter[pkg].replace(/^pub (?:use|struct) .*\n/, "") };
    assert.ok(compareApiGroup(tokenBefore, after, tokenGroup, tokenDocuments).removed.includes(`export ${pkg}::Token from ${pkg}`));
  }
});

test("relocation does not hide removed methods or trait implementations", () => {
  for (const [before, removed] of [
    ["pub fn packages::Token::refresh_token(&self) -> &str", "pub fn oauth::Token::refresh_token(&self) -> &str"],
    ["impl core::fmt::Debug for packages::Token", "impl core::fmt::Debug for oauth::Token"],
  ]) {
    assert.deepEqual(compareApiGroup({ ...tokenBefore, packages: `${tokenBefore.packages}${before}\n` }, tokenAfter, tokenGroup, tokenDocuments), {
      removed: [removed], added: [],
    });
  }
});

test("a configured alias does not normalize a suffix of an unrelated qualified path", () => {
  const before = { ...tokenBefore, oauth: `${tokenBefore.oauth}pub fn oauth::other(&unrelated::packages::Token)\n` };
  const after = { ...tokenAfter, oauth: `${tokenAfter.oauth}pub fn oauth::other(&unrelated::oauth::Token)\n` };
  assert.deepEqual(compareApiGroup(before, after, tokenGroup, tokenDocuments), {
    removed: ["pub fn oauth::other(&unrelated::packages::Token)"],
    added: ["pub fn oauth::other(&unrelated::oauth::Token)"],
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
