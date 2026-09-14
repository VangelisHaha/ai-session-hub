// 单一平台表：release 工作流的构建矩阵、npm 平台包和 npm/cli/bin/ash.js 都以此为准。
// 修改这里之后，记得同步 npm/cli/bin/ash.js 里的 PLATFORM_PACKAGES 副本。
export const PLATFORMS = [
  {
    npmTarget: "darwin-arm64",
    rustTarget: "aarch64-apple-darwin",
    os: "darwin",
    cpu: "arm64",
    binary: "ash",
  },
  {
    npmTarget: "darwin-x64",
    rustTarget: "x86_64-apple-darwin",
    os: "darwin",
    cpu: "x64",
    binary: "ash",
  },
  {
    npmTarget: "linux-arm64",
    rustTarget: "aarch64-unknown-linux-gnu",
    os: "linux",
    cpu: "arm64",
    binary: "ash",
  },
  {
    npmTarget: "linux-x64",
    rustTarget: "x86_64-unknown-linux-gnu",
    os: "linux",
    cpu: "x64",
    binary: "ash",
  },
];
// 暂不发布 Windows 包：paths.rs 只读 HOME、liveness.rs 依赖 `ps`，
// 原生 Windows 上无法正常工作，Windows 用户请走 WSL。

export function platformPackageName(npmTarget) {
  return `ai-session-hub-${npmTarget}`;
}
