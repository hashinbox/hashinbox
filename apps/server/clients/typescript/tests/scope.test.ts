// Every case of ../_golden/scope.json, written by packages/scope.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  escapeId,
  fill,
  Scope,
  ScopeError,
  Scopes,
  type Packed,
} from "../src/index";

interface Vectors {
  parse: { text: string; display: string }[];
  refuse: string[];
  covers: { ours: string; theirs: string; covers: boolean }[];
  sets: {
    scopes: string;
    display: string;
    compressed: string;
    packed: Packed;
  }[];
  unpack: { packed: Packed; scopes: string | null }[];
  first_uncovered: {
    granted: string;
    asked: string;
    first_uncovered: string | null;
  }[];
  escape: { id: string; text: string }[];
  expand: {
    depends: Record<string, string[]>;
    cases: { scopes: string; expanded: string }[];
  };
}

const vectors = JSON.parse(
  readFileSync(new URL("../../_golden/scope.json", import.meta.url), "utf8"),
) as Vectors;

test("parse", () => {
  for (const { text, display } of vectors.parse)
    assert.equal(Scope.parse(text).toString(), display, text);
});

test("refuse", () => {
  for (const text of vectors.refuse)
    assert.throws(() => Scope.parse(text), ScopeError, text);
});

test("covers", () => {
  for (const { ours, theirs, covers } of vectors.covers)
    assert.equal(
      Scope.parse(ours).covers(Scope.parse(theirs)),
      covers,
      `${ours} ${theirs}`,
    );
});

test("compress, pack, unpack", () => {
  for (const set of vectors.sets) {
    const held = Scopes.parse(set.scopes);
    assert.equal(held.toString(), set.display);
    assert.equal(held.compress().toString(), set.compressed);
    assert.deepEqual(held.pack(), set.packed);
    assert.equal(Scopes.parse(set.packed).toString(), set.compressed);
  }
});

test("unpack", () => {
  for (const { packed, scopes } of vectors.unpack) {
    if (scopes === null) assert.throws(() => Scopes.parse(packed), ScopeError);
    else assert.equal(Scopes.parse(packed).toString(), scopes);
  }
});

test("firstUncovered", () => {
  for (const { granted, asked, first_uncovered } of vectors.first_uncovered)
    assert.equal(
      Scopes.parse(granted).firstUncovered(Scopes.parse(asked))?.toString() ??
        null,
      first_uncovered,
    );
});

test("escape and fill", () => {
  for (const { id, text } of vectors.escape) {
    assert.equal(escapeId(id), text);
    assert.equal(
      fill("a.wallets[{id}]:read", { id }),
      `a.wallets[${text}]:read`,
    );
  }
  assert.throws(() => fill("a[{id}]:read", {}));
});

test("expand", () => {
  for (const { scopes, expanded } of vectors.expand.cases)
    assert.equal(
      Scopes.parse(scopes).expand(vectors.expand.depends).toString(),
      expanded,
      scopes,
    );
});
