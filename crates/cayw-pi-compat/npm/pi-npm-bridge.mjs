#!/usr/bin/env node
/**
 * CAYW ↔ Pi ExtensionAPI adapter (F15).
 *
 * Loads an *unmodified* Pi npm extension (default export receiving ExtensionAPI)
 * via jiti, then speaks the CAYW external plugin JSON-lines protocol on
 * stdin/stdout. The npm package itself is never rewritten; translation lives
 * only in this adapter.
 *
 * Scope (honest): tools.registerTool / tools.invoke for the pinned hello.ts
 * slice only. commands / events / sendMessage / session / UI throw
 * `unsupported:` — not silent noops, not full Pi ExtensionAPI compatibility.
 *
 * Usage:
 *   node pi-npm-bridge.mjs \
 *     --extension <abs-path-to-extension.ts|js> \
 *     --package-name <@scope/name> \
 *     --package-version <semver>
 */

import { createJiti } from "jiti";
import { createInterface } from "node:readline";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { existsSync, readFileSync } from "node:fs";

const PLUGIN_PROTOCOL_VERSION = 1;

function parseArgs(argv) {
  const out = {
    extension: null,
    packageName: null,
    packageVersion: null,
  };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--extension") out.extension = argv[++i];
    else if (a === "--package-name") out.packageName = argv[++i];
    else if (a === "--package-version") out.packageVersion = argv[++i];
    else if (a === "--help" || a === "-h") {
      process.stderr.write(
        "Usage: pi-npm-bridge.mjs --extension <path> --package-name <name> --package-version <ver>\n",
      );
      process.exit(0);
    }
  }
  if (!out.extension || !out.packageName || !out.packageVersion) {
    throw new Error(
      "required: --extension, --package-name, --package-version",
    );
  }
  if (!path.isAbsolute(out.extension)) {
    throw new Error(`--extension must be absolute: ${out.extension}`);
  }
  if (!existsSync(out.extension)) {
    throw new Error(`extension not found: ${out.extension}`);
  }
  return out;
}

function schemaFromParameters(parameters) {
  if (!parameters || typeof parameters !== "object") {
    return { type: "object", properties: {}, additionalProperties: true };
  }
  // typebox schemas are plain JSON-schema-like objects already
  return parameters;
}

function unsupported(surface) {
  // Honest refusal: never silent noop for unavailable / incompatible surfaces.
  // Proven path this cut: tools.registerTool + tools.invoke (hello.ts only).
  return (..._args) => {
    throw new Error(
      `unsupported: ExtensionAPI.${surface} is unavailable in the CAYW Pi npm bridge (F15 hello-tool slice only; not full Pi extension compatibility)`,
    );
  };
}

function createExtensionApi(state) {
  const api = {
    registerTool(tool) {
      if (!tool || typeof tool.name !== "string" || !tool.name) {
        throw new Error("registerTool: tool.name required");
      }
      if (typeof tool.execute !== "function") {
        throw new Error(`registerTool(${tool.name}): execute required`);
      }
      state.tools.set(tool.name, tool);
    },
    // Everything below is explicit unsupported — matrix marks these unavailable /
    // incompatible. Do not silent-noop; hello.ts only needs registerTool.
    registerCommand: unsupported("registerCommand"),
    registerShortcut: unsupported("registerShortcut"),
    registerFlag: unsupported("registerFlag"),
    getFlag: () => undefined,
    on: unsupported("on"),
    events: {
      on: unsupported("events.on"),
      emit: unsupported("events.emit"),
      off: unsupported("events.off"),
    },
    appendEntry: unsupported("appendEntry"),
    setActiveTools: unsupported("setActiveTools"),
    getAllTools: () =>
      [...state.tools.values()].map((t) => ({
        name: t.name,
        description: t.description ?? "",
      })),
    getActiveTools: () => [...state.tools.keys()],
    sendMessage: unsupported("sendMessage"),
    exec: unsupported("exec"),
    unregisterTool: unsupported("unregisterTool"),
    setModel: unsupported("setModel"),
    getModel: () => undefined,
  };
  return api;
}

function buildManifest(args, tools) {
  const capabilities = [...tools.values()].map((tool) => ({
    name: tool.name,
    description: tool.description || tool.label || tool.name,
    parameters: schemaFromParameters(tool.parameters),
  }));
  return {
    name: args.packageName,
    version: args.packageVersion,
    protocol_version: PLUGIN_PROTOCOL_VERSION,
    kind: "external",
    capabilities,
    permissions: {},
  };
}

function emit(msg) {
  process.stdout.write(`${JSON.stringify(msg)}\n`);
}

async function loadExtension(extensionPath, api) {
  const bridgeDir = path.dirname(fileURLToPath(import.meta.url));
  // Resolve jiti + peers from the npm workspace next to this crate.
  const npmRoot = bridgeDir;
  const jiti = createJiti(path.join(npmRoot, "bridge-loader.mjs"), {
    interopDefault: true,
    extensions: [".ts", ".js", ".mjs", ".cjs", ".json"],
  });
  const mod = await jiti.import(extensionPath);
  const factory = mod?.default ?? mod;
  if (typeof factory !== "function") {
    throw new Error(
      `extension default export is not a function: ${extensionPath}`,
    );
  }
  await factory(api);
}

async function handleCall(state, method, arguments_) {
  const tool = state.tools.get(method);
  if (!tool) {
    return {
      status: "error",
      message: `unknown capability ${method}`,
      outcome_unknown: false,
    };
  }
  try {
    const result = await tool.execute(
      `cayw-${Date.now()}`,
      arguments_ ?? {},
      undefined,
      undefined,
      {
        cwd: process.cwd(),
        hasUI: false,
        ui: {
          notify: unsupported("ui.notify"),
          select: unsupported("ui.select"),
          confirm: unsupported("ui.confirm"),
          input: unsupported("ui.input"),
        },
        // shutdown is lifecycle housekeeping; keep no-op (no claimed side effect).
        shutdown: () => {},
      },
    );
    const textParts = Array.isArray(result?.content)
      ? result.content
          .filter((c) => c && c.type === "text" && typeof c.text === "string")
          .map((c) => c.text)
      : [];
    return {
      status: "ok",
      data: {
        text: textParts.join("\n"),
        content: result?.content ?? [],
        details: result?.details ?? {},
        isError: Boolean(result?.isError),
        // Proof markers: acceptance tests require these (Rust stub alone fails).
        pi_extension_api: true,
        extension_path: state.extensionPath,
        package: state.packageName,
        package_version: state.packageVersion,
        protocol: "pi-ExtensionAPI",
      },
    };
  } catch (err) {
    return {
      status: "error",
      message: err instanceof Error ? err.message : String(err),
      outcome_unknown: false,
    };
  }
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const state = {
    tools: new Map(),
    extensionPath: args.extension,
    packageName: args.packageName,
    packageVersion: args.packageVersion,
  };
  const api = createExtensionApi(state);
  await loadExtension(args.extension, api);
  if (state.tools.size === 0) {
    throw new Error(
      `Pi extension registered zero tools: ${args.extension} (not a usable tools surface)`,
    );
  }

  const rl = createInterface({ input: process.stdin, crlfDelay: Infinity });
  let handshaken = false;

  for await (const line of rl) {
    if (!line.trim()) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch (err) {
      throw new Error(`invalid host JSON: ${err}`);
    }
    if (!handshaken) {
      if (msg.type !== "handshake") {
        throw new Error("expected handshake");
      }
      if (msg.protocol_version !== PLUGIN_PROTOCOL_VERSION) {
        throw new Error(
          `protocol mismatch: host=${msg.protocol_version} bridge=${PLUGIN_PROTOCOL_VERSION}`,
        );
      }
      emit({ type: "handshake", manifest: buildManifest(args, state.tools) });
      handshaken = true;
      continue;
    }
    if (msg.type !== "call") {
      throw new Error(`expected call, got ${msg.type}`);
    }
    const result = await handleCall(state, msg.method, msg.arguments);
    emit({ type: "response", request_id: msg.request_id, result });
  }
}

main().catch((err) => {
  process.stderr.write(`pi-npm-bridge: ${err?.stack || err}\n`);
  process.exit(1);
});
