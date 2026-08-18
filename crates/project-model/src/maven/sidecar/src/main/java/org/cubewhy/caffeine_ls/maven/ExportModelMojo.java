/*
 * Exports the Maven workspace model consumed by Caffeine LS.
 *
 * This mojo is the compiled replacement for the old Groovy export script. It
 * runs inside the user's reactor (via `mvn test-compile <coords>:export-model`)
 * and prints a single JSON model delimited by WORKSPACE_MODEL_BEGIN/END markers
 * on stdout, exactly like the script did, so the Rust side can parse it
 * unchanged.
 */
package org.cubewhy.caffeine_ls.maven;

import org.apache.maven.artifact.Artifact;
import org.apache.maven.execution.MavenSession;
import org.apache.maven.model.Resource;
import org.apache.maven.plugin.AbstractMojo;
import org.apache.maven.plugins.annotations.LifecyclePhase;
import org.apache.maven.plugins.annotations.Mojo;
import org.apache.maven.plugins.annotations.Parameter;
import org.apache.maven.plugins.annotations.ResolutionScope;
import org.apache.maven.project.MavenProject;

import java.io.File;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;

@Mojo(
        name = "export-model",
        defaultPhase = LifecyclePhase.NONE,
        requiresDependencyResolution = ResolutionScope.TEST,
        threadSafe = true)
public class ExportModelMojo extends AbstractMojo {

    private static final String MODEL_BEGIN = "WORKSPACE_MODEL_BEGIN";
    private static final String MODEL_END = "WORKSPACE_MODEL_END";

    @Parameter(defaultValue = "${session}", readonly = true, required = true)
    private MavenSession session;

    @Parameter(defaultValue = "${project}", readonly = true, required = true)
    private MavenProject project;

    @Override
    public void execute() {
        List<MavenProject> reactorProjects = session.getProjects();
        if (reactorProjects == null || reactorProjects.isEmpty()) {
            return;
        }

        // Guarantee execution occurs exactly once, at the end of the full
        // reactor pipeline (mirrors the original Groovy export script).
        if (project != reactorProjects.get(reactorProjects.size() - 1)) {
            return;
        }

        Map<String, String> targetDirToProjectKey = new LinkedHashMap<>();
        for (MavenProject proj : reactorProjects) {
            String key = proj.getGroupId() + ":" + proj.getArtifactId();
            String outDir = proj.getBuild().getOutputDirectory();
            if (outDir != null) {
                String normalized = normalizePath(outDir);
                if (normalized != null) {
                    targetDirToProjectKey.put(normalized, key);
                }
            }
            String testOutDir = proj.getBuild().getTestOutputDirectory();
            if (testOutDir != null) {
                String normalized = normalizePath(testOutDir);
                if (normalized != null) {
                    targetDirToProjectKey.put(normalized, key);
                }
            }
        }

        List<Object> modelProjects = new ArrayList<>();

        for (MavenProject proj : reactorProjects) {
            String projKey = proj.getGroupId() + ":" + proj.getArtifactId();

            Map<String, String> jarOriginMap = new LinkedHashMap<>();
            collectJarOrigins(proj, jarOriginMap);

            List<String> sourceRoots = new ArrayList<>();
            List<String> testRoots = new ArrayList<>();
            List<String> generatedRoots = new ArrayList<>();

            for (String src : safeStrings(proj.getCompileSourceRoots())) {
                String normalized = normalizePath(src);
                if (normalized == null) {
                    continue;
                }
                if (normalized.contains("/generated-sources") || normalized.contains("target/generated")) {
                    generatedRoots.add(normalized);
                } else {
                    sourceRoots.add(normalized);
                }
            }

            for (String src : safeStrings(proj.getTestCompileSourceRoots())) {
                String normalized = normalizePath(src);
                if (normalized == null) {
                    continue;
                }
                if (normalized.contains("/generated-test-sources") || normalized.contains("target/generated")) {
                    generatedRoots.add(normalized);
                } else {
                    testRoots.add(normalized);
                }
            }

            Set<String> resourceRoots = new LinkedHashSet<>();
            for (Resource res : safeResources(proj.getResources())) {
                String normalized = normalizePath(res.getDirectory());
                if (normalized != null) {
                    resourceRoots.add(normalized);
                }
            }
            for (Resource res : safeResources(proj.getTestResources())) {
                String normalized = normalizePath(res.getDirectory());
                if (normalized != null) {
                    resourceRoots.add(normalized);
                }
            }

            List<Object> compileClasspathEntries = mapClasspath(
                    safeStrings(classpathElements(proj, true)), targetDirToProjectKey, jarOriginMap);
            List<Object> testClasspathEntries = mapClasspath(
                    safeStrings(classpathElements(proj, false)), targetDirToProjectKey, jarOriginMap);

            String javaLangVersion = proj.getProperties().getProperty("maven.compiler.source");
            if (javaLangVersion == null) {
                javaLangVersion = proj.getProperties().getProperty("java.version");
            }
            if (javaLangVersion == null) {
                javaLangVersion = System.getProperty("java.version");
            }
            if (javaLangVersion != null && javaLangVersion.startsWith("1.")) {
                javaLangVersion = javaLangVersion.substring(2);
            }

            String javaHome = normalizePath(new File(System.getProperty("java.home")));

            Map<String, Object> modelProject = new LinkedHashMap<>();
            modelProject.put("path", projKey);
            modelProject.put("name", proj.getArtifactId());
            modelProject.put("project_dir", normalizePath(proj.getBasedir()));
            modelProject.put("source_roots", new ArrayList<>(new LinkedHashSet<>(sourceRoots)));
            modelProject.put("test_roots", new ArrayList<>(new LinkedHashSet<>(testRoots)));
            modelProject.put("resource_roots", new ArrayList<>(resourceRoots));
            modelProject.put("generated_roots", new ArrayList<>(new LinkedHashSet<>(generatedRoots)));
            modelProject.put("compile_classpath", compileClasspathEntries);
            modelProject.put("test_classpath", testClasspathEntries);
            modelProject.put("java_language_version", javaLangVersion);
            modelProject.put("java_home", javaHome);
            modelProjects.add(modelProject);
        }

        String workspaceName = reactorProjects.get(0).getArtifactId();

        Map<String, Object> model = new LinkedHashMap<>();
        model.put("workspace_name", workspaceName);
        model.put("projects", modelProjects);

        System.out.println(MODEL_BEGIN);
        System.out.println(Json.write(model));
        System.out.println(MODEL_END);
    }

    private void collectJarOrigins(MavenProject proj, Map<String, String> jarOriginMap) {
        Set<Artifact> artifacts = new LinkedHashSet<>();
        artifacts.addAll(safeArtifacts(proj.getArtifacts()));
        artifacts.addAll(safeArtifacts(proj.getDependencyArtifacts()));

        for (Artifact art : artifacts) {
            File file = art.getFile();
            if (file == null) {
                continue;
            }
            String normalized = normalizePath(file);
            if (normalized != null) {
                String origin = "system".equalsIgnoreCase(art.getScope()) ? "flat-file" : "coordinate";
                jarOriginMap.put(normalized, origin);
            }
        }
    }

    private List<Object> mapClasspath(
            List<String> elements,
            Map<String, String> targetDirToProjectKey,
            Map<String, String> jarOriginMap) {
        List<Object> entries = new ArrayList<>();
        for (String elem : elements) {
            String normalized = normalizePath(elem);
            if (normalized == null) {
                continue;
            }

            if (targetDirToProjectKey.containsKey(normalized)) {
                Map<String, Object> entry = new LinkedHashMap<>();
                entry.put("type", "project");
                entry.put("path", targetDirToProjectKey.get(normalized));
                entry.put("source_set", "main");
                addUnique(entries, entry);
            } else if (normalized.endsWith(".jar")) {
                String origin = jarOriginMap.getOrDefault(normalized, "flat-file");
                Map<String, Object> entry = new LinkedHashMap<>();
                entry.put("type", "jar");
                entry.put("path", normalized);
                entry.put("origin", origin);
                addUnique(entries, entry);
            }
        }
        return entries;
    }

    private static void addUnique(List<Object> entries, Map<String, Object> entry) {
        if (!entries.contains(entry)) {
            entries.add(entry);
        }
    }

    private static List<String> classpathElements(MavenProject proj, boolean compile) {
        try {
            if (compile) {
                return new ArrayList<>(proj.getCompileClasspathElements());
            }
            return new ArrayList<>(proj.getTestClasspathElements());
        } catch (Exception ignored) {
            return new ArrayList<>();
        }
    }

    private static List<String> safeStrings(List<String> values) {
        return values == null ? new ArrayList<>() : values;
    }

    private static List<Resource> safeResources(List<Resource> values) {
        return values == null ? new ArrayList<>() : values;
    }

    private static Set<Artifact> safeArtifacts(Set<Artifact> values) {
        return values == null ? new LinkedHashSet<>() : values;
    }

    private static String normalizePath(String path) {
        if (path == null) {
            return null;
        }
        return normalizePath(new File(path));
    }

    private static String normalizePath(File file) {
        if (file == null) {
            return null;
        }
        try {
            return file.getAbsoluteFile().getPath().replace(File.separatorChar, '/');
        } catch (Throwable ignored) {
            return null;
        }
    }

    /** Minimal JSON writer so the sidecar ships with zero runtime dependencies. */
    static final class Json {
        static String write(Object value) {
            StringBuilder out = new StringBuilder();
            writeValue(out, value);
            return out.toString();
        }

        @SuppressWarnings("unchecked")
        private static void writeValue(StringBuilder out, Object value) {
            if (value == null) {
                out.append("null");
            } else if (value instanceof String) {
                writeString(out, (String) value);
            } else if (value instanceof Boolean) {
                out.append(value.toString());
            } else if (value instanceof Number) {
                out.append(value.toString());
            } else if (value instanceof Map) {
                writeMap(out, (Map<String, Object>) value);
            } else if (value instanceof List) {
                writeList(out, (List<Object>) value);
            } else {
                writeString(out, value.toString());
            }
        }

        private static void writeMap(StringBuilder out, Map<String, Object> map) {
            out.append('{');
            boolean first = true;
            for (Map.Entry<String, Object> entry : map.entrySet()) {
                if (!first) {
                    out.append(',');
                }
                first = false;
                writeString(out, entry.getKey());
                out.append(':');
                writeValue(out, entry.getValue());
            }
            out.append('}');
        }

        private static void writeList(StringBuilder out, List<Object> list) {
            out.append('[');
            boolean first = true;
            for (Object item : list) {
                if (!first) {
                    out.append(',');
                }
                first = false;
                writeValue(out, item);
            }
            out.append(']');
        }

        private static void writeString(StringBuilder out, String value) {
            out.append('"');
            for (int i = 0; i < value.length(); i++) {
                char c = value.charAt(i);
                switch (c) {
                    case '"':
                        out.append("\\\"");
                        break;
                    case '\\':
                        out.append("\\\\");
                        break;
                    case '\b':
                        out.append("\\b");
                        break;
                    case '\f':
                        out.append("\\f");
                        break;
                    case '\n':
                        out.append("\\n");
                        break;
                    case '\r':
                        out.append("\\r");
                        break;
                    case '\t':
                        out.append("\\t");
                        break;
                    default:
                        if (c < 0x20) {
                            out.append(String.format("\\u%04x", (int) c));
                        } else {
                            out.append(c);
                        }
                        break;
                }
            }
            out.append('"');
        }
    }
}
