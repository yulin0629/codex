#!/usr/bin/env node
// Unified entry point for the Codex CLI.
/*
 * Behavior
 * =========
 *   1. By default we import the JavaScript implementation located in
 *      dist/cli.js.
 *
 *   2. Developers can opt-in to a pre-compiled Rust binary by setting the
 *      environment variable CODEX_RUST to a truthy value (`1`, `true`, etc.).
 *      When that variable is present we resolve the correct binary for the
 *      current platform / architecture and execute it via child_process.
 *
 *      If the CODEX_RUST=1 is specified and there is no native binary for the
 *      current platform / architecture, an error is thrown.
 */

import { spawnSync } from "child_process";
import fs from "fs";
import path from "path";
import { fileURLToPath, pathToFileURL } from "url";

// Determine whether the user explicitly wants the Rust CLI.

// __dirname equivalent in ESM
const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

// For the @native release of the Node module, the `use-native` file is added,
// indicating we should default to the native binary. For other releases,
// setting CODEX_RUST=1 will opt-in to the native binary, if included.
const wantsNative = fs.existsSync(path.join(__dirname, "use-native")) ||
  (process.env.CODEX_RUST != null
    ? ["1", "true", "yes"].includes(process.env.CODEX_RUST.toLowerCase())
    : false);

// Try native binary if requested.
if (wantsNative) {
  const { platform, arch } = process;

  let targetTriple = null;
  switch (platform) {
    case "linux":
      switch (arch) {
        case "x64":
          targetTriple = "x86_64-unknown-linux-musl";
          break;
        case "arm64":
          targetTriple = "aarch64-unknown-linux-musl";
          break;
        default:
          break;
      }
      break;
    case "darwin":
      switch (arch) {
        case "x64":
          targetTriple = "x86_64-apple-darwin";
          break;
        case "arm64":
          targetTriple = "aarch64-apple-darwin";
          break;
        default:
          break;
      }
      break;
    default:
      break;
  }

  if (!targetTriple) {
    throw new Error(`Unsupported platform: ${platform} (${arch})`);
  }

  const binaryPath = path.join(__dirname, "..", "bin", `codex-${targetTriple}`);
  const result = spawnSync(binaryPath, process.argv.slice(2), {
    stdio: "inherit",
  });

  const exitCode = typeof result.status === "number" ? result.status : 1;
  process.exit(exitCode);
}

// Fallback: execute the original JavaScript CLI.

// Resolve the path to the compiled CLI bundle
const cliPath = path.resolve(__dirname, "../dist/cli.js");
const cliUrl = pathToFileURL(cliPath).href;

// Load and execute the CLI
(async () => {
  try {
    await import(cliUrl);
  } catch (err) {
    // eslint-disable-next-line no-console
    console.error(err);
    process.exit(1);
  }
})();
