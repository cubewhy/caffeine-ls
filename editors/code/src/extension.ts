import * as path from "path";
import { workspace, ExtensionContext } from "vscode";
import {
  Executable,
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
} from "vscode-languageclient/node";

let client: LanguageClient;

export function activate(context: ExtensionContext) {
  const ext = process.platform === "win32" ? ".exe" : "";
  const binaryName = `caffeine-ls${ext}`;

  const command =
    process.env.CAFFEINE_LS_PATH ||
    context.asAbsolutePath(path.join("bin", binaryName));

  const run: Executable = {
    command,
    options: {
      env: process.env,
    },
  };

  const serverOptions: ServerOptions = {
    run,
    debug: run,
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [
      { scheme: "file", language: "java" },
      { scheme: "file", language: "kotlin" },
    ],
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

  client.start();
}

export function deactivate(): Thenable<void> | undefined {
  if (!client) {
    return undefined;
  }
  return client.stop();
}
