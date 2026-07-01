import * as path from "path";
import { workspace, ExtensionContext, commands } from "vscode";
import {
  Executable,
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
} from "vscode-languageclient/node";
import { getClientConfig, selectProjectJdkAction } from "./config";

let client: LanguageClient;

export function activate(context: ExtensionContext) {
  const ext = process.platform === "win32" ? ".exe" : "";
  const binaryName = `caffeine-ls${ext}`;

  const command =
    process.env.CAFFEINE_LS_PATH ||
    context.asAbsolutePath(path.join("bin", binaryName));

  const run: Executable = {
    command,
    options: { env: process.env },
  };

  const serverOptions: ServerOptions = { run, debug: run };

  const initialConfig = getClientConfig(context);

  const clientOptions: LanguageClientOptions = {
    documentSelector: [
      { scheme: "file", language: "java" },
      { scheme: "file", language: "kotlin" },
    ],
    initializationOptions: initialConfig,
    synchronize: {
      fileEvents: [
        workspace.createFileSystemWatcher(
          "**/{build.gradle,build.gradle.kts,settings.gradle,settings.gradle.kts,pom.xml}",
        ),
      ],
    },
  };

  client = new LanguageClient(
    "caffeine-ls",
    "Caffeine LS",
    serverOptions,
    clientOptions,
  );

  context.subscriptions.push(
    commands.registerCommand("caffeinels.selectProjectJdk", async () => {
      await selectProjectJdkAction(context, client);
    }),
  );

  client.start();
}

export function deactivate(): Thenable<void> | undefined {
  if (!client) {
    return undefined;
  }
  return client.stop();
}
