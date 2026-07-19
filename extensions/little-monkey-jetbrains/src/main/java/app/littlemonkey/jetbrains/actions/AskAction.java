package app.littlemonkey.jetbrains.actions;

import app.littlemonkey.jetbrains.LittleMonkeyProjectService;
import com.intellij.openapi.actionSystem.ActionUpdateThread;
import com.intellij.openapi.actionSystem.AnActionEvent;
import com.intellij.openapi.actionSystem.CommonDataKeys;
import com.intellij.openapi.project.DumbAwareAction;
import com.intellij.openapi.ui.Messages;
import org.jetbrains.annotations.NotNull;

/** Sends active editor context through ACP after an explicit user command. */
public final class AskAction extends DumbAwareAction {
    @Override
    public void actionPerformed(@NotNull AnActionEvent event) {
        var project = event.getProject();
        var editor = event.getData(CommonDataKeys.EDITOR);
        if (project == null || editor == null) return;
        String instruction = Messages.showInputDialog(
            project,
            "Ask Little Monkey about the active file and selection",
            "Ask Little Monkey",
            null
        );
        if (instruction == null || instruction.isBlank()) return;
        LittleMonkeyProjectService.get(project).sendPrompt(editor, instruction);
    }

    @Override
    public void update(@NotNull AnActionEvent event) {
        event.getPresentation().setEnabledAndVisible(
            event.getProject() != null && event.getData(CommonDataKeys.EDITOR) != null
        );
    }

    @Override
    public @NotNull ActionUpdateThread getActionUpdateThread() {
        return ActionUpdateThread.EDT;
    }
}
