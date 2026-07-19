"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const { EventEmitter } = require("node:events");
const { PassThrough } = require("node:stream");
const { AcpClient } = require("../src/acpClient");

function childFixture() {
  const child = new EventEmitter();
  child.stdin = new PassThrough();
  child.stdout = new PassThrough();
  child.stderr = new PassThrough();
  child.kill = () => child.emit("exit", null, "SIGTERM");
  return child;
}

test("correlates responses and forwards notifications", async () => {
  const child = childFixture();
  const client = new AcpClient(child);
  const notifications = [];
  client.on("notification", (...args) => notifications.push(args));
  const response = client.request("initialize", { protocolVersion: 1 });
  const line = child.stdin.read()?.toString() ?? await new Promise((resolve) => child.stdin.once("data", (data) => resolve(data.toString())));
  const request = JSON.parse(line);
  child.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", method: "session/update", params: { ok: true } })}\n`);
  child.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", id: request.id, result: { protocolVersion: 1 } })}\n`);
  assert.deepEqual(await response, { protocolVersion: 1 });
  assert.deepEqual(notifications, [["session/update", { ok: true }]]);
  client.dispose();
});

test("rejects pending requests when the process exits", async () => {
  const child = childFixture();
  const client = new AcpClient(child);
  const response = client.request("session/new", {});
  child.emit("exit", 2, null);
  await assert.rejects(response, /exited/);
});
