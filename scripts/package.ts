import { readFileSync, writeFileSync, existsSync, mkdirSync } from "fs";
import { execSync } from "child_process";

const tag = process.argv[2] || "";

const cargoToml = readFileSync("Cargo.toml", "utf-8");
const issFile = readFileSync("installer.iss", "utf-8");

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
  const candidates = [
    "D:\\soft\\Inno Setup 7\\Inno Setup 7\\ISCC.exe",
    "C:\\Program Files (x86)\\Inno Setup 7\\ISCC.exe",
    "C:\\Program Files\\Inno Setup 7\\ISCC.exe",
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
  execSync("cargo build --release", { stdio: "inherit" });

  if (!existsSync("Output")) {
    mkdirSync("Output");
  }

  console.log("Compiling installer...");
  execSync(`& "${iscc}" installer.iss`, { stdio: "inherit", shell: "powershell" });

  console.log(`\nDone! Installer: Output\\TrafficMonitor-Setup.exe`);
  if (tag) console.log(`Version: ${taggedVersion}`);
} finally {
  if (tag) {
    console.log(`Restoring version: ${baseVersion}`);
    writeFileSync("Cargo.toml", cargoToml);
    writeFileSync("installer.iss", issFile);
  }
}
