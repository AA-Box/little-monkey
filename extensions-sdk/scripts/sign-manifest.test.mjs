import assert from "node:assert/strict";
import test from "node:test";

import { assertRepresentableNumbers } from "./sign-manifest.mjs";

test("accepts finite fractions and safe integers", () => {
  assert.doesNotThrow(() =>
    assertRepresentableNumbers({
      schema: { minimum: -0.25, multipleOf: 0.5 },
      bounds: [Number.MIN_VALUE, Number.MAX_SAFE_INTEGER],
    }),
  );
});

test("rejects unsafe integers and non-finite numbers", () => {
  assert.throws(
    () => assertRepresentableNumbers({ maximum: Number.MAX_SAFE_INTEGER + 1 }),
    /outside JavaScript's safe range/,
  );
  assert.throws(
    () => assertRepresentableNumbers({ value: Number.POSITIVE_INFINITY }),
    /non-finite number/,
  );
  assert.throws(
    () => assertRepresentableNumbers({ value: Number.NaN }),
    /non-finite number/,
  );
});
