package app.littlemonkey.jetbrains;

import com.google.gson.JsonArray;
import com.google.gson.JsonObject;
import com.google.gson.JsonParser;

import java.io.BufferedReader;
import java.io.ByteArrayInputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.io.PipedInputStream;
import java.io.PipedOutputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.concurrent.CompletionException;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicReference;

/** Dependency-light ACP v1 contract suite runnable without an IntelliJ test harness. */
public final class AcpContractTest {
    private static int passed;

    public static void main(String[] args) throws Exception {
        System.out.println("contract: message builders");
        messageBuildersKeepTheIdeUnprivileged();
        System.out.println("contract: update parser");
        updateParserExtractsOnlyReadOnlyPreviews();
        System.out.println("contract: launch argv");
        launchArgumentsAreValidatedAndNeverUseAShell();
        System.out.println("contract: transport success");
        transportCorrelatesResponsesAndForwardsNotifications();
        System.out.println("contract: RPC error");
        transportSurfacesRpcErrors();
        System.out.println("contract: size limit");
        transportRejectsOversizedRequests();
        System.out.println("contract: disconnect");
        transportRejectsPendingRequestsOnDisconnect();
        System.out.println("ACP contract tests: " + passed + " passed");
    }

    private static void messageBuildersKeepTheIdeUnprivileged() {
        JsonObject initialize = AcpProtocol.initialize();
        check(initialize.get("protocolVersion").getAsInt() == 1, "ACP v1 is negotiated");
        JsonObject capabilities = initialize.getAsJsonObject("clientCapabilities");
        check(!capabilities.get("terminal").getAsBoolean(), "IDE terminal capability remains disabled");
        check(!capabilities.getAsJsonObject("fs").get("readTextFile").getAsBoolean(), "IDE ACP file reads remain disabled");
        check(!capabilities.getAsJsonObject("fs").get("writeTextFile").getAsBoolean(), "IDE ACP file writes remain disabled");

        JsonObject session = AcpProtocol.newSession("/workspace");
        check(session.get("cwd").getAsString().equals("/workspace"), "workspace is negotiated explicitly");
        check(session.getAsJsonArray("mcpServers").isEmpty(), "IDE cannot inject MCP servers");
        JsonObject resume = AcpProtocol.resumeSession("session-1", "/workspace");
        check(resume.get("sessionId").getAsString().equals("session-1"), "reconnect preserves the durable session id");
        check(!AcpProtocol.supportsResume(JsonParser.parseString("{\"agentCapabilities\":{\"sessionCapabilities\":{}}}")), "resume is capability gated");
        check(AcpProtocol.supportsResume(JsonParser.parseString("{\"agentCapabilities\":{\"sessionCapabilities\":{\"resume\":{}}}}")), "stable ACP resume capability is recognized");

        JsonArray blocks = new JsonArray();
        JsonObject text = new JsonObject();
        text.addProperty("type", "text");
        text.addProperty("text", "review this");
        blocks.add(text);
        JsonObject prompt = AcpProtocol.prompt("session-1", blocks);
        check(prompt.get("sessionId").getAsString().equals("session-1"), "prompt stays in negotiated session");
        check(prompt.getAsJsonArray("prompt").size() == 1, "prompt blocks are preserved");
        check(AcpProtocol.cancel("session-1").get("sessionId").getAsString().equals("session-1"), "cancellation is session scoped");
    }

    private static void updateParserExtractsOnlyReadOnlyPreviews() {
        JsonObject params = JsonParser.parseString("""
            {
              "update": {
                "title": "Changed src/Main.java",
                "status": "completed",
                "content": [
                  {"type":"diff","path":"/workspace/src/Main.java","oldText":"old","newText":"new"},
                  {"type":"content","content":{"type":"text","text":"ignored"}},
                  {"type":"diff","path":"","oldText":"bad","newText":"bad"}
                ]
              }
            }
            """).getAsJsonObject();
        List<AcpProtocol.DiffPreview> diffs = AcpProtocol.diffs(params);
        check(diffs.size() == 1, "only valid diff previews are accepted");
        check(diffs.get(0).path().equals("/workspace/src/Main.java"), "diff path is preserved");
        check(diffs.get(0).oldText().equals("old") && diffs.get(0).newText().equals("new"), "diff sides are immutable strings");
        check(AcpProtocol.updateTitle(params).equals("Changed src/Main.java"), "tool title is parsed");
        check(AcpProtocol.updateStatus(params).equals("completed"), "tool status is parsed");

        JsonObject chunk = JsonParser.parseString("""
            {"update":{"content":{"type":"text","text":"streamed"}}}
            """).getAsJsonObject();
        check(AcpProtocol.updateText(chunk).equals("streamed"), "agent text chunks are parsed");
        JsonObject run = new JsonObject();
        run.addProperty("runId", "run-123");
        check(AcpProtocol.runId(run).equals("run-123"), "durable run id is parsed");
    }

    private static void launchArgumentsAreValidatedAndNeverUseAShell() throws Exception {
        Path workspace = Files.createTempDirectory("little-monkey-jetbrains-contract-");
        LittleMonkeyLaunchConfig config = LittleMonkeyLaunchConfig.create(
            "/Applications/Little Monkey/monkey-cli",
            "qwen2.5-coder:7b",
            "manual",
            workspace.toString()
        );
        List<String> command = config.command();
        check(command.get(0).equals("/Applications/Little Monkey/monkey-cli"), "CLI path stays one argv value");
        check(command.contains("--workspace") && command.contains(workspace.toRealPath().toString()), "canonical workspace is fixed in argv");
        check(command.get(command.size() - 1).equals("acp"), "ACP is the only launched subcommand");
        expectFailure(() -> LittleMonkeyLaunchConfig.create("monkey-cli", "model", "bypass", workspace.toString()), "bypass mode is forbidden");
        expectFailure(() -> LittleMonkeyLaunchConfig.create("monkey-cli", "", "manual", workspace.toString()), "agent model is required");
        Files.deleteIfExists(workspace);
    }

    private static void transportCorrelatesResponsesAndForwardsNotifications() throws Exception {
        try (Fixture fixture = new Fixture()) {
            AtomicReference<String> method = new AtomicReference<>();
            AtomicReference<JsonObject> notificationParams = new AtomicReference<>();
            CountDownLatch notificationSeen = new CountDownLatch(1);
            AcpClient client = new AcpClient(fixture.process, (name, params) -> {
                method.set(name);
                notificationParams.set(params);
                notificationSeen.countDown();
            });
            var response = client.request("initialize", AcpProtocol.initialize());
            JsonObject request = JsonParser.parseString(fixture.requests.readLine()).getAsJsonObject();
            check(request.get("jsonrpc").getAsString().equals("2.0"), "transport emits JSON-RPC 2.0");
            check(request.get("method").getAsString().equals("initialize"), "transport emits requested method");
            fixture.send("{\"jsonrpc\":\"2.0\",\"method\":\"little-monkey/run\",\"params\":{\"runId\":\"r1\"}}");
            fixture.send("{\"jsonrpc\":\"2.0\",\"id\":" + request.get("id") + ",\"result\":{\"protocolVersion\":1}}");
            check(response.get(2, TimeUnit.SECONDS).getAsJsonObject().get("protocolVersion").getAsInt() == 1, "response is correlated by id");
            boolean delivered = notificationSeen.await(2, TimeUnit.SECONDS);
            check(delivered, "notification is delivered");
            check(method.get().equals("little-monkey/run") && notificationParams.get().get("runId").getAsString().equals("r1"), "notification payload is preserved");
            client.close();
        }
    }

    private static void transportSurfacesRpcErrors() throws Exception {
        try (Fixture fixture = new Fixture()) {
            AcpClient client = new AcpClient(fixture.process, null);
            var response = client.request("session/new", new JsonObject());
            JsonObject request = JsonParser.parseString(fixture.requests.readLine()).getAsJsonObject();
            fixture.send("{\"jsonrpc\":\"2.0\",\"id\":" + request.get("id") + ",\"error\":{\"code\":-32602,\"message\":\"cwd is required\"}}");
            expectAsyncFailure(response, "cwd is required", "RPC error message reaches the caller");
            client.close();
        }
    }

    private static void transportRejectsOversizedRequests() throws Exception {
        try (Fixture fixture = new Fixture()) {
            AcpClient client = new AcpClient(fixture.process, null);
            JsonObject params = new JsonObject();
            params.addProperty("payload", "x".repeat(AcpClient.MAX_LINE_BYTES));
            expectAsyncFailure(client.request("session/prompt", params), "8 MiB", "oversized RPC requests are rejected");
            client.close();
        }
    }

    private static void transportRejectsPendingRequestsOnDisconnect() throws Exception {
        try (Fixture fixture = new Fixture()) {
            AcpClient client = new AcpClient(fixture.process, null);
            var response = client.request("session/new", new JsonObject());
            fixture.requests.readLine();
            fixture.disconnectServer();
            expectAsyncFailure(response, "disconnected", "pending RPC fails on disconnect");
            client.close();
        }
    }

    private static void expectAsyncFailure(
        java.util.concurrent.CompletableFuture<?> future,
        String messagePart,
        String label
    ) {
        try {
            future.join();
            throw new AssertionError(label + ": expected failure");
        } catch (CompletionException error) {
            Throwable cause = error.getCause();
            check(cause != null && cause.getMessage().contains(messagePart), label);
        }
    }

    private static void expectFailure(ThrowingRunnable action, String label) throws Exception {
        try {
            action.run();
            throw new AssertionError(label + ": expected failure");
        } catch (IllegalArgumentException expected) {
            passed++;
        }
    }

    private static void check(boolean condition, String label) {
        if (!condition) throw new AssertionError(label);
        passed++;
    }

    @FunctionalInterface
    private interface ThrowingRunnable {
        void run() throws Exception;
    }

    private static final class Fixture implements AutoCloseable {
        private final PipedInputStream clientInput = new PipedInputStream();
        private final PipedOutputStream serverOutput;
        private final PipedInputStream serverInput = new PipedInputStream();
        private final PipedOutputStream clientOutput;
        private final FakeProcess process;
        private final BufferedReader requests;

        Fixture() throws IOException {
            serverOutput = new PipedOutputStream(clientInput);
            clientOutput = new PipedOutputStream(serverInput);
            process = new FakeProcess(clientInput, clientOutput);
            requests = new BufferedReader(new InputStreamReader(serverInput, StandardCharsets.UTF_8));
        }

        void send(String json) throws IOException {
            serverOutput.write((json + "\n").getBytes(StandardCharsets.UTF_8));
            serverOutput.flush();
        }

        void disconnectServer() throws IOException {
            process.alive.set(false);
            serverOutput.close();
        }

        @Override
        public void close() {
            try { requests.close(); } catch (IOException ignored) {}
            try { serverOutput.close(); } catch (IOException ignored) {}
            try { clientInput.close(); } catch (IOException ignored) {}
            try { clientOutput.close(); } catch (IOException ignored) {}
        }
    }

    private static final class FakeProcess extends Process {
        private final InputStream input;
        private final OutputStream output;
        private final AtomicBoolean alive = new AtomicBoolean(true);

        FakeProcess(InputStream input, OutputStream output) {
            this.input = input;
            this.output = output;
        }

        @Override public OutputStream getOutputStream() { return output; }
        @Override public InputStream getInputStream() { return input; }
        @Override public InputStream getErrorStream() { return new ByteArrayInputStream(new byte[0]); }
        @Override public int waitFor() { alive.set(false); return 0; }
        @Override public int exitValue() { if (alive.get()) throw new IllegalThreadStateException(); return 0; }
        @Override public void destroy() {
            alive.set(false);
            try { input.close(); } catch (IOException ignored) {}
            try { output.close(); } catch (IOException ignored) {}
        }
        @Override public Process destroyForcibly() { destroy(); return this; }
        @Override public boolean isAlive() { return alive.get(); }
    }
}
