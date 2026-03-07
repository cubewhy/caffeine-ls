import * as vscode from "vscode";

import {
  LanguageClient,
  type LanguageClientOptions,
} from "vscode-languageclient/node";

import {
  EXTENSION_ID,
  normalizeOptionalPath,
  type ExtensionSettings,
} from "./config";
import { resolveServerOptions } from "./serverPath";

function createClientOptions(settings: ExtensionSettings): LanguageClientOptions {
  return {
    documentSelector: [
      { scheme: "file", language: "java" },
      { scheme: "file", language: "kotlin" },
    ],
    synchronize: {
      fileEvents: vscode.workspace.createFileSystemWatcher("**/.clientrc"),
    },
    initializationOptions: {
      jdkPath: normalizeOptionalPath(settings.jdkPath),
      decompilerPath: normalizeOptionalPath(settings.decompilerPath),
      decompilerBackend: settings.decompilerBackend,
    },
  };
}

export class LanguageClientManager {
  private client: LanguageClient | undefined;

  constructor(private readonly context: vscode.ExtensionContext) {}

  async start(settings: ExtensionSettings): Promise<void> {
    this.client = new LanguageClient(
      EXTENSION_ID,
      EXTENSION_ID,
      resolveServerOptions(this.context, settings),
      createClientOptions(settings),
    );

    await this.client.start();
  }

  async stop(): Promise<void> {
    if (!this.client) {
      return;
    }

    await this.client.stop();
    this.client = undefined;
  }

  async restart(settings: ExtensionSettings): Promise<void> {
    await this.stop();
    await this.start(settings);
  }
}
