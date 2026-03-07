import * as vscode from "vscode";

export const CONFIG_NAMESPACE = "java-analyzer";
export const EXTENSION_ID = "java-analyzer";

export const EXTENSION_CONFIG_KEYS = {
  jdkPath: "jdkPath",
  decompilerPath: "decompilerPath",
  decompilerBackend: "decompilerBackend",
  serverPath: "serverPath",
} as const;

const RELEVANT_CONFIGURATION_PATHS = Object
  .values(EXTENSION_CONFIG_KEYS)
  .map((key) => `${CONFIG_NAMESPACE}.${key}`);

export type ExtensionConfigKey =
  (typeof EXTENSION_CONFIG_KEYS)[keyof typeof EXTENSION_CONFIG_KEYS];

export type DecompilerBackend = "vineflower" | "cfr";

export interface ExtensionSettings {
  jdkPath: string;
  decompilerPath: string;
  decompilerBackend: DecompilerBackend;
  serverPath: string;
}

export function getExtensionSettings(): ExtensionSettings {
  const config = vscode.workspace.getConfiguration(CONFIG_NAMESPACE);
  return {
    jdkPath: config.get<string>(EXTENSION_CONFIG_KEYS.jdkPath, "").trim(),
    decompilerPath: config.get<string>(EXTENSION_CONFIG_KEYS.decompilerPath, "").trim(),
    decompilerBackend: normalizeDecompilerBackend(
      config.get<string>(EXTENSION_CONFIG_KEYS.decompilerBackend),
    ),
    serverPath: config.get<string>(EXTENSION_CONFIG_KEYS.serverPath, "").trim(),
  };
}

export function didRelevantConfigChange(event: vscode.ConfigurationChangeEvent): boolean {
  return RELEVANT_CONFIGURATION_PATHS.some((path) => event.affectsConfiguration(path));
}

export function updateConfigurationValue(
  key: ExtensionConfigKey,
  value: string,
): Thenable<void> {
  return vscode.workspace
    .getConfiguration(CONFIG_NAMESPACE)
    .update(key, value, vscode.ConfigurationTarget.Global);
}

export function normalizeOptionalPath(value: string): string | undefined {
  const trimmed = value.trim();
  if (!trimmed) {
    return undefined;
  }
  return trimmed;
}

function normalizeDecompilerBackend(value: string | undefined): DecompilerBackend {
  if (value === "cfr") {
    return value;
  }
  return "vineflower";
}
