package app.littlemonkey.jetbrains;

import java.io.IOException;
import java.nio.file.Path;
import java.util.List;
import java.util.Set;

/** Validated, immutable ACP process configuration. */
record LittleMonkeyLaunchConfig(String cliPath, String model, String permissionMode, Path workspace) {
    private static final Set<String> PERMISSION_MODES = Set.of(
        "manual", "plan", "acceptEdits", "smart", "auto"
    );

    static LittleMonkeyLaunchConfig create(
        String cliPath,
        String model,
        String permissionMode,
        String workspace
    ) throws IOException {
        String command = cliPath == null ? "" : cliPath.trim();
        String tag = model == null ? "" : model.trim();
        String mode = permissionMode == null ? "" : permissionMode.trim();
        if (command.isEmpty()) throw new IllegalArgumentException("Set the Little Monkey CLI path first");
        if (tag.isEmpty()) throw new IllegalArgumentException("Set an installed Ollama agent model first");
        if (!PERMISSION_MODES.contains(mode)) {
            throw new IllegalArgumentException("Unsupported permission mode: " + mode);
        }
        if (workspace == null || workspace.isBlank()) {
            throw new IllegalArgumentException("Open a project with a local workspace first");
        }
        Path root = Path.of(workspace).toRealPath();
        if (!root.toFile().isDirectory()) throw new IllegalArgumentException("Workspace is not a directory");
        return new LittleMonkeyLaunchConfig(command, tag, mode, root);
    }

    List<String> command() {
        return List.of(
            cliPath,
            "--workspace", workspace.toString(),
            "--ollama", model,
            "--permission-mode", permissionMode,
            "acp"
        );
    }
}
