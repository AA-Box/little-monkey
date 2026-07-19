package app.littlemonkey.jetbrains.actions;

import app.littlemonkey.jetbrains.LittleMonkeyProjectService;
import com.intellij.openapi.actionSystem.ActionUpdateThread;
import com.intellij.openapi.actionSystem.AnActionEvent;
import com.intellij.openapi.project.DumbAwareAction;
import org.jetbrains.annotations.NotNull;

/** Requests durable-run cancellation; disconnecting the IDE never implies cancellation. */
public final class CancelAction extends DumbAwareAction {
    @Override
    public void actionPerformed(@NotNull AnActionEvent event) {
        var project = event.getProject();
        if (project != null) LittleMonkeyProjectService.get(project).cancelActiveRun();
    }

    @Override
    public void update(@NotNull AnActionEvent event) {
        var project = event.getProject();
        event.getPresentation().setEnabled(project != null
            && LittleMonkeyProjectService.get(project).hasActiveRun());
    }

    @Override
    public @NotNull ActionUpdateThread getActionUpdateThread() {
        return ActionUpdateThread.BGT;
    }
}
