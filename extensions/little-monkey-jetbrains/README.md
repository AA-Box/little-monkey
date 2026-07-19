# Little Monkey for JetBrains

Thin ACP v1 client for IntelliJ IDEA, Android Studio, and compatible JetBrains
IDEs. It captures the active file, selection, exact document modification
stamp, and editor diagnostics, then queues an immutable durable run through
`monkey acp`.

The plugin intentionally has no approval or file-write API. Little Monkey
owns permissions, checkpoints, execution, and cancellation. JetBrains opens
native read-only diff previews for run results and never auto-applies them.

## Use

1. In **Settings > Tools > Little Monkey**, set the `monkey` executable,
   an installed Ollama model tag, and the Little Monkey permission mode.
2. In an editor, open the **Little Monkey** context menu.
3. Choose **Ask About Active Editor** or **Review Problems**.
4. Use **Cancel Active Run** to request durable cancellation, or **Show Attach
   Command** to copy `monkey daemon attach <run-id>`.

## Verify

The production sources target Java 17 and the IntelliJ 2025.1 platform:

```sh
gradle compileJava
bash scripts/run-contract-tests.sh
```

The contract runner only needs JDK 17+ and Gson. On macOS it automatically
uses Android Studio's bundled JBR and Gson; set `JAVA_HOME` and `GSON_JAR` on
other systems.
