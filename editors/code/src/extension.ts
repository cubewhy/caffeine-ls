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
import { getClientConfig, selectProjectJdkAction } from "./config";

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
