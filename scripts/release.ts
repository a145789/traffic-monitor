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

// Run clippy to catch common mistakes
console.log("Running clippy...");
execSync("cargo clippy -- -D warnings", { stdio: "inherit" });

// Run tests
console.log("Running tests...");
execSync("cargo test", { stdio: "inherit" });

// Build release to verify current code compiles before modifying dependencies
console.log("Building release (pre-guard)...");
execSync("cargo build --release", { stdio: "inherit" });

// Update Cargo.lock to reflect the new version
console.log("Updating Cargo.lock...");
execSync("cargo update --workspace", { stdio: "inherit" });

// Build release again to verify updated dependencies compile
console.log("Building release (post-update verification)...");
execSync("cargo build --release", { stdio: "inherit" });

// Git commit
console.log("Creating git commit...");
execSync("git add Cargo.toml installer.iss Cargo.lock", { stdio: "inherit" });
execFileSync("git", ["commit", "-m", `release: v${newVersion}`], { stdio: "inherit" });

// Create tag
console.log(`Creating tag v${newVersion}...`);
execFileSync("git", ["tag", `v${newVersion}`], { stdio: "inherit" });

// Git push commit and tag
console.log("Pushing commit and tag to remote...");
execSync("git push && git push --tags", { stdio: "inherit" });

console.log(`\nVersion v${newVersion} released and tag pushed successfully!`);
console.log("GitHub Actions will build the binaries and publish the release.");
