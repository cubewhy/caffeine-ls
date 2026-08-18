plugins {
    java
}

group = "org.cubewhy.caffeine_ls"
version = "0.1.0"

repositories {
    mavenCentral()
}

dependencies {
    compileOnly("org.apache.maven:maven-plugin-api:3.9.9")
    compileOnly("org.apache.maven:maven-core:3.9.9")
    compileOnly("org.apache.maven.plugin-tools:maven-plugin-annotations:3.13.1")
}

java {
    sourceCompatibility = JavaVersion.VERSION_1_8
    targetCompatibility = JavaVersion.VERSION_1_8
}

tasks.withType<JavaCompile>().configureEach {
    options.release.set(8)
}

tasks.named<Jar>("jar") {
    archiveFileName.set("caffeine-ls-maven-sidecar.jar")
}
