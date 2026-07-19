package app.littlemonkey.jetbrains.actions;

import app.littlemonkey.jetbrains.LittleMonkeyProjectService;
import com.intellij.openapi.actionSystem.ActionUpdateThread;
import com.intellij.openapi.actionSystem.AnActionEvent;
import com.intellij.openapi.project.DumbAwareAction;
import org.jetbrains.annotations.NotNull;

/** Shows and copies the terminal attach command for the current durable run. */
public final class OpenRunAction extends DumbAwareAction {
    @Override
    public void actionPerformed(@NotNull AnActionEvent event) {
        var project = event.getProject();
        if (project != null) LittleMonkeyProjectService.get(project).showRun();
    }

    @Override
    public void update(@NotNull AnActionEvent event) {
        var project = event.getProject();
        event.getPresentation().setEnabled(project != null
            && LittleMonkeyProjectService.get(project).currentRunId() != null);
    }

    @Override
    public @NotNull ActionUpdateThread getActionUpdateThread() {
        return ActionUpdateThread.BGT;
    }
}
