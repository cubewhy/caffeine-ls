import * as fs from "fs";
import * as path from "path";
import * as os from "os";
import { execSync } from "child_process";

/**
 * Resolves unique, valid JDK home directories from various system locations.
 */
export async function scanForJdks(): Promise<string[]> {
  const jdkPaths = new Set<string>();

  // 1. Check JAVA_HOME
  if (process.env.JAVA_HOME && isValidJdk(process.env.JAVA_HOME)) {
    jdkPaths.add(path.normalize(process.env.JAVA_HOME));
  }

  // 2. Check ~/.jdks (IntelliJ default download location)
  const homeJdks = path.join(os.homedir(), ".jdks");
  scanDirectoryForJdks(homeJdks, jdkPaths);

  // 3. Platform-specific scanning
  if (process.platform === "darwin") {
    // macOS: Use the official binary wrapper to query installed JDKs
    try {
      const stdout = execSync("/usr/libexec/java_home -V 2>&1").toString();
      // Match paths out of the java_home -V output
      const matches = stdout.match(/\/.+Legacy\/Home|\/.+Contents\/Home/g);
      if (matches) {
        matches.forEach((p) => {
          if (isValidJdk(p)) {
            jdkPaths.add(path.normalize(p));
          }
        });
      }
    } catch {
      // Fallback to common macOS path if binary fails
      scanDirectoryForJdks("/Library/Java/JavaVirtualMachines", jdkPaths);
    }
  } else if (process.platform === "win32") {
    scanDirectoryForJdks("C:\\Program Files\\Java", jdkPaths);
    scanDirectoryForJdks("C:\\Program Files\\Eclipse Adoptium", jdkPaths);
    scanDirectoryForJdks("C:\\Program Files\\JetBrains", jdkPaths);
  } else if (process.platform === "linux") {
    scanDirectoryForJdks("/usr/lib/jvm", jdkPaths);
    scanDirectoryForJdks("/usr/java", jdkPaths);
  }

  // 4. Parse PATH environment variable (Resolving symlinks safely)
  const pathSeparator = process.platform === "win32" ? ";" : ":";
  const paths = (process.env.PATH || "").split(pathSeparator);
  const javaBinName = process.platform === "win32" ? "java.exe" : "java";

  for (const dir of paths) {
    const maybeJavaBin = path.join(dir, javaBinName);
    if (fs.existsSync(maybeJavaBin)) {
      try {
        // Realpath resolves symlinks like /etc/alternatives/java on Linux
        const realPath = fs.realpathSync(maybeJavaBin);
        // If it's a macOS generic wrapper wrapper, skip it
        if (
          process.platform === "darwin" &&
          realPath.includes("/usr/bin/java")
        ) {
          continue;
        }
        const possibleHome = path.dirname(path.dirname(realPath));
        if (isValidJdk(possibleHome)) {
          jdkPaths.add(path.normalize(possibleHome));
        }
      } catch {
        // Ignore resolution errors
      }
    }
  }

  return Array.from(jdkPaths);
}

/**
 * Validates if a path is structurally a JDK home directory.
 */
function isValidJdk(dir: string): boolean {
  if (!dir || !fs.existsSync(dir)) {
    return false;
  }
  const javaBin = process.platform === "win32" ? "bin\\java.exe" : "bin/java";
  return fs.existsSync(path.join(dir, javaBin));
}

/**
 * Helper to non-recursively check subdirectories for valid JDKs.
 */
function scanDirectoryForJdks(baseDir: string, set: Set<string>) {
  if (!fs.existsSync(baseDir)) {
    return;
  }
  try {
    const elements = fs.readdirSync(baseDir);
    for (const el of elements) {
      const fullPath = path.join(baseDir, el);
      if (isValidJdk(fullPath)) {
        set.add(path.normalize(fullPath));
      } else {
        // Handle macOS deep structure (Contents/Home)
        const macHome = path.join(fullPath, "Contents", "Home");
        if (isValidJdk(macHome)) {
          set.add(path.normalize(macHome));
        }
      }
    }
  } catch {
    // Ignore unreadable directories
  }
}
