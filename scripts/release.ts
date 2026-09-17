import { readFileSync, writeFileSync } from "fs";
import { execSync, execFileSync } from "child_process";

const newVersion = process.argv[2];
if (!newVersion) {
  console.error("Usage: bun release.ts <version>");
  console.error("Example: bun release.ts 0.2.0");
  process.exit(1);
}

// Validate semver format
const semverRegex = /^\d+\.\d+\.\d+$/;
if (!semverRegex.test(newVersion)) {
  console.error(`Error: Invalid version format "${newVersion}". Expected semver format: x.y.z`);
  process.exit(1);
}

// Check current branch
console.log("Checking current branch...");
const currentBranch = execSync("git rev-parse --abbrev-ref HEAD", { encoding: "utf-8" }).trim();
if (currentBranch !== "main") {
  console.error(`Error: Release must be run on the main branch, but current branch is "${currentBranch}".`);
  console.error("Please switch to main first: git checkout main");
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

// Check version is greater than current
console.log("Checking version increment...");
const currentVersion = readFileSync("Cargo.toml", "utf-8").match(/^version\s*=\s*"(\d+\.\d+\.\d+)"/m)?.[1];
if (!currentVersion) {
  console.error("Error: Cannot read current version from Cargo.toml");
  process.exit(1);
}
const [curMajor, curMinor, curPatch] = currentVersion.split(".").map(Number);
const [newMajor, newMinor, newPatch] = newVersion.split(".").map(Number);
if (newMajor < curMajor || (newMajor === curMajor && newMinor < curMinor) || (newMajor === curMajor && newMinor === curMinor && newPatch <= curPatch)) {
  console.error(`Error: New version (${newVersion}) must be greater than current version (${currentVersion})`);
  process.exit(1);
}

// Check tag does not exist
console.log("Checking tag does not exist...");
try {
  execSync(`git rev-parse v${newVersion}`, { encoding: "utf-8", stdio: "pipe" });
  console.error(`Error: Tag v${newVersion} already exists`);
  process.exit(1);
} catch {
  // Tag does not exist, good to proceed
}

// 三方一致校验：目标标签（须不存在）、Cargo.toml、installer.iss 任一不一致即退出，且必须在改版本之前。
console.log("Checking version consistency...");
const issVersion = readFileSync("installer.iss", "utf-8").match(/^AppVersion=(.+)$/m)?.[1]?.trim();
if (!issVersion) {
  console.error("Error: Cannot read AppVersion from installer.iss");
  process.exit(1);
}
if (issVersion !== currentVersion) {
  console.error(`Error: Version mismatch (Cargo.toml=${currentVersion}, installer.iss=${issVersion}). Align them before release.`);
  process.exit(1);
}

// 门禁必须全部在改版本之前执行：改写 Cargo.toml 版本后锁文件即过期，任何 --locked 命令必败。
// 此时锁与基线天然一致，--locked 恰好验证“发版前基线是绿的”这一本意。
// Run clippy to catch common mistakes
console.log("Running clippy...");
execSync("cargo clippy --all-targets --locked -- -D warnings", { stdio: "inherit" });

// Run tests
console.log("Running tests...");
execSync("cargo test --locked", { stdio: "inherit" });

// Check formatting
console.log("Checking formatting...");
execSync("cargo fmt -- --check", { stdio: "inherit" });

// Build release to verify baseline compiles
console.log("Building release (pre-guard)...");
execSync("cargo build --release --locked", { stdio: "inherit" });

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

// Update Cargo.lock to reflect the new version
console.log("Updating Cargo.lock...");
execSync("cargo update --workspace", { stdio: "inherit" });

// Build release again to verify updated dependencies compile
console.log("Building release (post-update verification)...");
execSync("cargo build --release --locked", { stdio: "inherit" });

// Git commit
console.log("Creating git commit...");
execSync("git add Cargo.toml installer.iss Cargo.lock", { stdio: "inherit" });
execFileSync("git", ["commit", "-m", `release: v${newVersion}`], { stdio: "inherit" });

// Create tag
console.log(`Creating tag v${newVersion}...`);
execFileSync("git", ["tag", `v${newVersion}`], { stdio: "inherit" });

// Git push commit and tag
// 单条命令一次推出本次分支与目标标签，不把本地其他无关标签推出。
// 若此步失败，不要重跑脚本（版本已改写会撞增量检查），按下方指引手工续推。
console.log("Pushing commit and tag to remote...");
try {
  execSync(`git push origin main v${newVersion}`, { stdio: "inherit" });
} catch {
  console.error(`Push failed after commit and tag v${newVersion} were created locally.`);
  console.error(`To resume, run manually: git push origin main v${newVersion}`);
  process.exit(1);
}

console.log(`\nVersion v${newVersion} released and tag pushed successfully!`);
console.log("GitHub Actions will build the binaries and publish the release.");
