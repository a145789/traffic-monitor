import { readFileSync, writeFileSync, renameSync, mkdirSync, unlinkSync, existsSync } from "fs";
import { execSync } from "child_process";

const ISCC = "D:\\soft\\Inno Setup 7\\Inno Setup 7\\ISCC.exe";

const newVersion = process.argv[2];
if (!newVersion) {
  console.error("Usage: bun release.ts <version>");
  console.error("Example: bun release.ts 0.2.0");
  process.exit(1);
}

if (!existsSync(ISCC)) {
  console.error(`Error: Inno Setup not found at ${ISCC}`);
  process.exit(1);
}

// Check git status
console.log("Checking git status...");
const gitStatus = execSync("git status --porcelain", { encoding: "utf-8" }).trim();
if (gitStatus) {
  console.error("Error: Git working directory is not clean. Please commit or stash your changes first.");
  console.error(gitStatus);
  process.exit(1);
}

// Update Cargo.toml
console.log(`Updating Cargo.toml to ${newVersion}...`);
let cargo = readFileSync("Cargo.toml", "utf-8");
cargo = cargo.replace(/^version\s*=\s*".*"/m, `version = "${newVersion}"`);
writeFileSync("Cargo.toml", cargo);

// Update installer.iss
console.log(`Updating installer.iss to ${newVersion}...`);
let iss = readFileSync("installer.iss", "utf-8");
iss = iss.replace(/^AppVersion=.*/m, `AppVersion=${newVersion}`);
writeFileSync("installer.iss", iss);

// Cargo build
console.log("Building release...");
execSync("cargo build --release", { stdio: "inherit" });

// Inno Setup compile
console.log("Compiling installer...");
execSync(`& "${ISCC}" installer.iss`, { stdio: "inherit", shell: "powershell" });

// Rename output
const outputDir = "Output";
const outputFile = `${outputDir}\\TrafficMonitor-Setup.exe`;
const renamedFile = `${outputDir}\\TrafficMonitor-Setup-${newVersion}.exe`;

if (existsSync(outputFile)) {
  if (existsSync(renamedFile)) {
    unlinkSync(renamedFile);
  }
  renameSync(outputFile, renamedFile);
  console.log(`Renamed to ${renamedFile}`);
}

// Git commit and tag
console.log("Creating git commit and tag...");
execSync("git add Cargo.toml installer.iss", { stdio: "inherit" });
execSync(`git commit -m "release: v${newVersion}"`, { stdio: "inherit" });
execSync(`git tag v${newVersion}`, { stdio: "inherit" });

console.log(`\nRelease v${newVersion} complete!`);
console.log(`Output: ${renamedFile}`);
console.log(`\nTo push: git push && git push --tags`);
