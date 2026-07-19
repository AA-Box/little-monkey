plugins {
    java
    id("org.jetbrains.intellij.platform") version "2.18.1"
}

group = "app.littlemonkey"
version = "0.1.0"

repositories {
    mavenCentral()
    intellijPlatform { defaultRepositories() }
}

dependencies {
    intellijPlatform { intellijIdea("2025.1.7") }
    implementation("com.google.code.gson:gson:2.11.0")
    testRuntimeOnly("junit:junit:4.13.2")
    testImplementation("org.junit.jupiter:junit-jupiter:5.12.2")
    testRuntimeOnly("org.junit.platform:junit-platform-launcher:1.12.2")
}

java {
    toolchain { languageVersion.set(JavaLanguageVersion.of(17)) }
}

tasks.test { useJUnitPlatform() }

tasks.register<JavaExec>("contractTest") {
    group = "verification"
    description = "Runs the dependency-light ACP v1 contract suite"
    dependsOn(tasks.testClasses)
    classpath = sourceSets.test.get().runtimeClasspath
    mainClass.set("app.littlemonkey.jetbrains.AcpContractTest")
}

intellijPlatform {
    pluginConfiguration {
        id = "app.littlemonkey.jetbrains"
        name = "Little Monkey"
        version = project.version.toString()
        ideaVersion {
            sinceBuild = "251"
            untilBuild = provider { null }
        }
        vendor {
            name = "Little Monkey"
            url = "https://github.com/sarollahi"
        }
    }
}
