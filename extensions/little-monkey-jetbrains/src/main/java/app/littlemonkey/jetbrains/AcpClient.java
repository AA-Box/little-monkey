package app.littlemonkey.jetbrains;

import com.google.gson.JsonElement;
import com.google.gson.JsonObject;
import com.google.gson.JsonParser;

import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.Closeable;
import java.io.IOException;
import java.io.InputStreamReader;
import java.io.OutputStreamWriter;
import java.nio.charset.StandardCharsets;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;
import java.util.function.Consumer;

/** Newline-delimited JSON-RPC client for the local ACP process. */
public final class AcpClient implements Closeable {
    static final int MAX_LINE_BYTES = 8 * 1024 * 1024;

    public interface NotificationListener {
        void onNotification(String method, JsonObject params);
    }

    private final Process process;
    private final BufferedReader input;
    private final BufferedWriter output;
    private final NotificationListener listener;
    private final Map<Long, CompletableFuture<JsonElement>> pending = new ConcurrentHashMap<>();
    private final AtomicLong ids = new AtomicLong(1);
    private final AtomicBoolean closed = new AtomicBoolean(false);
    private final Thread reader;
    private final Thread stderrReader;

    public AcpClient(Process process, NotificationListener listener) {
        this(process, listener, ignored -> {});
    }

    public AcpClient(Process process, NotificationListener listener, Consumer<String> stderrListener) {
        this(
            process,
            new BufferedReader(new InputStreamReader(process.getInputStream(), StandardCharsets.UTF_8)),
            new BufferedWriter(new OutputStreamWriter(process.getOutputStream(), StandardCharsets.UTF_8)),
            listener,
            stderrListener
        );
    }

    AcpClient(
        Process process,
        BufferedReader input,
        BufferedWriter output,
        NotificationListener listener,
        Consumer<String> stderrListener
    ) {
        this.process = process;
        this.input = input;
        this.output = output;
        this.listener = listener == null ? (method, params) -> {} : listener;
        this.reader = new Thread(this::readLoop, "little-monkey-acp-reader");
        this.reader.setDaemon(true);
        this.reader.start();
        this.stderrReader = new Thread(() -> readStderr(stderrListener), "little-monkey-acp-stderr");
        this.stderrReader.setDaemon(true);
        this.stderrReader.start();
    }

    public CompletableFuture<JsonElement> request(String method, JsonObject params) {
        if (closed.get()) return CompletableFuture.failedFuture(new IOException("ACP client is closed"));
        long id = ids.getAndIncrement();
        JsonObject message = new JsonObject();
        message.addProperty("jsonrpc", "2.0");
        message.addProperty("id", id);
        message.addProperty("method", method);
        message.add("params", params == null ? new JsonObject() : params);
        String line = message.toString();
        if (line.getBytes(StandardCharsets.UTF_8).length > MAX_LINE_BYTES) {
            return CompletableFuture.failedFuture(new IOException("ACP request exceeds 8 MiB"));
        }
        CompletableFuture<JsonElement> future = new CompletableFuture<>();
        pending.put(id, future);
        try {
            synchronized (output) {
                output.write(line);
                output.newLine();
                output.flush();
            }
        } catch (IOException error) {
            pending.remove(id);
            future.completeExceptionally(error);
        }
        return future;
    }

    private void readLoop() {
        try {
            String line;
            while (!closed.get() && (line = input.readLine()) != null) {
                if (line.getBytes(StandardCharsets.UTF_8).length > MAX_LINE_BYTES) {
                    throw new IOException("ACP response exceeds 8 MiB");
                }
                JsonObject message = JsonParser.parseString(line).getAsJsonObject();
                if (message.has("id") && message.get("id").isJsonPrimitive()) {
                    long id = message.get("id").getAsLong();
                    CompletableFuture<JsonElement> future = pending.remove(id);
                    if (future == null) continue;
                    if (message.has("error")) {
                        JsonObject error = message.getAsJsonObject("error");
                        future.completeExceptionally(new IOException(error.has("message")
                            ? error.get("message").getAsString()
                            : "ACP request failed"));
                    } else {
                        future.complete(message.get("result"));
                    }
                    continue;
                }
                if (message.has("method")) {
                    JsonObject params = message.has("params") && message.get("params").isJsonObject()
                        ? message.getAsJsonObject("params") : new JsonObject();
                    try {
                        listener.onNotification(message.get("method").getAsString(), params);
                    } catch (RuntimeException ignored) {
                        // A UI notification failure must not tear down the ACP transport.
                    }
                }
            }
            disconnect(new IOException("ACP process disconnected"));
        } catch (Exception error) {
            disconnect(error);
        }
    }

    private void readStderr(Consumer<String> stderrListener) {
        Consumer<String> sink = stderrListener == null ? ignored -> {} : stderrListener;
        try (BufferedReader stderr = new BufferedReader(
            new InputStreamReader(process.getErrorStream(), StandardCharsets.UTF_8)
        )) {
            String line;
            while (!closed.get() && (line = stderr.readLine()) != null) sink.accept(line);
        } catch (IOException ignored) {
            // The process lifecycle is authoritative; stderr is diagnostic only.
        }
    }

    private void disconnect(Exception error) {
        closed.set(true);
        failPending(error);
    }

    private void failPending(Exception error) {
        pending.values().forEach(future -> future.completeExceptionally(error));
        pending.clear();
    }

    public boolean isClosed() { return closed.get() || !process.isAlive(); }

    @Override
    public void close() {
        boolean wasOpen = closed.compareAndSet(false, true);
        if (wasOpen) failPending(new IOException("ACP client closed"));
        if (process.isAlive()) {
            process.destroy();
            if (process.isAlive()) process.destroyForcibly();
        }
        // Close the raw process streams before the buffered wrappers. A
        // BufferedReader may hold its lock while blocked in readLine(); closing
        // the underlying stream first releases that read without a deadlock.
        try { process.getInputStream().close(); } catch (IOException ignored) {}
        try { process.getOutputStream().close(); } catch (IOException ignored) {}
        try { process.getErrorStream().close(); } catch (IOException ignored) {}
        // Do not close BufferedReader here: BufferedReader holds its own lock
        // throughout a blocking readLine(). The daemon reader thread is
        // already released by closing the underlying process stream above.
    }
}
