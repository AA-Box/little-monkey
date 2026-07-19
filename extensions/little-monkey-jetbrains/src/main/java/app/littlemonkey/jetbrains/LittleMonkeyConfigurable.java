package app.littlemonkey.jetbrains;

import com.intellij.openapi.options.Configurable;
import com.intellij.openapi.options.ConfigurationException;
import com.intellij.openapi.project.Project;
import com.intellij.openapi.ui.ComboBox;
import com.intellij.ui.components.JBLabel;
import com.intellij.ui.components.JBTextField;
import org.jetbrains.annotations.Nls;
import org.jetbrains.annotations.Nullable;

import javax.swing.JComponent;
import javax.swing.JComboBox;
import javax.swing.JPanel;
import java.awt.GridBagConstraints;
import java.awt.GridBagLayout;
import java.awt.Insets;
import java.util.Objects;

/** Project-scoped ACP launch settings. */
public final class LittleMonkeyConfigurable implements Configurable {
    private static final String[] PERMISSION_MODES = {
        "manual", "plan", "acceptEdits", "smart", "auto"
    };

    private final Project project;
    private JPanel panel;
    private JBTextField cliPath;
    private JBTextField model;
    private JComboBox<String> permissionMode;

    public LittleMonkeyConfigurable(Project project) {
        this.project = project;
    }

    @Override
    public @Nls String getDisplayName() {
        return "Little Monkey";
    }

    @Override
    public @Nullable JComponent createComponent() {
        if (panel != null) return panel;
        panel = new JPanel(new GridBagLayout());
        cliPath = new JBTextField();
        model = new JBTextField();
        permissionMode = new ComboBox<>(PERMISSION_MODES);

        GridBagConstraints label = constraints(0, 0, 0.0, GridBagConstraints.NONE);
        GridBagConstraints field = constraints(1, 0, 1.0, GridBagConstraints.HORIZONTAL);
        addRow("CLI executable", cliPath, label, field, 0);
        addRow("Ollama agent model", model, label, field, 1);
        addRow("Permission mode", permissionMode, label, field, 2);

        JBLabel note = new JBLabel(
            "<html>The IDE is a thin ACP client. Approvals, checkpoints, writes, and verification "
                + "stay in Little Monkey. JetBrains only opens read-only diff previews and never auto-applies edits.</html>"
        );
        GridBagConstraints noteConstraints = constraints(0, 3, 1.0, GridBagConstraints.HORIZONTAL);
        noteConstraints.gridwidth = 2;
        noteConstraints.insets = new Insets(14, 0, 0, 0);
        panel.add(note, noteConstraints);

        GridBagConstraints spacer = constraints(0, 4, 1.0, GridBagConstraints.BOTH);
        spacer.gridwidth = 2;
        spacer.weighty = 1.0;
        panel.add(new JPanel(), spacer);
        reset();
        return panel;
    }

    private void addRow(
        String name,
        JComponent component,
        GridBagConstraints labelTemplate,
        GridBagConstraints fieldTemplate,
        int row
    ) {
        GridBagConstraints left = (GridBagConstraints) labelTemplate.clone();
        GridBagConstraints right = (GridBagConstraints) fieldTemplate.clone();
        left.gridy = row;
        right.gridy = row;
        panel.add(new JBLabel(name), left);
        panel.add(component, right);
    }

    private static GridBagConstraints constraints(int x, int y, double weightX, int fill) {
        GridBagConstraints value = new GridBagConstraints();
        value.gridx = x;
        value.gridy = y;
        value.weightx = weightX;
        value.fill = fill;
        value.anchor = GridBagConstraints.NORTHWEST;
        value.insets = new Insets(4, x == 0 ? 0 : 12, 4, 0);
        return value;
    }

    @Override
    public boolean isModified() {
        if (panel == null) return false;
        LittleMonkeySettings.Data current = LittleMonkeySettings.get(project).data();
        return !Objects.equals(cliPath.getText().trim(), current.cliPath)
            || !Objects.equals(model.getText().trim(), current.ollamaModel)
            || !Objects.equals(permissionMode.getSelectedItem(), current.permissionMode);
    }

    @Override
    public void apply() throws ConfigurationException {
        String command = cliPath.getText().trim();
        String selectedModel = model.getText().trim();
        String selectedMode = Objects.toString(permissionMode.getSelectedItem(), "");
        if (command.isEmpty()) throw new ConfigurationException("CLI executable cannot be empty");
        if (selectedModel.isEmpty()) throw new ConfigurationException("Ollama agent model cannot be empty");
        boolean knownMode = false;
        for (String candidate : PERMISSION_MODES) knownMode |= candidate.equals(selectedMode);
        if (!knownMode) throw new ConfigurationException("Unsupported permission mode");

        LittleMonkeySettings.Data current = LittleMonkeySettings.get(project).data();
        current.cliPath = command;
        current.ollamaModel = selectedModel;
        current.permissionMode = selectedMode;
        LittleMonkeyProjectService.get(project).settingsChanged();
    }

    @Override
    public void reset() {
        if (panel == null) return;
        LittleMonkeySettings.Data current = LittleMonkeySettings.get(project).data();
        cliPath.setText(current.cliPath);
        model.setText(current.ollamaModel);
        permissionMode.setSelectedItem(current.permissionMode);
    }

    @Override
    public void disposeUIResources() {
        panel = null;
        cliPath = null;
        model = null;
        permissionMode = null;
    }
}
