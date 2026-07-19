package app.littlemonkey.jetbrains;

import com.google.gson.JsonArray;
import com.google.gson.JsonElement;
import com.google.gson.JsonObject;
import com.intellij.diff.DiffContentFactory;
import com.intellij.diff.DiffManager;
import com.intellij.diff.requests.SimpleDiffRequest;
import com.intellij.notification.Notification;
import com.intellij.notification.NotificationAction;
import com.intellij.notification.NotificationGroupManager;
import com.intellij.notification.NotificationType;
import com.intellij.openapi.Disposable;
import com.intellij.openapi.application.ApplicationManager;
import com.intellij.openapi.components.Service;
import com.intellij.openapi.editor.Editor;
import com.intellij.openapi.ide.CopyPasteManager;
import com.intellij.openapi.project.Project;
import com.intellij.openapi.util.text.StringUtil;

import java.io.IOException;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionException;

/**
 * One ACP process/session per project. The service sends immutable IDE context
 * and renders read-only previews. Permission decisions, file writes,
 * checkpoints, and verification remain authoritative in Little Monkey.
 */
@Service(Service.Level.PROJECT)
public final class LittleMonkeyProjectService implements Disposable {
    private static final String NOTIFICATION_GROUP = "Little Monkey";

    private final Project project;
    private final Object lock = new Object();
    private final StringBuilder transcript = new StringBuilder();

    private AcpClient client;
    private LittleMonkeyLaunchConfig launchConfig;
    private CompletableFuture<AcpClient> connecting;
    private CompletableFuture<JsonElement> activePrompt;
    private String sessionId;
    private volatile String runId;
    private long connectionGeneration;

    public LittleMonkeyProjectService(Project project) {
        this.project = project;
    }

    public static LittleMonkeyProjectService get(Project project) {
        return project.getService(LittleMonkeyProjectService.class);
    }

    public CompletableFuture<JsonElement> sendPrompt(Editor editor, String instruction) {
        if (instruction == null || instruction.isBlank()) {
            return CompletableFuture.failedFuture(new IllegalArgumentException("Prompt cannot be empty"));
        }

        final JsonArray prompt;
        try {
            prompt = EditorContext.prompt(editor, instruction.trim());
        } catch (RuntimeException error) {
            notifyError("Cannot capture editor context", error);
            return CompletableFuture.failedFuture(error);
        }

        return ensureConnection().thenCompose(connected -> {
            CompletableFuture<JsonElement> request;
            synchronized (lock) {
                if (activePrompt != null && !activePrompt.isDone()) {
                    throw new CompletionException(
                        new IllegalStateException("This project already has an active Little Monkey run")
                    );
                }
                transcript.setLength(0);
                request = connected.request("session/prompt", AcpProtocol.prompt(sessionId, prompt));
                activePrompt = request;
            }
            notifyInfo("Run queued", "Execution and approvals are controlled by Little Monkey.");
            request.whenComplete((result, error) -> finishPrompt(request, result, error));
            return request;
        }).whenComplete((ignored, error) -> {
            if (error != null) notifyError("Little Monkey run failed", unwrap(error));
        });
    }

    public CompletableFuture<Void> cancelActiveRun() {
        final AcpClient current;
        final String currentSession;
        synchronized (lock) {
            if (activePrompt == null || activePrompt.isDone() || client == null || sessionId == null) {
                notifyInfo("No active run", "There is no project run to cancel.");
                return CompletableFuture.completedFuture(null);
            }
            current = client;
            currentSession = sessionId;
        }
        return current.request("session/cancel", AcpProtocol.cancel(currentSession))
            .thenAccept(ignored -> notifyInfo("Cancellation requested", "Little Monkey is stopping the durable run."))
            .whenComplete((ignored, error) -> {
                if (error != null) notifyError("Could not cancel run", unwrap(error));
            });
    }

    public boolean hasActiveRun() {
        synchronized (lock) {
            return activePrompt != null && !activePrompt.isDone();
        }
    }

    public String currentRunId() {
        return runId;
    }

    public void showRun() {
        String currentRun = runId;
        if (currentRun == null || currentRun.isBlank()) {
            notifyInfo("No attached run", "Start a Little Monkey request from this project first.");
            return;
        }
        String command = attachCommand(currentRun);
        Notification notification = notification(
            "Durable run " + currentRun.substring(0, Math.min(8, currentRun.length())),
            "Attach from a terminal: <code>" + StringUtil.escapeXmlEntities(command) + "</code>",
            NotificationType.INFORMATION
        );
        notification.addAction(NotificationAction.createSimple("Copy attach command", () -> {
            CopyPasteManager.copyTextToClipboard(command);
        }));
        notification.notify(project);
    }

    public void settingsChanged() {
        closeConnection();
    }

    private CompletableFuture<AcpClient> ensureConnection() {
        final LittleMonkeyLaunchConfig requested;
        try {
            LittleMonkeySettings.Data settings = LittleMonkeySettings.get(project).data();
            requested = LittleMonkeyLaunchConfig.create(
                settings.cliPath,
                settings.ollamaModel,
                settings.permissionMode,
                project.getBasePath()
            );
        } catch (Exception error) {
            return CompletableFuture.failedFuture(error);
        }

        synchronized (lock) {
            if (client != null && !client.isClosed() && requested.equals(launchConfig) && sessionId != null) {
                return CompletableFuture.completedFuture(client);
            }
            if (connecting != null && requested.equals(launchConfig)) return connecting;
            closeConnectionLocked();
            launchConfig = requested;
            long generation = connectionGeneration;
            connecting = CompletableFuture.supplyAsync(() -> connect(requested, generation));
            CompletableFuture<AcpClient> result = connecting;
            result.whenComplete((connected, error) -> {
                synchronized (lock) {
                    if (connecting == result) connecting = null;
                    if (error != null && connectionGeneration == generation) launchConfig = null;
                }
            });
            return result;
        }
    }

    private AcpClient connect(LittleMonkeyLaunchConfig config, long generation) {
        Process process = null;
        AcpClient connected = null;
        try {
            process = new ProcessBuilder(config.command())
                .directory(config.workspace().toFile())
                .redirectErrorStream(false)
                .start();
            connected = new AcpClient(process, this::onNotification, line -> {
                if (!line.isBlank()) notifyWarning("Little Monkey CLI", line);
            });
            JsonElement initialized = connected.request("initialize", AcpProtocol.initialize()).join();
            if (initialized == null || !initialized.isJsonObject()
                || !initialized.getAsJsonObject().has("protocolVersion")
                || initialized.getAsJsonObject().get("protocolVersion").getAsInt() != 1) {
                throw new IOException("Little Monkey returned an invalid ACP initialize response");
            }
            LittleMonkeySettings.Data persisted = LittleMonkeySettings.get(project).data();
            String createdSession = null;
            boolean sameStoredContext = config.workspace().toString().equals(persisted.acpSessionWorkspace)
                && config.model().equals(persisted.acpSessionModel)
                && config.permissionMode().equals(persisted.acpSessionPermissionMode);
            if (AcpProtocol.supportsResume(initialized) && sameStoredContext
                && persisted.acpSessionId != null && !persisted.acpSessionId.isBlank()) {
                try {
                    JsonElement resumed = connected.request(
                        "session/resume",
                        AcpProtocol.resumeSession(persisted.acpSessionId, config.workspace().toString())
                    ).join();
                    createdSession = resumed != null && resumed.isJsonObject()
                        && resumed.getAsJsonObject().has("sessionId")
                        ? resumed.getAsJsonObject().get("sessionId").getAsString()
                        : persisted.acpSessionId;
                } catch (RuntimeException resumeError) {
                    notifyWarning("ACP reconnect", "The previous session could not be resumed; a new durable session will be created.");
                    persisted.acpSessionId = "";
                }
            }
            if (createdSession == null || createdSession.isBlank()) {
                JsonElement created = connected.request(
                    "session/new",
                    AcpProtocol.newSession(config.workspace().toString())
                ).join();
                if (created == null || !created.isJsonObject() || !created.getAsJsonObject().has("sessionId")) {
                    throw new IOException("Little Monkey returned an invalid ACP session response");
                }
                createdSession = created.getAsJsonObject().get("sessionId").getAsString();
            }
            if (createdSession.isBlank()) throw new IOException("Little Monkey returned an empty ACP session id");
            persisted.acpSessionId = createdSession;
            persisted.acpSessionWorkspace = config.workspace().toString();
            persisted.acpSessionModel = config.model();
            persisted.acpSessionPermissionMode = config.permissionMode();
            synchronized (lock) {
                if (connectionGeneration != generation || project.isDisposed()) {
                    throw new IOException("ACP connection was superseded by new project settings");
                }
                client = connected;
                sessionId = createdSession;
            }
            notifyInfo("Connected", "ACP v1 is ready for this project.");
            return connected;
        } catch (Exception error) {
            if (connected != null) connected.close();
            else if (process != null) process.destroyForcibly();
            throw new CompletionException(unwrap(error));
        }
    }

    private void onNotification(String method, JsonObject params) {
        if ("little-monkey/run".equals(method)) {
            String id = AcpProtocol.runId(params);
            if (!id.isBlank()) runId = id;
            return;
        }
        if (!"session/update".equals(method)) return;

        String text = AcpProtocol.updateText(params);
        if (!text.isEmpty()) {
            synchronized (transcript) {
                int room = 32_000 - transcript.length();
                if (room > 0) transcript.append(text, 0, Math.min(text.length(), room));
            }
        }
        String title = AcpProtocol.updateTitle(params);
        String status = AcpProtocol.updateStatus(params);
        if (!title.isBlank()) {
            if (title.startsWith("Approval required in Little Monkey")) {
                notifyWarning("Approval required", title + ". Approve or deny it in Little Monkey.");
            } else if ("failed".equals(status)) {
                notifyWarning(title, "The Little Monkey tool reported a failure.");
            }
        }
        for (AcpProtocol.DiffPreview diff : AcpProtocol.diffs(params)) showNativeDiff(diff);
    }

    private void showNativeDiff(AcpProtocol.DiffPreview preview) {
        ApplicationManager.getApplication().invokeLater(() -> {
            if (project.isDisposed()) return;
            var factory = DiffContentFactory.getInstance();
            var request = new SimpleDiffRequest(
                "Little Monkey preview: " + preview.path(),
                factory.create(project, preview.oldText()),
                factory.create(project, preview.newText()),
                "Before",
                "Run result (read-only preview)"
            );
            DiffManager.getInstance().showDiff(project, request);
            notifyInfo("Diff preview opened", "The IDE did not apply any edit. Review the working tree and Little Monkey audit trail.");
        });
    }

    private void finishPrompt(
        CompletableFuture<JsonElement> request,
        JsonElement result,
        Throwable error
    ) {
        synchronized (lock) {
            if (activePrompt == request) activePrompt = null;
        }
        if (error != null) return;
        String excerpt;
        synchronized (transcript) {
            excerpt = transcript.toString().trim();
        }
        String stopReason = result != null && result.isJsonObject()
            && result.getAsJsonObject().has("stopReason")
            ? result.getAsJsonObject().get("stopReason").getAsString() : "complete";
        if (excerpt.length() > 800) excerpt = excerpt.substring(excerpt.length() - 800);
        notifyInfo(
            "Run " + stopReason,
            excerpt.isBlank() ? "The durable run finished in Little Monkey." : excerpt
        );
    }

    private String attachCommand(String id) {
        String cli = LittleMonkeySettings.get(project).data().cliPath;
        return shellQuote(cli == null || cli.isBlank() ? "monkey" : cli.trim())
            + " daemon attach " + shellQuote(id);
    }

    static String shellQuote(String value) {
        if (value.matches("[A-Za-z0-9_./:@%+=,-]+")) return value;
        return "'" + value.replace("'", "'\\''") + "'";
    }

    private void notifyInfo(String title, String content) {
        notification(title, StringUtil.escapeXmlEntities(content), NotificationType.INFORMATION).notify(project);
    }

    private void notifyWarning(String title, String content) {
        notification(title, StringUtil.escapeXmlEntities(content), NotificationType.WARNING).notify(project);
    }

    private void notifyError(String title, Throwable error) {
        String message = error == null || error.getMessage() == null ? "Unknown error" : error.getMessage();
        notification(title, StringUtil.escapeXmlEntities(message), NotificationType.ERROR).notify(project);
    }

    private Notification notification(String title, String content, NotificationType type) {
        return NotificationGroupManager.getInstance()
            .getNotificationGroup(NOTIFICATION_GROUP)
            .createNotification(title, content, type);
    }

    private static Throwable unwrap(Throwable error) {
        Throwable result = error;
        while ((result instanceof CompletionException || result instanceof java.util.concurrent.ExecutionException)
            && result.getCause() != null) {
            result = result.getCause();
        }
        return result;
    }

    private void closeConnection() {
        synchronized (lock) {
            closeConnectionLocked();
        }
    }

    private void closeConnectionLocked() {
        connectionGeneration++;
        if (client != null) client.close();
        client = null;
        sessionId = null;
        connecting = null;
        launchConfig = null;
        activePrompt = null;
    }

    @Override
    public void dispose() {
        closeConnection();
    }
}
