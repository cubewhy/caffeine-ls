import * as path from "path";
import { spawn } from "child_process";
import * as vscode from "vscode";
import { window, workspace, ExtensionContext, commands } from "vscode";
import {
  ChildProcessInfo,
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
} from "vscode-languageclient/node";
import {
  getClientConfig,
  notifyLspConfigUpdate,
  selectProjectJdkAction,
} from "./config";

/** URI scheme the server serves read-only library views over. */
const LIBRARY_SCHEME = "caffeine-ls";

let client: LanguageClient;
let currentServerPid: number | undefined;

export function activate(context: ExtensionContext) {
  const ext = process.platform === "win32" ? ".exe" : "";
  const binaryName = `caffeine-ls${ext}`;

  const command =
    process.env.CAFFEINE_LS_PATH ||
    context.asAbsolutePath(path.join("bin", binaryName));

  const serverOptions: ServerOptions = () => {
    const config = workspace.getConfiguration("caffeine_ls");
    const logLevel = config.get<string>("logLevel", "warn");
    const waitForDebugger = config.get<boolean>("waitForDebugger", false);

    const child = spawn(command, waitForDebugger ? ["--wait-dbg"] : [], {
      env: { ...process.env, CAFFEINE_LS_LOG: logLevel },
    });
    if (!child.pid) {
      return Promise.reject(
        new Error("Failed to launch the Caffeine LS server process."),
      );
    }
    currentServerPid = child.pid;
    if (waitForDebugger) {
      promptWaitForDebugger();
    }
    return Promise.resolve<ChildProcessInfo>({
      process: child,
      detached: false,
    });
  };

  const initialConfig = getClientConfig(context);

  const clientOptions: LanguageClientOptions = {
    documentSelector: [
      { scheme: "file", language: "java" },
      { scheme: "file", language: "kotlin" },
      // Attached sources retain their language; decompiled views are Java.
      // Both are read-only library documents served by this language server.
      { scheme: LIBRARY_SCHEME, language: "java" },
      { scheme: LIBRARY_SCHEME, language: "kotlin" },
    ],
    initializationOptions: initialConfig,
    synchronize: {
      fileEvents: [
        workspace.createFileSystemWatcher("**/*.{java,kt,kts}"),
        workspace.createFileSystemWatcher(
          "**/{build.gradle,build.gradle.kts,settings.gradle,settings.gradle.kts,pom.xml}",
        ),
      ],
    },
  };

  client = new LanguageClient(
    "caffeine_ls",
    "Caffeine LS",
    serverOptions,
    clientOptions,
  );

  // The server answers go-to-definition for a library class that ships no sources with a
  // `caffeine-ls://` URI pointing at its cached view (real source or decompiled output).
  // Text documents over that scheme are read-only for the client and have no filesystem
  // counterpart to read, so this provider fetching the content from the server is what
  // makes the resulting editors readable at all. Every URI maps back to the file the
  // server materialized, so a failed read is the server's own error and is left to
  // surface as one — never replaced with a stand-in document, which would be analysed
  // as Java and reported as problems.
  context.subscriptions.push(
    workspace.registerTextDocumentContentProvider(LIBRARY_SCHEME, {
      async provideTextDocumentContent(uri) {
        const res = await client.sendRequest(
          "caffeine_ls/libraryFileContent",
          { uri: uri.toString() },
        );
        return (res as { content: string } | null)?.content ?? "";
      },
    }),
  );

  context.subscriptions.push(
    commands.registerCommand("caffeine_ls.selectProjectJdk", async () => {
      await selectProjectJdkAction(context, client);
    }),
  );

  context.subscriptions.push(
    commands.registerCommand("caffeine_ls.restart", async () => {
      client.restart();
    }),
  );

  context.subscriptions.push(
    commands.registerCommand("caffeine_ls.disableWaitForDebugger", async () => {
      await workspace
        .getConfiguration("caffeine_ls")
        .update("waitForDebugger", false, vscode.ConfigurationTarget.Global);
      window
        .showInformationMessage(
          "Wait for debugger disabled. Restart the language server to apply it.",
          "Restart",
        )
        .then((selected) => {
          if (selected === "Restart") {
            client.restart();
          }
        });
    }),
  );

  context.subscriptions.push(
    workspace.onDidChangeConfiguration((event) => {
      if (event.affectsConfiguration("caffeine_ls.downloadSources")) {
        notifyLspConfigUpdate(client, getClientConfig(context));
      }
      if (event.affectsConfiguration("caffeine_ls.decompiler")) {
        notifyLspConfigUpdate(client, getClientConfig(context));
      }
      if (event.affectsConfiguration("caffeine_ls.bootstrapJdk")) {
        // Only the JVM that runs the decompiler changes, so nothing is re-synced:
        // the newly configured JDK is used by the next decompile.
        notifyLspConfigUpdate(client, getClientConfig(context));
      }
      if (event.affectsConfiguration("caffeine_ls.logLevel")) {
        const choice = "Restart";
        window
          .showInformationMessage(
            "Caffeine LS log level changed. Restart the language server to apply it.",
            choice,
          )
          .then((selected) => {
            if (selected === choice) {
              client.restart();
            }
          });
      }
    }),
  );

  client.start();
}

function promptWaitForDebugger() {
  window
    .showInformationMessage(
      `Caffeine LS started with the "wait for debugger" option enabled (PID ${currentServerPid}). Attach a debugger to the process to continue. If you're not on Windows, you need to modify the variable 'd' (make it not equal to 4) to continue.`,
      "Disable Wait for Debugger",
    )
    .then((selected) => {
      if (selected === "Disable Wait for Debugger") {
        commands.executeCommand("caffeine_ls.disableWaitForDebugger");
      }
    });
}

export function deactivate(): Thenable<void> | undefined {
  if (!client) {
    return undefined;
  }
  return client.stop();
}
