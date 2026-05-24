#!/usr/bin/env node

const { spawnSync } = require("node:child_process");
const { join } = require("node:path");

const wrapper = join(__dirname, "..", "bin", "agentmesh");
const installArg =
  process.env.AGENTMESH_NPM_POSTINSTALL_SMOKE === "1" ? "--smoke" : "--install";
const result = spawnSync(wrapper, [installArg], {
  stdio: "inherit",
  shell: process.platform === "win32",
});

if (result.error) {
  console.error(result.error.message);
  process.exit(1);
}

process.exit(result.status ?? 1);
