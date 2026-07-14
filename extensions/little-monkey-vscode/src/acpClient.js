"use strict";

const { EventEmitter } = require("node:events");

const MAX_LINE_BYTES = 8 * 1024 * 1024;

class AcpClient extends EventEmitter {
  constructor(child) {
    super();
    this.child = child;
    this.nextId = 1;
    this.pending = new Map();
    this.buffer = "";
    this.closed = false;
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk) => this.onData(chunk));
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (chunk) => this.emit("stderr", String(chunk)));
    child.once("error", (error) => this.close(error));
    child.once("exit", (code, signal) => {
      this.close(new Error(`Little Monkey ACP exited (${code ?? signal ?? "unknown"})`));
    });
  }

  request(method, params = {}) {
    if (this.closed) return Promise.reject(new Error("ACP connection is closed"));
    const id = this.nextId++;
    const message = { jsonrpc: "2.0", id, method, params };
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.write(message).catch((error) => {
        this.pending.delete(id);
        reject(error);
      });
    });
  }

  notify(method, params = {}) {
    return this.write({ jsonrpc: "2.0", method, params });
  }

  async write(message) {
    const line = `${JSON.stringify(message)}\n`;
    if (Buffer.byteLength(line) > MAX_LINE_BYTES) throw new Error("ACP request exceeds 8 MiB");
    await new Promise((resolve, reject) => {
      this.child.stdin.write(line, "utf8", (error) => (error ? reject(error) : resolve()));
    });
  }

  onData(chunk) {
    this.buffer += chunk;
    if (Buffer.byteLength(this.buffer) > MAX_LINE_BYTES * 2) {
      this.close(new Error("ACP response buffer exceeded its limit"));
      this.child.kill();
      return;
    }
    for (;;) {
      const newline = this.buffer.indexOf("\n");
      if (newline < 0) break;
      const line = this.buffer.slice(0, newline);
      this.buffer = this.buffer.slice(newline + 1);
      if (!line.trim()) continue;
      let message;
      try {
        message = JSON.parse(line);
      } catch {
        this.emit("protocolError", new Error("ACP returned invalid JSON"));
        continue;
      }
      if (Object.prototype.hasOwnProperty.call(message, "id")) {
        const pending = this.pending.get(message.id);
        if (!pending) continue;
        this.pending.delete(message.id);
        if (message.error) pending.reject(new Error(message.error.message || "ACP request failed"));
        else pending.resolve(message.result);
      } else if (message.method) {
        this.emit("notification", message.method, message.params || {});
      }
    }
  }

  close(error = new Error("ACP connection closed")) {
    if (this.closed) return;
    this.closed = true;
    for (const pending of this.pending.values()) pending.reject(error);
    this.pending.clear();
    this.emit("closed", error);
  }

  dispose() {
    if (!this.closed) this.child.kill();
    this.close();
  }
}

module.exports = { AcpClient, MAX_LINE_BYTES };
