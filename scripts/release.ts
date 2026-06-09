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

// Build release to verify everything compiles
console.log("Building release (verification)...");
execSync("cargo build --release", { stdio: "inherit" });

// Create tag and push
console.log(`Creating tag v${newVersion}...`);
execSync(`git tag v${newVersion}`, { stdio: "inherit" });

console.log("Pushing tag to remote...");
execSync("git push --tags", { stdio: "inherit" });

console.log(`\nTag v${newVersion} pushed successfully!`);
console.log("GitHub Actions will update versions, build the binaries, and publish the release.");
