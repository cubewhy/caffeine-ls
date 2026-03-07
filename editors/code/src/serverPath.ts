import * as path from "path";
import * as vscode from "vscode";

import { type ExtensionSettings } from "./config";
import {
  type ExecutableOptions,
  type ServerOptions,
} from "vscode-languageclient/node";

function getBundledServerPath(context: vscode.ExtensionContext): string {
  const extName = process.platform === "win32" ? ".exe" : "";
  return vscode.Uri.joinPath(
    context.extensionUri,
    "bin",
    `server${extName}`,
  ).fsPath;
}

function getDevServerCwd(context: vscode.ExtensionContext): string {
  return path.resolve(context.extensionPath, "..", "..");
}

function resolveJavaCommand(jdkPath: string): string {
  const trimmed = jdkPath.trim();
  if (!trimmed) {
    return "java";
  }

  const normalizedPath = trimmed.toLowerCase();
  if (normalizedPath.endsWith(".exe") || path.basename(normalizedPath) === "java") {
    return trimmed;
  }

  return path.join(trimmed, "bin", process.platform === "win32" ? "java.exe" : "java");
}

function getJavaExecutableOptions(javaRuntime: string): ExecutableOptions | undefined {
  if (!javaRuntime.trim()) {
    return undefined;
  }

  const env = { ...process.env };
  const runtimePath = javaRuntime.trim();
  const runtimeBaseName = path.basename(runtimePath).toLowerCase();
  if (
    runtimeBaseName === "java"
    || runtimeBaseName === "java.exe"
    || runtimePath.toLowerCase().endsWith(path.join("bin", "java").toLowerCase())
    || runtimePath.toLowerCase().endsWith(path.join("bin", "java.exe").toLowerCase())
  ) {
    env.JAVA_HOME = path.dirname(path.dirname(runtimePath));
  } else {
    env.JAVA_HOME = runtimePath;
  }

  return { env };
}

export function resolveServerOptions(
  context: vscode.ExtensionContext,
  settings: ExtensionSettings,
): ServerOptions {
  if (settings.serverPath) {
    const runtimeOptions = getJavaExecutableOptions(settings.jdkPath);
    if (settings.serverPath.toLowerCase().endsWith(".jar")) {
      const javaCommand = resolveJavaCommand(settings.jdkPath);
      return {
        run: {
          command: javaCommand,
          args: ["-jar", settings.serverPath],
          options: runtimeOptions,
        },
        debug: {
          command: javaCommand,
          args: ["-jar", settings.serverPath],
          options: runtimeOptions,
        },
      };
    }

    return {
      run: { command: settings.serverPath, options: runtimeOptions },
      debug: { command: settings.serverPath, args: [], options: runtimeOptions },
    };
  }

  if (context.extensionMode === vscode.ExtensionMode.Development) {
    return {
      run: {
        command: "cargo",
        args: ["run"],
        options: { cwd: getDevServerCwd(context) },
      },
      debug: {
        command: "cargo",
        args: ["run"],
        options: { cwd: getDevServerCwd(context) },
      },
    };
  }

  const serverPath = getBundledServerPath(context);
  return {
    run: { command: serverPath },
    debug: { command: serverPath, args: [] },
  };
}
