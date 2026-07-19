package app.littlemonkey.jetbrains;

import com.intellij.openapi.components.PersistentStateComponent;
import com.intellij.openapi.components.Service;
import com.intellij.openapi.components.State;
import com.intellij.openapi.components.Storage;
import com.intellij.openapi.project.Project;
import org.jetbrains.annotations.NotNull;
import org.jetbrains.annotations.Nullable;

@Service(Service.Level.PROJECT)
@State(name = "LittleMonkeySettings", storages = @Storage("little-monkey.xml"))
public final class LittleMonkeySettings implements PersistentStateComponent<LittleMonkeySettings.Data> {
    public static final class Data {
        public String cliPath = "monkey";
        public String ollamaModel = "";
        public String permissionMode = "manual";
        public String acpSessionId = "";
        public String acpSessionWorkspace = "";
        public String acpSessionModel = "";
        public String acpSessionPermissionMode = "";
    }

    private Data data = new Data();

    public static LittleMonkeySettings get(Project project) {
        return project.getService(LittleMonkeySettings.class);
    }

    @Override public @Nullable Data getState() { return data; }
    @Override public void loadState(@NotNull Data state) { data = state; }
    public Data data() { return data; }
}
