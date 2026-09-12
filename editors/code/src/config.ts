import * as fs from "fs";
import { ExtensionContext, window } from "vscode";
import { LanguageClient } from "vscode-languageclient/node";
import { scanForJdks } from "./jdkDetector";
import * as vscode from "vscode";
import path from "path";

export const JDK_STATE_KEY = "caffeine_ls.project.java_home";

export interface ClientConfig {
  cache_dir: string;
  java_home: string | null;
  /**
   * JDK home whose `bin/java` runs the decompiler — a newer JVM than the project
   * compiles against is often needed by the decompiler itself. `null` (the
   * default) falls back to `java_home`; a path without `bin/java` falls back
   * server-side as well.
   */
  bootstrap_java_home: string | null;
  download_sources: boolean;
  /**
   * Decompiler backend the server runs on library classes that ship no sources:
   * `"vineflower"` (the default), `"cfr"` or `"none"`.
   */
  decompiler: string;
  /**
   * Backend id to the absolute path of the jar that implements it. Only jars that
   * exist on disk are sent, so an installation without them simply leaves the
   * corresponding backend disabled on the server.
   */
  decompiler_jars: Record<string, string>;
  /** URI scheme the server serves read-only library views over. */
  library_uri_scheme: string;
}

/**
 * Constructs the configuration payload.
 */
export function getClientConfig(context: ExtensionContext): ClientConfig {
  const cacheDir = context.globalStorageUri.fsPath;
  if (!fs.existsSync(cacheDir)) {
    fs.mkdirSync(cacheDir, { recursive: true });
  }

  const javaHome = context.workspaceState.get<string>(JDK_STATE_KEY) || null;

  const downloadSources = vscode.workspace
    .getConfiguration("caffeine_ls")
    .get<boolean>("downloadSources", false);

  const decompiler = vscode.workspace
    .getConfiguration("caffeine_ls")
    .get<string>("decompiler", "vineflower");

  // An empty setting means "use the project JDK": the server falls back itself,
  // so an unusable path is its decision to report, not ours to guess at.
  const bootstrapJdk = vscode.workspace
    .getConfiguration("caffeine_ls")
    .get<string>("bootstrapJdk", "")
    .trim();

  return {
    cache_dir: cacheDir,
    java_home: javaHome,
    bootstrap_java_home: bootstrapJdk.length > 0 ? bootstrapJdk : null,
    download_sources: downloadSources,
    decompiler,
    decompiler_jars: Object.fromEntries(
      ["cfr", "vineflower"]
        .map((id): [string, string] => [
          id,
          context.asAbsolutePath(
            path.join("resources", "decompilers", `${id}.jar`),
          ),
        ])
        .filter(([, jarPath]) => fs.existsSync(jarPath)),
    ),
    library_uri_scheme: "caffeine-ls",
  };
}

/**
 * Dispatches a standard workspace/didChangeConfiguration notification to the LSP server.
 */
export function notifyLspConfigUpdate(
  client: LanguageClient,
  config: ClientConfig,
) {
  if (!client || !client.needsStart()) {
    // Pack the payload inside a structured 'settings' object matching LSP standards
    client.sendNotification("workspace/didChangeConfiguration", {
      settings: {
        caffeine_ls: config,
      },
    });
  }
}

/**
 * Command Implementation: Prompts the user to pick from auto-detected JDKs
 */
export async function selectProjectJdkAction(
  context: ExtensionContext,
  client: LanguageClient,
) {
  window
    .withProgress(
      {
        location: vscode.ProgressLocation.Notification,
        title: "Scanning for local JDKs...",
        cancellable: false,
      },
      async () => {
        return await scanForJdks();
      },
    )
    .then(async (jdks) => {
      if (jdks.length === 0) {
        window.showWarningMessage("No valid JDKs discovered automatically.");
        return;
      }

      const currentJdk = context.workspaceState.get<string>(JDK_STATE_KEY);

      const items = jdks.map((jdk) => ({
        label: path.basename(jdk),
        description: jdk,
        detail: jdk === currentJdk ? "$(check) Currently Selected" : undefined,
      }));

      const selected = await window.showQuickPick(items, {
        placeHolder: "Select a JDK for this project",
      });

      if (selected) {
        await context.workspaceState.update(
          JDK_STATE_KEY,
          selected.description,
        );
        window.showInformationMessage(
          `Project JDK switched to: ${selected.description}`,
        );

        // Dynamic Hot Update
        const updatedConfig = getClientConfig(context);
        notifyLspConfigUpdate(client, updatedConfig);
      }
    });
}
