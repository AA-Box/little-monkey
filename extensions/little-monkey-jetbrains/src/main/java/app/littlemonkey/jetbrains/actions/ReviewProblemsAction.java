package app.littlemonkey.jetbrains.actions;

import app.littlemonkey.jetbrains.LittleMonkeyProjectService;
import com.intellij.openapi.actionSystem.ActionUpdateThread;
import com.intellij.openapi.actionSystem.AnActionEvent;
import com.intellij.openapi.actionSystem.CommonDataKeys;
import com.intellij.openapi.project.DumbAwareAction;
import org.jetbrains.annotations.NotNull;

/** Captures the current editor's exact-version diagnostics for an ACP review. */
public final class ReviewProblemsAction extends DumbAwareAction {
    private static final String INSTRUCTION =
        "Review the captured Problems diagnostics for this exact document version. "
            + "Explain fixes and only edit after the normal Little Monkey approval policy allows it.";

    @Override
    public void actionPerformed(@NotNull AnActionEvent event) {
        var project = event.getProject();
        var editor = event.getData(CommonDataKeys.EDITOR);
        if (project == null || editor == null) return;
        LittleMonkeyProjectService.get(project).sendPrompt(editor, INSTRUCTION);
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
