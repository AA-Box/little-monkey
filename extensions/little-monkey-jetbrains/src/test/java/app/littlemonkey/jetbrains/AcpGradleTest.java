package app.littlemonkey.jetbrains;

import org.junit.jupiter.api.Test;

/** Runs the dependency-light contract corpus through Gradle's normal test task. */
final class AcpGradleTest {
    @Test
    void acpContractCorpusPasses() throws Exception {
        AcpContractTest.main(new String[0]);
    }
}
