import { readFileSync, writeFileSync } from "fs";
import { execSync } from "child_process";

const newVersion = process.argv[2];
if (!newVersion) {
  console.error("Usage: bun release.ts <version>");
  console.error("Example: bun release.ts 0.2.0");
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

// Build release to verify everything compiles
console.log("Building release...");
execSync("cargo build --release", { stdio: "inherit" });

// Git commit and tag
console.log("Creating git commit and tag...");
execSync("git add Cargo.toml installer.iss Cargo.lock", { stdio: "inherit" });
execSync(`git commit -m "release: v${newVersion}"`, { stdio: "inherit" });
execSync(`git tag v${newVersion}`, { stdio: "inherit" });

// Git push
console.log("Pushing to remote...");
execSync("git push && git push --tags", { stdio: "inherit" });

// Print success message
console.log(`\nTag v${newVersion} pushed successfully!`);
console.log("GitHub Actions will automatically create the Release, build the binaries, and upload the assets.");

