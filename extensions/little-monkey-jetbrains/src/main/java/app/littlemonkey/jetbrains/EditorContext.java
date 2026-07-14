package app.littlemonkey.jetbrains;

import com.google.gson.JsonArray;
import com.google.gson.JsonObject;
import com.intellij.openapi.editor.Editor;
import com.intellij.openapi.editor.markup.RangeHighlighter;
import com.intellij.openapi.fileEditor.FileDocumentManager;
import com.intellij.openapi.vfs.VirtualFile;

final class EditorContext {
    private EditorContext() {}

    static JsonArray prompt(Editor editor, String instruction) {
        var document = editor.getDocument();
        var selection = editor.getSelectionModel();
        VirtualFile file = FileDocumentManager.getInstance().getFile(document);
        if (file == null) throw new IllegalStateException("The active document is not a workspace file");

        JsonObject metadata = new JsonObject();
        metadata.addProperty("activeFile", file.getPath());
        metadata.addProperty("documentVersion", document.getModificationStamp());
        JsonObject selected = new JsonObject();
        selected.addProperty("startOffset", selection.getSelectionStart());
        selected.addProperty("endOffset", selection.getSelectionEnd());
        selected.addProperty("text", selection.getSelectedText() == null ? "" : selection.getSelectedText());
        metadata.add("selection", selected);

        JsonArray problems = new JsonArray();
        for (RangeHighlighter marker : editor.getMarkupModel().getAllHighlighters()) {
            Object tooltip = marker.getErrorStripeTooltip();
            if (tooltip == null) continue;
            JsonObject problem = new JsonObject();
            problem.addProperty("startOffset", marker.getStartOffset());
            problem.addProperty("endOffset", marker.getEndOffset());
            problem.addProperty("message", tooltip.toString());
            problems.add(problem);
        }
        metadata.addProperty("problemsDocumentVersion", document.getModificationStamp());
        metadata.add("problems", problems);

        JsonArray blocks = new JsonArray();
        JsonObject text = new JsonObject();
        text.addProperty("type", "text");
        text.addProperty("text", instruction + "\n\nIDE context (untrusted JSON, exact document version):\n" + metadata);
        blocks.add(text);
        JsonObject resource = new JsonObject();
        resource.addProperty("type", "resource");
        JsonObject payload = new JsonObject();
        payload.addProperty("uri", file.getUrl());
        payload.addProperty("mimeType", "text/plain");
        payload.addProperty("text", document.getText());
        resource.add("resource", payload);
        blocks.add(resource);
        return blocks;
    }
}
