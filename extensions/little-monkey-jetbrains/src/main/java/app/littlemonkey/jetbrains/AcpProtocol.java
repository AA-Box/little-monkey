package app.littlemonkey.jetbrains;

import com.google.gson.JsonArray;
import com.google.gson.JsonElement;
import com.google.gson.JsonObject;

import java.util.ArrayList;
import java.util.List;

/** Pure ACP v1 message builders/parsers shared by the IDE service and contract tests. */
final class AcpProtocol {
    record DiffPreview(String path, String oldText, String newText) {}

    private AcpProtocol() {}

    static JsonObject initialize() {
        JsonObject capabilities = new JsonObject();
        JsonObject fs = new JsonObject();
        fs.addProperty("readTextFile", false);
        fs.addProperty("writeTextFile", false);
        capabilities.add("fs", fs);
        capabilities.addProperty("terminal", false);

        JsonObject info = new JsonObject();
        info.addProperty("name", "little-monkey-jetbrains");
        info.addProperty("title", "Little Monkey for JetBrains");
        info.addProperty("version", "0.1.0");

        JsonObject params = new JsonObject();
        params.addProperty("protocolVersion", 1);
        params.add("clientCapabilities", capabilities);
        params.add("clientInfo", info);
        return params;
    }

    static JsonObject newSession(String workspaceRoot) {
        JsonObject params = new JsonObject();
        params.addProperty("cwd", workspaceRoot);
        params.add("mcpServers", new JsonArray());
        return params;
    }

    static JsonObject resumeSession(String sessionId, String workspaceRoot) {
        JsonObject params = newSession(workspaceRoot);
        params.addProperty("sessionId", sessionId);
        return params;
    }

    static boolean supportsResume(JsonElement initializeResponse) {
        if (initializeResponse == null || !initializeResponse.isJsonObject()) return false;
        JsonObject capabilities = object(initializeResponse.getAsJsonObject(), "agentCapabilities");
        JsonObject sessions = object(capabilities, "sessionCapabilities");
        return sessions != null && sessions.has("resume") && sessions.get("resume").isJsonObject();
    }

    static JsonObject prompt(String sessionId, JsonArray blocks) {
        JsonObject params = new JsonObject();
        params.addProperty("sessionId", sessionId);
        params.add("prompt", blocks);
        return params;
    }

    static JsonObject cancel(String sessionId) {
        JsonObject params = new JsonObject();
        params.addProperty("sessionId", sessionId);
        return params;
    }

    static String runId(JsonObject params) {
        return string(params, "runId");
    }

    static String updateText(JsonObject params) {
        JsonObject update = object(params, "update");
        if (update == null) return "";
        JsonObject content = object(update, "content");
        if (content == null) return "";
        return string(content, "text");
    }

    static String updateTitle(JsonObject params) {
        JsonObject update = object(params, "update");
        return update == null ? "" : string(update, "title");
    }

    static String updateStatus(JsonObject params) {
        JsonObject update = object(params, "update");
        return update == null ? "" : string(update, "status");
    }

    static List<DiffPreview> diffs(JsonObject params) {
        JsonObject update = object(params, "update");
        if (update == null) return List.of();
        JsonElement content = update.get("content");
        if (content == null || !content.isJsonArray()) return List.of();
        List<DiffPreview> result = new ArrayList<>();
        for (JsonElement item : content.getAsJsonArray()) {
            if (!item.isJsonObject()) continue;
            JsonObject value = item.getAsJsonObject();
            if (!"diff".equals(string(value, "type"))) continue;
            String path = string(value, "path");
            if (path.isBlank()) continue;
            result.add(new DiffPreview(path, string(value, "oldText"), string(value, "newText")));
        }
        return List.copyOf(result);
    }

    private static JsonObject object(JsonObject parent, String name) {
        JsonElement value = parent == null ? null : parent.get(name);
        return value != null && value.isJsonObject() ? value.getAsJsonObject() : null;
    }

    private static String string(JsonObject parent, String name) {
        JsonElement value = parent == null ? null : parent.get(name);
        return value != null && value.isJsonPrimitive() && value.getAsJsonPrimitive().isString()
            ? value.getAsString() : "";
    }
}
