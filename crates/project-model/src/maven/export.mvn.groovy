// Supported Range: Maven 3.x - 4.x (via GMavenPlus Plugin execution context)
import groovy.json.JsonOutput
import java.io.File

String normalizePath(def file) {
    if (file == null) return null
    try {
        if (file instanceof String) {
            return new File(file).getAbsoluteFile().getPath().replace(File.separatorChar, '/' as char)
        }
        return file.getAbsoluteFile().getPath().replace(File.separatorChar, '/' as char)
    } catch (Throwable ignored) {
        return null
    }
}

def reactorProjects = session.getProjects()
def targetDirToProjectKey = [:]

// Build a look-up map matching build targets back to reactor coordinate keys
reactorProjects.each { proj ->
    String key = "${proj.getGroupId()}:${proj.getArtifactId()}"
    def outDir = proj.getBuild()?.getOutputDirectory()
    if (outDir) {
        targetDirToProjectKey[normalizePath(outDir)] = key
    }
    def testOutDir = proj.getBuild()?.getTestOutputDirectory()
    if (testOutDir) {
        targetDirToProjectKey[normalizePath(testOutDir)] = key
    }
}

// Guarantee execution occurs exactly once at the end of the full reactor pipeline
def currentProject = project
if (currentProject != reactorProjects[-1]) {
    return
}

def modelProjects = []

reactorProjects.each { proj ->
    String projKey = "${proj.getGroupId()}:${proj.getArtifactId()}"
    
    // Categorize regular source directories vs compiler-generated source roots
    def sourceRoots = []
    def testRoots = []
    def generatedRoots = []
    
    proj.getCompileSourceRoots().each { src ->
        String norm = normalizePath(src)
        if (norm) {
            if (norm.contains("/generated-sources") || norm.contains("target/generated")) {
                generatedRoots << norm
            } else {
                sourceRoots << norm
            }
        }
    }
    
    proj.getTestCompileSourceRoots().each { src ->
        String norm = normalizePath(src)
        if (norm) {
            if (norm.contains("/generated-test-sources") || norm.contains("target/generated")) {
                generatedRoots << norm
            } else {
                testRoots << norm
            }
        }
    }
    
    // Extract asset directories
    def resourceRoots = []
    proj.getResources().each { res ->
        String norm = normalizePath(res.getDirectory())
        if (norm) resourceRoots << norm
    }
    proj.getTestResources().each { res ->
        String norm = normalizePath(res.getDirectory())
        if (norm) resourceRoots << norm
    }
    resourceRoots = resourceRoots.unique()

    // Map the resolved Compile Classpath Elements
    def compileClasspathEntries = []
    try {
        proj.getCompileClasspathElements().each { elem ->
            String norm = normalizePath(elem)
            if (!norm) return
            
            if (targetDirToProjectKey.containsKey(norm)) {
                compileClasspathEntries << [
                    type: 'project',
                    path: targetDirToProjectKey[norm],
                    source_set: 'main'
                ]
            } else if (norm.endsWith('.jar')) {
                compileClasspathEntries << [ type: 'jar', path: norm ]
            }
        }
    } catch (Throwable ignored) {}

    // Map the resolved Test Classpath Elements
    def testClasspathEntries = []
    try {
        proj.getTestClasspathElements().each { elem ->
            String norm = normalizePath(elem)
            if (!norm) return
            
            if (targetDirToProjectKey.containsKey(norm)) {
                testClasspathEntries << [
                    type: 'project',
                    path: targetDirToProjectKey[norm],
                    source_set: 'main'
                ]
            } else if (norm.endsWith('.jar')) {
                testClasspathEntries << [ type: 'jar', path: norm ]
            }
        }
    } catch (Throwable ignored) {}

    // Resolve target compilation language level
    def javaLangVersion = proj.getProperties().getProperty('maven.compiler.source') ?: 
                          proj.getProperties().getProperty('java.version') ?: 
                          System.getProperty('java.version')
    if (javaLangVersion && javaLangVersion.startsWith("1.")) {
        javaLangVersion = javaLangVersion.substring(2) // Normalize legacy "1.8" -> "8"
    }

    def javaHome = normalizePath(new File(System.getProperty('java.home')))

    modelProjects << [
        path: projKey,
        name: proj.getArtifactId(),
        project_dir: normalizePath(proj.getBasedir()),
        source_roots: sourceRoots.unique(),
        test_roots: testRoots.unique(),
        resource_roots: resourceRoots,
        generated_roots: generatedRoots.unique(),
        compile_classpath: compileClasspathEntries.unique(),
        test_classpath: testClasspathEntries.unique(),
        java_language_version: javaLangVersion,
        java_home: javaHome
    ]
}

def model = [
    workspace_name: session.getTopLevelProject().getArtifactId(),
    projects: modelProjects
]

println('WORKSPACE_MODEL_BEGIN')
println(JsonOutput.toJson(model))
println('WORKSPACE_MODEL_END')
