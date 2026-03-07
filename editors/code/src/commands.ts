import * as vscode from "vscode";

import {
  CONFIG_NAMESPACE,
  EXTENSION_CONFIG_KEYS,
  type DecompilerBackend,
  type ExtensionConfigKey,
  type ExtensionSettings,
} from "./config";

const COMMAND_SET_JDK_PATH = `${CONFIG_NAMESPACE}.setJdkPath`;
const COMMAND_SET_DECOMPILER_PATH = `${CONFIG_NAMESPACE}.setDecompilerPath`;
const COMMAND_SELECT_DECOMPILER_BACKEND = `${CONFIG_NAMESPACE}.selectDecompilerBackend`;
const COMMAND_SET_SERVER_PATH = `${CONFIG_NAMESPACE}.setServerPath`;

interface SelectableItem<T extends string> extends vscode.QuickPickItem {
  value: T;
}

interface CommandDependencies {
  getSettings: () => ExtensionSettings;
  updateConfigurationValue: (key: ExtensionConfigKey, value: string) => Thenable<void>;
}

export function registerCommands(deps: CommandDependencies): vscode.Disposable[] {
  return [
    vscode.commands.registerCommand(COMMAND_SET_JDK_PATH, () => setJdkPath(deps)),
    vscode.commands.registerCommand(COMMAND_SET_DECOMPILER_PATH, () => setDecompilerPath(deps)),
    vscode.commands.registerCommand(
      COMMAND_SELECT_DECOMPILER_BACKEND,
      () => selectDecompilerBackend(deps),
    ),
    vscode.commands.registerCommand(COMMAND_SET_SERVER_PATH, () => setServerPath(deps)),
  ];
}

async function choosePath(
  title: string,
  currentValue: string,
  canSelectFiles: boolean,
  canSelectFolders: boolean,
): Promise<string | undefined> {
  const action = await vscode.window.showQuickPick(
    [
      {
        label: "$(folder-opened) Browse...",
        value: "browse",
      },
      {
        label: "$(edit) Enter path manually",
        value: "manual",
      },
      {
        label: "$(circle-slash) Clear",
        value: "clear",
      },
    ] satisfies Array<SelectableItem<"browse" | "manual" | "clear">>,
    {
      title,
      placeHolder: "Choose how to set this path",
    },
  );

  if (!action) {
    return undefined;
  }

  if (action.value === "clear") {
    return "";
  }

  if (action.value === "manual") {
    const manualPath = await vscode.window.showInputBox({
      title,
      prompt: "Enter path",
      value: currentValue,
      ignoreFocusOut: true,
    });
    if (manualPath === undefined) {
      return undefined;
    }
    return manualPath.trim();
  }

  const picked = await vscode.window.showOpenDialog({
    title,
    canSelectFiles,
    canSelectFolders,
    canSelectMany: false,
    defaultUri: currentValue ? vscode.Uri.file(currentValue) : undefined,
    openLabel: "Select",
  });

  if (!picked || picked.length === 0) {
    return undefined;
  }

  return picked[0].fsPath;
}

async function setJdkPath(deps: CommandDependencies): Promise<void> {
  const currentPath = deps.getSettings().jdkPath;
  const selectedPath = await choosePath("Set JDK Path", currentPath, false, true);

  if (selectedPath === undefined) {
    return;
  }

  await deps.updateConfigurationValue(EXTENSION_CONFIG_KEYS.jdkPath, selectedPath);
}

async function setDecompilerPath(deps: CommandDependencies): Promise<void> {
  const currentPath = deps.getSettings().decompilerPath;
  const selectedPath = await choosePath("Set Decompiler Path", currentPath, true, false);

  if (selectedPath === undefined) {
    return;
  }

  await deps.updateConfigurationValue(EXTENSION_CONFIG_KEYS.decompilerPath, selectedPath);
}

async function setServerPath(deps: CommandDependencies): Promise<void> {
  const currentPath = deps.getSettings().serverPath;
  const selectedPath = await choosePath("Set Server Path", currentPath, true, false);

  if (selectedPath === undefined) {
    return;
  }

  await deps.updateConfigurationValue(EXTENSION_CONFIG_KEYS.serverPath, selectedPath);
}

async function selectDecompilerBackend(deps: CommandDependencies): Promise<void> {
  const picked = await vscode.window.showQuickPick(
    [
      {
        label: "vineflower",
        detail: "Use the Vineflower decompiler backend",
        value: "vineflower",
      },
      {
        label: "cfr",
        detail: "Use the CFR decompiler backend",
        value: "cfr",
      },
    ] satisfies Array<SelectableItem<DecompilerBackend>>,
    {
      title: "Select Decompiler Backend",
      placeHolder: "Choose Java decompiler backend",
    },
  );

  if (!picked) {
    return;
  }

  await deps.updateConfigurationValue(EXTENSION_CONFIG_KEYS.decompilerBackend, picked.value);
}
