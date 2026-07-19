"use strict";

// Twenty-five compact insertion shapes, each expanded over four maintained
// variants. Every fixture represents prefix + completion + suffix exactly as
// the editor applies it; no surrounding code is repaired by the test runner.
const patterns = [
  ["const answer__N__ = ", ";\n", "42"],
  ["function add__N__(a, b) { return ", "; }\n", "a + b"],
  ["const names__N__ = [", "];\n", "\"Ada\", \"Linus\""],
  ["const config__N__ = { enabled: ", " };\n", "true"],
  ["const greet__N__ = (name) => ", ";\n", "`Hello ${name}`"],
  ["class Counter__N__ { value = 0; inc() { ", " } }\n", "this.value += 1;"],
  ["const doubled__N__ = [1, 2, 3].map((value) => ", ");\n", "value * 2"],
  ["const adult__N__ = users.find((user) => ", ");\n", "user.age >= 18"],
  ["function clamp__N__(value, min, max) { return ", "; }\n", "Math.min(max, Math.max(min, value))"],
  ["const message__N__ = condition ? ", " : \"no\";\n", "\"yes\""],
  ["const copy__N__ = { ", " };\n", "...source, active: true"],
  ["const first__N__ = items?.[0] ", ";\n", "?? null"],
  ["try { risky(); } catch (error) { ", " }\n", "console.error(error);"],
  ["for (const item of items) { ", " }\n", "results.push(item.id);"],
  ["if (ready) { ", " } else { wait(); }\n", "start();"],
  ["const parsed__N__ = JSON.parse(", ");\n", "input"],
  ["const total__N__ = values.reduce((sum, value) => ", ", 0);\n", "sum + value"],
  ["const unique__N__ = new Set(", ");\n", "values"],
  ["function isString__N__(value) { return ", "; }\n", "typeof value === \"string\""],
  ["const lower__N__ = text.", "();\n", "toLowerCase"],
  ["const selected__N__ = rows.filter((row) => ", ");\n", "row.selected"],
  ["const pair__N__ = [", "];\n", "key, value"],
  ["function noop__N__() { ", " }\n", "return undefined;"],
  ["const promise__N__ = Promise.", "(value);\n", "resolve"],
  ["const normalized__N__ = String(value).", "();\n", "trim"],
];

function buildCompletionFixtures() {
  return patterns.flatMap(([prefix, suffix, completion], patternIndex) =>
    Array.from({ length: 4 }, (_, variant) => ({
      id: `js-${String(patternIndex + 1).padStart(2, "0")}-${variant + 1}`,
      language: "javascript",
      prefix: prefix.replaceAll("__N__", String(variant + 1)),
      suffix,
      completion,
    }))
  );
}

module.exports = { buildCompletionFixtures };
