#!/usr/bin/env node
// 组装待发布的 npm 包：主包 ai-session-hub + 各平台二进制包。
//
// 用法：
//   node scripts/npm-prepare.mjs --input <二进制目录> [--output npm/build] [--version 0.1.0]
//
// --input 目录下每个平台一个子目录，名字是 npmTarget（darwin-arm64 等），
// 里面放对应的 ash / ash.exe。release 工作流下载 artifact 后正好是这个结构。
// 缺失的平台会被跳过并在末尾汇总，方便本地只打一个平台做验证。

import { existsSync } from "node:fs";
import { chmod, copyFile, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PLATFORMS, platformPackageName } from "./npm-platforms.mjs";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

function parseArgs(argv) {
  const args = { input: null, output: "npm/build", version: null };
  for (let i = 0; i < argv.length; i += 1) {
    const key = argv[i];
    const value = argv[i + 1];
    if (key === "--input" || key === "--output" || key === "--version") {
      if (!value) {
        throw new Error(`${key} 缺少取值`);
      }
      args[key.slice(2)] = value;
      i += 1;
      continue;
    }
    throw new Error(`未知参数：${key}`);
  }
  if (!args.input) {
    throw new Error("必须通过 --input 指定二进制目录");
  }
  return args;
}

async function readCargoVersion() {
  const cargoToml = await readFile(path.join(repoRoot, "Cargo.toml"), "utf8");
  const match = cargoToml.match(/^\s*version\s*=\s*"([^"]+)"/m);
  if (!match) {
    throw new Error("无法从 Cargo.toml 解析 version");
  }
  return match[1];
}

async function writeJson(filePath, value) {
  await writeFile(filePath, `${JSON.stringify(value, null, 2)}\n`, "utf8");
}

async function buildPlatformPackage({ platform, version, inputDir, outputDir }) {
  const sourceBinary = path.join(inputDir, platform.npmTarget, platform.binary);
  if (!existsSync(sourceBinary)) {
    return { skipped: true, reason: `缺少二进制 ${sourceBinary}` };
  }

  const packageName = platformPackageName(platform.npmTarget);
  const packageDir = path.join(outputDir, packageName);
  await mkdir(path.join(packageDir, "bin"), { recursive: true });

  const targetBinary = path.join(packageDir, "bin", platform.binary);
  await copyFile(sourceBinary, targetBinary);
  if (platform.os !== "win32") {
    await chmod(targetBinary, 0o755);
  }

  await writeJson(path.join(packageDir, "package.json"), {
    name: packageName,
    version,
    description: `Prebuilt ai-session-hub (ash) binary for ${platform.npmTarget}.`,
    license: "MIT",
    author: "VangelisHaha",
    homepage: "https://github.com/VangelisHaha/ai-session-hub#readme",
    repository: {
      type: "git",
      url: "git+https://github.com/VangelisHaha/ai-session-hub.git",
    },
    os: [platform.os],
    cpu: [platform.cpu],
    files: [`bin/${platform.binary}`],
    preferUnplugged: true,
  });

  await writeFile(
    path.join(packageDir, "README.md"),
    [
      `# ${packageName}`,
      "",
      `Prebuilt \`ash\` binary for \`${platform.npmTarget}\` (Rust target \`${platform.rustTarget}\`).`,
      "",
      "Do not install this package directly. Install [`ai-session-hub`](https://www.npmjs.com/package/ai-session-hub) instead;",
      "npm picks the matching platform package automatically.",
      "",
    ].join("\n"),
    "utf8",
  );

  return { skipped: false, packageDir };
}

async function buildMainPackage({ version, outputDir, availableTargets }) {
  const packageDir = path.join(outputDir, "ai-session-hub");
  await mkdir(path.join(packageDir, "bin"), { recursive: true });

  const manifest = JSON.parse(
    await readFile(path.join(repoRoot, "npm", "cli", "package.json"), "utf8"),
  );
  manifest.version = version;
  manifest.optionalDependencies = Object.fromEntries(
    availableTargets.map((npmTarget) => [platformPackageName(npmTarget), version]),
  );

  await writeJson(path.join(packageDir, "package.json"), manifest);
  await copyFile(
    path.join(repoRoot, "npm", "cli", "bin", "ash.js"),
    path.join(packageDir, "bin", "ash.js"),
  );
  await chmod(path.join(packageDir, "bin", "ash.js"), 0o755);
  await copyFile(path.join(repoRoot, "README.md"), path.join(packageDir, "README.md"));
  await copyFile(path.join(repoRoot, "LICENSE"), path.join(packageDir, "LICENSE"));

  return packageDir;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const version = args.version ?? (await readCargoVersion());
  const inputDir = path.resolve(repoRoot, args.input);
  const outputDir = path.resolve(repoRoot, args.output);

  await rm(outputDir, { recursive: true, force: true });
  await mkdir(outputDir, { recursive: true });

  const built = [];
  const skipped = [];
  for (const platform of PLATFORMS) {
    const result = await buildPlatformPackage({ platform, version, inputDir, outputDir });
    if (result.skipped) {
      skipped.push(`${platform.npmTarget}（${result.reason}）`);
      continue;
    }
    built.push(platform.npmTarget);
    console.log(`✓ ${platformPackageName(platform.npmTarget)}@${version}`);
  }

  if (built.length === 0) {
    throw new Error(`在 ${inputDir} 下没有找到任何平台二进制`);
  }

  const mainPackageDir = await buildMainPackage({
    version,
    outputDir,
    availableTargets: built,
  });
  console.log(`✓ ai-session-hub@${version}`);

  if (skipped.length > 0) {
    console.log(`\n跳过的平台：${skipped.join("、")}`);
  }
  console.log(`\n产物目录：${outputDir}`);
  console.log(`主包目录：${mainPackageDir}`);
}

main().catch((error) => {
  console.error(`npm-prepare 失败：${error.message}`);
  process.exit(1);
});
