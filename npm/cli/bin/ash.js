#!/usr/bin/env node
"use strict";

// 转发层：把 `ash` / `ai-session-hub` 调用转到当前平台的预编译二进制。
// 平台表需要与 scripts/npm-platforms.mjs 保持一致；本文件独立随包发布，
// 所以这里保留一份硬编码副本，不引用仓库内的其他模块。
const PLATFORM_PACKAGES = {
  "darwin-arm64": "ai-session-hub-darwin-arm64",
  "darwin-x64": "ai-session-hub-darwin-x64",
  "linux-arm64": "ai-session-hub-linux-arm64",
  "linux-x64": "ai-session-hub-linux-x64",
};

const { spawnSync } = require("node:child_process");

const platformKey = `${process.platform}-${process.arch}`;
const platformPackage = PLATFORM_PACKAGES[platformKey];

const SOURCE_HINT = [
  "可以改用源码安装 / build from source:",
  "  git clone https://github.com/VangelisHaha/ai-session-hub.git",
  "  cd ai-session-hub && cargo build --release",
].join("\n");

if (!platformPackage) {
  const windowsHint =
    process.platform === "win32"
      ? "\n原生 Windows 暂不支持（会话路径依赖 HOME、状态判定依赖 ps），请在 WSL 中使用。\nNative Windows is not supported yet (session paths rely on HOME, liveness relies on `ps`); please use WSL."
      : "";
  console.error(
    `ai-session-hub: 暂无 ${platformKey} 的预编译二进制 / no prebuilt binary for ${platformKey}.${windowsHint}\n${SOURCE_HINT}`,
  );
  process.exit(1);
}

const binaryName = process.platform === "win32" ? "ash.exe" : "ash";

let binaryPath;
try {
  binaryPath = require.resolve(`${platformPackage}/bin/${binaryName}`);
} catch {
  console.error(
    [
      `ai-session-hub: 找不到平台包 ${platformPackage} / failed to resolve ${platformPackage}.`,
      "如果安装时跳过了 optionalDependencies（例如 --no-optional 或离线镜像），请重新安装：",
      "If optional dependencies were skipped (e.g. --no-optional or an offline mirror), reinstall:",
      "  npm install -g ai-session-hub",
      SOURCE_HINT,
    ].join("\n"),
  );
  process.exit(1);
}

const result = spawnSync(binaryPath, process.argv.slice(2), { stdio: "inherit" });

if (result.error) {
  const isExecFailure =
    result.error.code === "ENOENT" || result.error.code === "EACCES";
  console.error(
    [
      `ai-session-hub: 无法执行 ${binaryPath} / failed to execute ${binaryPath}`,
      String(result.error.message || result.error),
      isExecFailure
        ? "Linux 预编译包基于 glibc；musl 系发行版（如 Alpine）请从源码构建。\nPrebuilt Linux binaries target glibc; on musl distros such as Alpine, build from source."
        : "",
      SOURCE_HINT,
    ]
      .filter(Boolean)
      .join("\n"),
  );
  process.exit(1);
}

if (typeof result.status === "number") {
  process.exit(result.status);
}

// 被信号终止时，用 128 + signal 的惯例返回退出码。
const SIGNAL_EXIT_BASE = 128;
const signalNumbers = { SIGINT: 2, SIGQUIT: 3, SIGKILL: 9, SIGTERM: 15 };
process.exit(SIGNAL_EXIT_BASE + (signalNumbers[result.signal] || 0));
