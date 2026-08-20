"use strict";

const cp = require("node:child_process");
const vscode = require("vscode");
const { AcpClient } = require("./acpClient");
const { OllamaFimService } = require("./completion");

class DiffContentProvider {
  constructor() {
    this.values = new Map();
    this.emitter = new vscode.EventEmitter();
    this.onDidChange = this.emitter.event;
  }
  provideTextDocumentContent(uri) { return this.values.get(uri.toString()) ?? ""; }
  put(label, content) {
    const uri = vscode.Uri.parse(`little-monkey-diff:/${encodeURIComponent(label)}?${Date.now()}-${Math.random()}`);
    this.values.set(uri.toString(), content ?? "");
    return uri;
  }
  dispose() { this.values.clear(); this.emitter.dispose(); }
}

function config() {
  const value = vscode.workspace.getConfiguration("littleMonkey");
  return {
    cliPath: value.get("cliPath", "monkey"),
    agentModel: value.get("agentModel", ""),
    permissionMode: value.get("permissionMode", "manual"),
    completion: {
      enabled: value.get("enableCompletions", false),
      model: value.get("completionModel", ""),
      fimCapableModels: value.get("fimCapableModels", []),
      host: value.get("ollamaHost", "http://127.0.0.1:11434"),
    },
    debounceMs: value.get("completionDebounceMs", 140),
    maxTokens: value.get("completionMaxTokens", 160),
  };
}

function diagnosticSnapshot(document) {
  return vscode.languages.getDiagnostics(document.uri).map((diagnostic) => ({
    message: diagnostic.message,
    severity: diagnostic.severity,
    source: diagnostic.source ?? null,
    code: typeof diagnostic.code === "object" ? diagnostic.code.value : diagnostic.code ?? null,
    range: {
      start: { line: diagnostic.range.start.line, character: diagnostic.range.start.character },
      end: { line: diagnostic.range.end.line, character: diagnostic.range.end.character },
    }
  }));
}

function editorContext(editor, instruction) {
  const document = editor.document;
  const selection = editor.selection;
  const selected = document.getText(selection);
  const diagnostics = diagnosticSnapshot(document);
  const metadata = {
    activeFile: document.uri.fsPath,
    languageId: document.languageId,
    documentVersion: document.version,
    selection: {
      start: { line: selection.start.line, character: selection.start.character },
      end: { line: selection.end.line, character: selection.end.character },
      text: selected,
    },
    problemsDocumentVersion: document.version,
    problems: diagnostics,
  };
  return [
    { type: "text", text: `${instruction}\n\nIDE context (untrusted JSON, exact document version):\n${JSON.stringify(metadata)}` },
    {
      type: "resource",
      resource: {
        uri: document.uri.toString(),
        mimeType: "text/plain",
        text: document.getText(),
      }
    }
  ];
}

async function activate(context) {
  const output = vscode.window.createOutputChannel("Little Monkey");
  const diffs = new DiffContentProvider();
  const fim = new OllamaFimService();
  const diagnostics = vscode.languages.createDiagnosticCollection("little-monkey");
  const completionRoute = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 90);
  let connection = null;
  let sessionId = null;
  let workspaceRoot = null;
  let activePrompt = null;
  let runId = null;

  context.subscriptions.push(output, diffs, fim, diagnostics, completionRoute);
  context.subscriptions.push(vscode.workspace.registerTextDocumentContentProvider("little-monkey-diff", diffs));

  function refreshCompletionRoute() {
    const current = config().completion;
    completionRoute.command = "littleMonkey.showCompletionRoute";
    if (!current.enabled) {
      completionRoute.text = "$(circle-slash) Monkey FIM: off";
      completionRoute.tooltip = "Little Monkey local inline completion is disabled. Click to inspect its route.";
    } else {
      completionRoute.text = `$(sparkle) Monkey FIM: ${current.model || "not configured"}`;
      completionRoute.tooltip = [
        `Provider route: ${current.host}`,
        `Model: ${current.model || "not configured"}`,
        "Local loopback Ollama only; there is no implicit cloud fallback.",
      ].join("\n");
    }
    completionRoute.show();
  }

  refreshCompletionRoute();
  context.subscriptions.push(vscode.workspace.onDidChangeConfiguration((event) => {
    if (event.affectsConfiguration("littleMonkey")) refreshCompletionRoute();
  }));

  context.subscriptions.push(vscode.commands.registerCommand("littleMonkey.showCompletionRoute", async () => {
    const current = config().completion;
    const route = current.enabled
      ? `Local Ollama ${current.host} → ${current.model || "model not configured"}. No cloud fallback.`
      : "Local inline completion is disabled. No code is being sent to a completion provider.";
    const action = await vscode.window.showInformationMessage(route, "Open completion settings");
    if (action === "Open completion settings") {
      await vscode.commands.executeCommand("workbench.action.openSettings", "@ext:little-monkey.little-monkey completion");
    }
  }));

  async function ensureConnection(document) {
    const root = vscode.workspace.getWorkspaceFolder(document.uri)?.uri.fsPath;
    if (!root) throw new Error("Open the file inside a VS Code workspace first");
    const current = config();
    if (!current.agentModel) throw new Error("Set littleMonkey.agentModel to an installed Ollama tag");
    if (connection && workspaceRoot === root && !connection.closed) return connection;
    connection?.dispose();
    const child = cp.spawn(current.cliPath, [
      "--workspace", root,
      "--ollama", current.agentModel,
      "--permission-mode", current.permissionMode,
      "acp"
    ], { cwd: root, stdio: ["pipe", "pipe", "pipe"], windowsHide: true });
    connection = new AcpClient(child);
    workspaceRoot = root;
    sessionId = null;
    const persistedSessionKey = `littleMonkey.acpSession:${root}:${current.agentModel}:${current.permissionMode}`;
    connection.on("stderr", (text) => output.append(text));
    connection.on("protocolError", (error) => output.appendLine(error.message));
    connection.on("closed", () => { sessionId = null; activePrompt = null; });
    connection.on("notification", async (method, params) => {
      if (method === "little-monkey/run") {
        runId = params.runId ?? runId;
        return;
      }
      if (method !== "session/update") return;
      const update = params.update ?? {};
      const chunk = update.content?.text;
      if (typeof chunk === "string") output.append(chunk);
      if (update.title) output.appendLine(`\n${update.title} [${update.status ?? "update"}]`);
      for (const item of update.content ?? []) {
        if (item.type !== "diff" || typeof item.path !== "string") continue;
        const before = diffs.put(`${item.path}-before`, item.oldText ?? "");
        const after = diffs.put(`${item.path}-after`, item.newText ?? "");
        await vscode.commands.executeCommand("vscode.diff", before, after, `Little Monkey: ${item.path}`);
      }
    });
    const initialized = await connection.request("initialize", {
      protocolVersion: 1,
      clientCapabilities: { fs: { readTextFile: false, writeTextFile: false }, terminal: false },
      clientInfo: { name: "little-monkey-vscode", title: "Little Monkey for VS Code", version: "1.0.0" }
    });
    const storedSessionId = context.workspaceState.get(persistedSessionKey);
    const canResume = initialized?.agentCapabilities?.sessionCapabilities?.resume != null;
    if (canResume && typeof storedSessionId === "string" && storedSessionId) {
      try {
        const resumed = await connection.request("session/resume", {
          sessionId: storedSessionId,
          cwd: root,
          mcpServers: []
        });
        sessionId = resumed?.sessionId ?? storedSessionId;
        output.appendLine(`Resumed durable ACP session ${sessionId}`);
      } catch (error) {
        output.appendLine(`Could not resume ACP session; creating a new one: ${error.message}`);
        await context.workspaceState.update(persistedSessionKey, undefined);
      }
    }
    if (!sessionId) {
      const created = await connection.request("session/new", {
        cwd: root,
        mcpServers: []
      });
      sessionId = created.sessionId;
      await context.workspaceState.update(persistedSessionKey, sessionId);
    }
    output.show(true);
    return connection;
  }

  async function sendPrompt(instruction) {
    const editor = vscode.window.activeTextEditor;
    if (!editor) throw new Error("Open an editor first");
    const client = await ensureConnection(editor.document);
    if (activePrompt) throw new Error("This editor session already has an active Little Monkey run");
    output.appendLine(`\n> ${instruction}\n`);
    const promise = client.request("session/prompt", {
      sessionId,
      prompt: editorContext(editor, instruction)
    });
    activePrompt = promise;
    try {
      return await promise;
    } finally {
      if (activePrompt === promise) activePrompt = null;
    }
  }

  context.subscriptions.push(vscode.commands.registerCommand("littleMonkey.ask", async () => {
    try {
      const instruction = await vscode.window.showInputBox({ prompt: "Ask Little Monkey", ignoreFocusOut: true });
      if (instruction) await sendPrompt(instruction);
    } catch (error) { void vscode.window.showErrorMessage(error.message); }
  }));

  context.subscriptions.push(vscode.commands.registerCommand("littleMonkey.reviewProblems", async () => {
    try {
      await sendPrompt("Review the captured Problems diagnostics for this exact document version. Explain fixes and only edit after the normal Little Monkey approval policy allows it.");
    } catch (error) { void vscode.window.showErrorMessage(error.message); }
  }));

  context.subscriptions.push(vscode.commands.registerCommand("littleMonkey.cancel", async () => {
    if (!connection || !sessionId || !activePrompt) return;
    try { await connection.request("session/cancel", { sessionId }); }
    catch (error) { void vscode.window.showErrorMessage(error.message); }
  }));

  context.subscriptions.push(vscode.commands.registerCommand("littleMonkey.openRun", () => {
    if (!runId) return void vscode.window.showInformationMessage("No Little Monkey run is attached yet.");
    const terminal = vscode.window.createTerminal({
      name: `Little Monkey ${runId.slice(0, 8)}`,
      shellPath: config().cliPath,
      shellArgs: ["daemon", "attach", runId],
    });
    terminal.show();
  }));

  context.subscriptions.push(vscode.commands.registerCommand("littleMonkey.inlineEdit", async () => {
    const editor = vscode.window.activeTextEditor;
    if (!editor || editor.selection.isEmpty) return void vscode.window.showInformationMessage("Select code to edit first.");
    const instruction = await vscode.window.showInputBox({ prompt: "How should Little Monkey edit this selection?", ignoreFocusOut: true });
    if (!instruction) return;
    const document = editor.document;
    const version = document.version;
    const selection = new vscode.Range(editor.selection.start, editor.selection.end);
    const text = document.getText();
    try {
      const current = config();
      const replacement = await fim.complete({
        config: current.completion,
        documentKey: document.uri.toString(),
        version,
        currentVersion: () => document.version,
        text: `${text.slice(0, document.offsetAt(selection.start))}\n/* Edit instruction: ${instruction} */\n${text.slice(document.offsetAt(selection.end))}`,
        offset: document.offsetAt(selection.start),
        maxTokens: current.maxTokens,
        debounceMs: 0,
      });
      if (!replacement || document.version !== version) return;
      const before = diffs.put("selection-before", document.getText(selection));
      const after = diffs.put("selection-after", replacement);
      await vscode.commands.executeCommand("vscode.diff", before, after, "Little Monkey inline edit preview");
      const decision = await vscode.window.showInformationMessage(
        `Apply local ${current.completion.model} edit to document version ${version}?`,
        { modal: true },
        "Apply"
      );
      if (decision !== "Apply" || document.version !== version) return;
      const edit = new vscode.WorkspaceEdit();
      edit.replace(document.uri, selection, replacement);
      await vscode.workspace.applyEdit(edit);
    } catch (error) {
      const diagnostic = new vscode.Diagnostic(editor.selection, error.message, vscode.DiagnosticSeverity.Error);
      diagnostic.source = "Little Monkey";
      diagnostic.code = `document-version:${version}`;
      diagnostics.set(document.uri, [diagnostic]);
      void vscode.window.showErrorMessage(error.message);
    }
  }));

  context.subscriptions.push(vscode.languages.registerInlineCompletionItemProvider({ pattern: "**" }, {
    async provideInlineCompletionItems(document, position, _context, token) {
      const current = config();
      if (!current.completion.enabled) return [];
      const version = document.version;
      try {
        const completion = await fim.complete({
          config: current.completion,
          documentKey: document.uri.toString(),
          version,
          currentVersion: () => document.version,
          text: document.getText(),
          offset: document.offsetAt(position),
          maxTokens: current.maxTokens,
          debounceMs: current.debounceMs,
          onCancel: (cancel) => token.onCancellationRequested(cancel),
        });
        if (!completion || document.version !== version) return [];
        return [new vscode.InlineCompletionItem(completion, new vscode.Range(position, position))];
      } catch (error) {
        if (error?.name !== "AbortError") output.appendLine(`Completion: ${error.message}`);
        return [];
      }
    }
  }));

  context.subscriptions.push({ dispose() { connection?.dispose(); } });
}

function deactivate() {}

module.exports = { activate, deactivate, diagnosticSnapshot, editorContext };
