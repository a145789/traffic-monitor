import { readFileSync, writeFileSync, existsSync, mkdirSync } from "fs";
import { execSync } from "child_process";

const tag = process.argv[2] || "";

const cargoToml = readFileSync("Cargo.toml", "utf-8");
const issFile = readFileSync("installer.iss", "utf-8");
// dev 打包改版本后构建会同步改写 Cargo.lock 中的根包版本，提前保存以便恢复，避免时间戳版本脏改被误提交。
const cargoLock = existsSync("Cargo.lock") ? readFileSync("Cargo.lock", "utf-8") : null;

const versionMatch = cargoToml.match(/^version\s*=\s*"(.+)"/m);
if (!versionMatch) {
  console.error("Cannot read version from Cargo.toml");
  process.exit(1);
}
const baseVersion = versionMatch[1];
let taggedVersion = baseVersion;
if (tag) {
  const ts = Math.floor(Date.now() / 1000).toString(36).slice(-6);
  taggedVersion = `${baseVersion}-${tag}${ts}`;
}

function findISCC(): string | null {
  // 仅查系统标准安装路径 + PATH（where 兜底）：个人机器路径禁止入库。
  const candidates = [
    "C:\\Program Files\\Inno Setup 7\\ISCC.exe",
    "C:\\Program Files (x86)\\Inno Setup 7\\ISCC.exe",
  ];
  for (const p of candidates) {
    if (existsSync(p)) return p;
  }
  try {
    return execSync("where ISCC.exe", { encoding: "utf-8" }).trim().split("\n")[0];
  } catch {
    return null;
  }
}

const iscc = findISCC();
if (!iscc) {
  console.error("Inno Setup not found. Install it from https://jrsoftware.org/isinfo.php");
  process.exit(1);
}

if (tag) {
  console.log(`Patching version: ${baseVersion} → ${taggedVersion}`);
  writeFileSync("Cargo.toml", cargoToml.replace(/^version\s*=\s*".*"/m, `version = "${taggedVersion}"`));
  writeFileSync("installer.iss", issFile.replace(/^AppVersion=.*/m, `AppVersion=${taggedVersion}`));
}

try {
  console.log("Building release...");
  // TRAFFIC_MONITOR_DEV_BUILD 是「开发版」判定的唯一来源（build.rs 注入 →
  // src/config.rs 的 DEV_BUILD）：这一行不可删，删掉后 dev 包会在手动检查更新时
  // 谎称「当前已是最新版本」。带 tag 的打包一律视为非发布构建，不带 tag 的常规
  // 打包保持和 CI 一致：既注入标记，也要主动清掉外部环境里同名的变量，
  // 否则 shell 里 export 过它的人会打出一个自称开发版的常规包。
  const buildEnv = { ...process.env };
  if (tag) {
    buildEnv.TRAFFIC_MONITOR_DEV_BUILD = "1";
  } else {
    delete buildEnv.TRAFFIC_MONITOR_DEV_BUILD;
  }
  execSync("cargo build --release", { stdio: "inherit", env: buildEnv });

  if (!existsSync("Output")) {
    mkdirSync("Output");
  }

  console.log("Compiling installer...");
  execSync(`& "${iscc}" installer.iss`, { stdio: "inherit", shell: "powershell" });

  console.log(`\nDone! Installer: Output\\TrafficMonitor-Setup-${taggedVersion}.exe`);
  if (tag) console.log(`Version: ${taggedVersion}`);
} finally {
  if (tag) {
    console.log(`Restoring version: ${baseVersion}`);
    writeFileSync("Cargo.toml", cargoToml);
    writeFileSync("installer.iss", issFile);
    // 构建成功/失败都走这里，把 Cargo 同步改写的根包版本一并还原。
    if (cargoLock !== null) {
      writeFileSync("Cargo.lock", cargoLock);
    }
  }
}
