#!/usr/bin/env node
// 从工作目录内的 Release 资产生成 Tauri updater 所需的 latest.json。
//
// 输入约定（build.yml 的 Release job 会在 download-artifact 后调用本脚本）：
// - 各平台构建产物（含 .sig 签名文件）已合并到同一目录；
// - 环境变量 GITHUB_REPOSITORY / GITHUB_REF_NAME 由 GitHub Actions 提供，
//   本地调试时可显式传入 repo 与 tag 参数。
//
// 平台映射（按 .sig 文件名匹配，url 指向同 Release 的对应产物）：
//   *_aarch64.app.tar.gz.sig    -> darwin-aarch64   （macOS arm64 热更包，由 CI 重签后改名）
//   *_x86_64.app.tar.gz.sig     -> darwin-x86_64    （macOS x64 热更包）
//   *_x64-setup.exe.sig         -> windows-x86_64   （NSIS 安装器，per-user 静默重装）
//   *_arm64-setup.exe.sig       -> windows-aarch64
//   *_amd64.AppImage.sig        -> linux-x86_64     （AppImage 自替换，无 root）
//   *_arm64.AppImage.sig        -> linux-aarch64
//
// 注意：latest.json 只输出这 6 个平台；Windows 更新包经 minisign 校验后由 NSIS
// 安装器完成重装，DEB/RPM 不纳入热更（需要 root，/Linux 上由用户自行跳转到 Release）。

import { readFileSync, readdirSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const dir = process.argv[2] ?? "release-assets";
const out = process.argv[3] ?? join(dir, "latest.json");
const repo = process.env.GITHUB_REPOSITORY ?? "jockiller/dsh-launcher";
const rawTag = process.env.GITHUB_REF_NAME ?? process.argv[4] ?? "";
const version = rawTag.replace(/^v/i, "");

if (!/^\d+\.\d+\.\d+/.test(version)) {
  console.error(`无法从标签解析版本号："${rawTag}"`);
  process.exit(1);
}

const ROUTES = [
  [/_aarch64\.app\.tar\.gz\.sig$/, "darwin-aarch64"],
  [/_x86_64\.app\.tar\.gz\.sig$/, "darwin-x86_64"],
  [/_x64-setup\.exe\.sig$/, "windows-x86_64"],
  [/_arm64-setup\.exe\.sig$/, "windows-aarch64"],
  [/_amd64\.AppImage\.sig$/i, "linux-x86_64"],
  // 注意：Tauri 对 AppImage 的 arm64 产物命名为 _aarch64（实测 v0.9.0 CI 日志），
  // Windows NSIS 才是 _arm64；两条路由并存以防命名口径变化。
  [/_aarch64\.AppImage\.sig$/i, "linux-aarch64"],
  [/_arm64\.AppImage\.sig$/i, "linux-aarch64"],
];

function collectFiles(root) {
  const files = [];
  for (const entry of readdirSync(root)) {
    const path = join(root, entry);
    if (statSync(path).isDirectory()) {
      files.push(...collectFiles(path));
    } else {
      files.push(path);
    }
  }
  return files;
}

const platformsWithSig = new Map();
for (const path of collectFiles(dir).sort()) {
  const name = path.split("/").pop() ?? path;
  const route = ROUTES.find(([pattern]) => pattern.test(name));
  if (!route) continue;
  const [, platform] = route;
  if (platformsWithSig.has(platform)) {
    console.error(`平台 ${platform} 出现重复签名文件：${name}`);
    process.exit(1);
  }
  // GitHub 上传 Release 资产时会把文件名中的空格替换为点号（DSH Launcher_x.exe →
  // DSH.Launcher_x...），latest.json 的 URL 必须与上传后的资产名一致，否则 404。
  // 构建侧已负责把产物改名（空格→下划线/点），这里对剩余空格做同样归一化兜底。
  const assetName = basenameWithoutSig(name).replaceAll(" ", ".");
  const url = `https://github.com/${repo}/releases/download/${rawTag}/${assetName}`;
  const signature = readFileSync(path, "utf8").trim();
  if (!signature) {
    console.error(`签名文件为空：${path}`);
    process.exit(1);
  }
  platformsWithSig.set(platform, { signature, url });
}

const missing = ROUTES.map(([, platform]) => platform).filter((p) => !platformsWithSig.has(p));
if (missing.length > 0) {
  console.error(`缺少以下平台的更新产物（.sig）：${missing.join(", ")}`);
  process.exit(1);
}

const latest = {
  version,
  notes: process.env.LATEST_NOTES ?? "",
  pub_date: new Date().toISOString(),
  platforms: Object.fromEntries(platformsWithSig),
};

writeFileSync(out, JSON.stringify(latest, null, 2) + "\n");
console.log(`已生成 ${out}（${Object.keys(latest.platforms).length} 个平台，版本 ${version}）`);
for (const [platform, info] of Object.entries(latest.platforms)) {
  console.log(`  ${platform} -> ${info.url}`);
}

/** 签名文件名去掉 .sig 后即为对应更新包的资产名。 */
function basenameWithoutSig(name) {
  return name.slice(0, -".sig".length);
}