// Maven export script for java-analyzer
// Support matrix: Maven 3.6+
// This script exports workspace metadata as JSON for the LSP to consume

import groovy.json.JsonOutput

boolean isDebugEnabled() {
    return System.getProperty('JAVA_ANALYZER_MAVEN_DEBUG') == '1' || System.getenv('JAVA_ANALYZER_MAVEN_DEBUG') == '1'
}

void debugLog(String message) {
    if (isDebugEnabled()) {
        System.err.println("JAVA_ANALYZER_MAVEN_DEBUG: ${message}")
    }
}

String normalizePath(def file) {
    if (file == null) return null
    try {
        return file.absoluteFile.path.replace(File.separatorChar, '/' as char)
    } catch (Throwable ignored) {
        return null
    }
}

def projects = []
def reactor = session.projects

reactor.each { project ->
    debugLog(">>> Analyzing project: ${project.groupId}:${project.artifactId}")
    
    def model = [
        path: "${project.groupId}:${project.artifactId}",
        name: project.artifactId,
        project_dir: normalizePath(project.basedir),
        source_roots: [],
        test_roots: [],
        resource_roots: [],
        generated_roots: [],
        compile_classpath: [],
        test_classpath: [],
        module_dependencies: [],
        java_language_version: null
    ]
    
    // Extract source directories
    project.compileSourceRoots.each { src ->
        def path = normalizePath(new File(src))
        if (path && new File(path).exists()) {
            model.source_roots << path
            debugLog("    [SOURCE] ${path}")
        }
    }
    
    project.testCompileSourceRoots.each { src ->
        def path = normalizePath(new File(src))
        if (path && new File(path).exists()) {
            model.test_roots << path
            debugLog("    [TEST] ${path}")
        }
    }
    
    // Extract resources
    project.build.resources.each { res ->
        def path = normalizePath(new File(res.directory))
        if (path && new File(path).exists()) {
            model.resource_roots << path
        }
    }
    
    project.build.testResources.each { res ->
        def path = normalizePath(new File(res.directory))
        if (path && new File(path).exists()) {
            model.resource_roots << path
        }
    }
    
    // Extract generated sources
    def generatedSourcesDir = new File(project.build.directory, 'generated-sources')
    if (generatedSourcesDir.exists()) {
        generatedSourcesDir.eachDir { dir ->
            def path = normalizePath(dir)
            if (path) {
                model.generated_roots << path
                debugLog("    [GENERATED] ${path}")
            }
        }
    }
    
    // Extract compile classpath
    try {
        project.compileClasspathElements.each { cp ->
            def path = normalizePath(new File(cp))
            if (path && new File(path).exists()) {
                model.compile_classpath << path
            }
        }
        debugLog("    [COMPILE_CP] ${model.compile_classpath.size()} entries")
    } catch (Throwable t) {
        debugLog("    [ERROR] Failed to get compile classpath: ${t.message}")
    }
    
    // Extract test classpath
    try {
        project.testClasspathElements.each { cp ->
            def path = normalizePath(new File(cp))
            if (path && new File(path).exists()) {
                model.test_classpath << path
            }
        }
        debugLog("    [TEST_CP] ${model.test_classpath.size()} entries")
    } catch (Throwable t) {
        debugLog("    [ERROR] Failed to get test classpath: ${t.message}")
    }
    
    // Extract module dependencies (inter-project dependencies)
    project.dependencies.each { dep ->
        if (dep.scope in ['compile', 'provided', 'system']) {
            def depProject = reactor.find { 
                it.groupId == dep.groupId && it.artifactId == dep.artifactId 
            }
            if (depProject) {
                def depPath = "${dep.groupId}:${dep.artifactId}"
                model.module_dependencies << depPath
                debugLog("    [MODULE_DEP] ${depPath}")
            }
        }
    }
    
    // Java version
    def javaVersion = project.properties['maven.compiler.source'] ?: 
                      project.properties['maven.compiler.release'] ?:
                      project.properties['maven.compiler.target']
    if (javaVersion) {
        model.java_language_version = javaVersion.toString()
        debugLog("    [JAVA_VERSION] ${model.java_language_version}")
    }
    
    projects << model
}

def output = [
    workspace_name: reactor[0].name ?: 'maven-workspace',
    projects: projects
]

println('JAVA_ANALYZER_MODEL_BEGIN')
println(JsonOutput.toJson(output))
println('JAVA_ANALYZER_MODEL_END')
