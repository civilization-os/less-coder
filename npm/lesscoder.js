#!/usr/bin/env node
"use strict";

const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

const repoRoot = path.resolve(__dirname, "..");
const args = process.argv.slice(2);

function runPython(cmd, cmdArgs) {
  const env = { ...process.env };
  const adapterBin = resolveOptionalAdapterPath();
  if (!env.LESSCODER_ADAPTER_BIN && adapterBin) {
    env.LESSCODER_ADAPTER_BIN = adapterBin;
  }
  return spawnSync(cmd, cmdArgs, {
    cwd: repoRoot,
    stdio: "inherit",
    env,
    shell: false,
  });
}

function resolveOptionalAdapterPath() {
  try {
    const adapterPkg = require("@civilization/lesscoder-adapter-win32-x64");
    const candidate = adapterPkg && adapterPkg.adapterPath;
    if (typeof candidate === "string" && candidate.length > 0 && fs.existsSync(candidate)) {
      return candidate;
    }
  } catch (_err) {
    // optional dependency may not be installed on non-win32/x64 platforms
  }
  return null;
}

function main() {
  const preferred = process.env.LESSCODER_PYTHON;
  const candidates = preferred
    ? [[preferred, ["-m", "clients.cli.lesscoder", ...args]]]
    : process.platform === "win32"
      ? [
          ["python", ["-m", "clients.cli.lesscoder", ...args]],
          ["py", ["-3", "-m", "clients.cli.lesscoder", ...args]],
        ]
      : [["python3", ["-m", "clients.cli.lesscoder", ...args]], ["python", ["-m", "clients.cli.lesscoder", ...args]]];

  for (const [cmd, cmdArgs] of candidates) {
    const result = runPython(cmd, cmdArgs);
    if (!result.error) {
      process.exit(typeof result.status === "number" ? result.status : 0);
    }
  }

  process.stderr.write(
    "lesscoder: Python 3.11+ not found. Install Python and retry, or set LESSCODER_PYTHON.\n"
  );
  process.exit(127);
}

main();
