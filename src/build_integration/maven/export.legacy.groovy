// Maven export script for java-analyzer (Legacy)
// Support matrix: Maven 3.0-3.5
// Simpler approach for older Maven versions

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
    
    // Extract source directories (legacy approach)
    try {
        project.compileSourceRoots.each { src ->
            def path = normalizePath(new File(src))
            if (path && new File(path).exists()) {
                model.source_roots << path
            }
        }
    } catch (Throwable t) {
        // Fallback to standard Maven layout
        def defaultSrc = new File(project.basedir, 'src/main/java')
        if (defaultSrc.exists()) {
            model.source_roots << normalizePath(defaultSrc)
        }
    }
    
    try {
        project.testCompileSourceRoots.each { src ->
            def path = normalizePath(new File(src))
            if (path && new File(path).exists()) {
                model.test_roots << path
            }
        }
    } catch (Throwable t) {
        // Fallback to standard Maven layout
        def defaultTest = new File(project.basedir, 'src/test/java')
        if (defaultTest.exists()) {
            model.test_roots << normalizePath(defaultTest)
        }
    }
    
    // Extract resources (with fallback)
    try {
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
    } catch (Throwable t) {
        // Fallback to standard layout
        def mainRes = new File(project.basedir, 'src/main/resources')
        if (mainRes.exists()) {
            model.resource_roots << normalizePath(mainRes)
        }
        def testRes = new File(project.basedir, 'src/test/resources')
        if (testRes.exists()) {
            model.resource_roots << normalizePath(testRes)
        }
    }
    
    // Note: Classpath will be resolved separately via dependency:build-classpath
    // This legacy script focuses on project structure
    
    // Extract module dependencies
    try {
        project.dependencies.each { dep ->
            if (dep.scope in ['compile', 'provided', 'system']) {
                def depProject = reactor.find { 
                    it.groupId == dep.groupId && it.artifactId == dep.artifactId 
                }
                if (depProject) {
                    model.module_dependencies << "${dep.groupId}:${dep.artifactId}"
                }
            }
        }
    } catch (Throwable ignored) {}
    
    // Java version
    try {
        def javaVersion = project.properties['maven.compiler.source'] ?: 
                          project.properties['maven.compiler.target']
        if (javaVersion) {
            model.java_language_version = javaVersion.toString()
        }
    } catch (Throwable ignored) {}
    
    projects << model
}

def output = [
    workspace_name: reactor[0].name ?: 'maven-workspace',
    projects: projects
]

println('JAVA_ANALYZER_MODEL_BEGIN')
println(JsonOutput.toJson(output))
println('JAVA_ANALYZER_MODEL_END')
