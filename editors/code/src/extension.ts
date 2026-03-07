import * as vscode from "vscode";

import { LanguageClientManager } from "./client";
import { registerCommands } from "./commands";
import {
  didRelevantConfigChange,
  getExtensionSettings,
  updateConfigurationValue,
} from "./config";

let clientManager: LanguageClientManager | undefined;

export async function activate(context: vscode.ExtensionContext) {
  clientManager = new LanguageClientManager(context);

  context.subscriptions.push(
    ...registerCommands({
      getSettings: getExtensionSettings,
      updateConfigurationValue,
    }),
    vscode.workspace.onDidChangeConfiguration(async (event) => {
      if (!didRelevantConfigChange(event)) {
        return;
      }

      const restartChoice = "Restart";
      const choice = await vscode.window.showInformationMessage(
        "Java Analyzer configuration changed. Restart the language server to apply the new settings.",
        restartChoice,
        "Later",
      );

      if (choice === restartChoice) {
        await clientManager?.restart(getExtensionSettings());
      }
    }),
  );

  await clientManager.start(getExtensionSettings());
}

export function deactivate(): Thenable<void> | undefined {
  if (!clientManager) {
    return undefined;
  }
  return clientManager.stop();
}
