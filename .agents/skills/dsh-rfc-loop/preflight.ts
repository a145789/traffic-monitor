#!/usr/bin/env bun
/**
 * preflight.ts — DSH RFC 闭环（dsh-rfc-loop）开工前预检。
 *
 * 只读：不写工作区、不改任何 ref、不跑 cargo。唯一的写动作是在系统临时目录里
 * 建/删自己的抓取文件（见 git() 的注释）。
 *
 * 用法：
 *   bun .agents/skills/dsh-rfc-loop/preflight.ts
 *   bun .agents/skills/dsh-rfc-loop/preflight.ts --repo D:\path\to\repo
 *
 * 输出为可解析的 KEY=VALUE 行；`PREFLIGHT: OK` / `PREFLIGHT: STOP` 是唯一判据。
 * 退出码：0 = OK（可以派写手）；2 = STOP（原样交给用户，不要自行修复）。
 *
 * 维护约定：新增检查项时保持只读；硬失败进 STOPS，软风险进 WARNS。
 */
import {
  closeSync,
  existsSync,
  openSync,
  readFileSync,
  readdirSync,
  statSync,
  unlinkSync,
} from "node:fs";
import { basename, dirname, join, resolve } from "node:path";
import { tmpdir } from "node:os";

const STOP_EXIT = 2;

const stops: string[] = [];
const warns: string[] = [];
const infos: string[] = [];

// ---------- 0. 定位仓库 ----------
const repoFlagIndex = process.argv.indexOf("--repo");
const repoRoot = resolve(
  repoFlagIndex >= 0 && process.argv[repoFlagIndex + 1]
    ? process.argv[repoFlagIndex + 1]
    : join(import.meta.dir, "..", "..", ".."),
);

const normalize = (p: string) => resolve(p).replace(/[\\/]+$/, "").toLowerCase();

/**
 * 跑一条 git 命令并取回它的输出。
 *
 * 为什么不是 stdout: "pipe"：本脚本运行在 DSH 的受限文件沙箱下，该沙箱明确
 * 禁止子进程通过管道捕获输出（Node/Bun 的默认 stdio: 'pipe' 会 EPERM）。
 * 因此这里把子进程的 stdout/stderr 重定向到临时文件再读回——文件重定向不走管道，
 * 在受限与不受限环境下都成立。抓取文件只落在系统临时目录，用完即删。
 */
let gitSeq = 0;
function git(args: string[]): { code: number; out: string } {
  const captureFile = join(tmpdir(), `dsh-rfc-loop-preflight-${process.pid}-${gitSeq++}.log`);
  let code = 1;
  let spawnError = "";
  let fd = -1;
  try {
    fd = openSync(captureFile, "w");
  } catch (err) {
    return { code: 127, out: `cannot create capture file: ${String(err)}` };
  }
  try {
    const proc = Bun.spawnSync({
      cmd: ["git", "-C", repoRoot, ...args],
      stdin: "ignore",
      stdout: fd,
      stderr: fd,
    });
    code = proc.exitCode ?? 1;
  } catch (err) {
    code = 127;
    spawnError = String(err);
  } finally {
    closeSync(fd);
  }
  let out = "";
  try {
    out = readFileSync(captureFile, "utf8");
  } catch {
    /* 读不回来就当空输出，由 code 决定成败 */
  }
  try {
    unlinkSync(captureFile);
  } catch {
    /* 临时文件删不掉不影响判定 */
  }
  if (spawnError) out = spawnError;
  return { code, out };
}

const firstLine = (text: string): string => (text.split(/\r?\n/)[0] ?? "").trim();
const nonEmptyLines = (text: string): string[] =>
  text
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line !== "");

function walkFiles(dir: string, predicate: (name: string) => boolean, out: string[] = []): string[] {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) walkFiles(full, predicate, out);
    else if (entry.isFile() && predicate(entry.name)) out.push(full);
  }
  return out;
}

const relFromRoot = (absolute: string): string =>
  absolute.slice(repoRoot.length).replace(/^[\\/]+/, "").replace(/\\/g, "/");

// ---------- 1. git 可用，且脚本根 == 仓库根 ----------
const gitVersion = git(["--version"]);
if (gitVersion.code !== 0) {
  stops.push("git not runnable (is it on PATH?)");
} else {
  const top = git(["rev-parse", "--show-toplevel"]);
  if (top.code !== 0) {
    stops.push(`not a git repository: ${repoRoot}`);
  } else if (normalize(firstLine(top.out)) !== normalize(repoRoot)) {
    stops.push(`git top-level (${firstLine(top.out)}) != script root (${repoRoot})`);
  }
}

if (!existsSync(join(repoRoot, "Cargo.toml"))) {
  stops.push("Cargo.toml not found at repo root (wrong repo?)");
}

// ---------- 2. 没有进行中的 git 操作 ----------
const gitDir = join(repoRoot, ".git");
const gitDirIsFile = existsSync(gitDir) && statSync(gitDir).isFile();
if (gitDirIsFile) {
  warns.push(".git is a file (linked worktree / submodule); in-progress operation markers were not checked");
} else if (!existsSync(gitDir)) {
  stops.push(".git not found at repo root");
} else {
  for (const marker of [
    "rebase-merge",
    "rebase-apply",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
  ]) {
    if (existsSync(join(gitDir, marker))) stops.push(`git operation in progress: .git/${marker}`);
  }
}

// ---------- 3. 在 main 上 ----------
const branch = firstLine(git(["rev-parse", "--abbrev-ref", "HEAD"]).out);
if (branch !== "main") stops.push(`current branch is '${branch || "<unknown>"}', expected 'main'`);

// ---------- 4. 记录 base sha ----------
const baseSha = firstLine(git(["rev-parse", "HEAD"]).out);
if (!/^[0-9a-f]{40}$/i.test(baseSha)) stops.push("cannot resolve HEAD");

// ---------- 5. 工作区干净（含未跟踪文件） ----------
const statusLines = nonEmptyLines(git(["status", "--porcelain"]).out);
if (statusLines.length > 0) {
  stops.push(`working tree not clean: ${statusLines.length} entries`);
  for (const line of statusLines.slice(0, 20)) infos.push(`dirty: ${line}`);
}

// ---------- 6. 只有这一个 worktree ----------
const worktreeCount = nonEmptyLines(git(["worktree", "list", "--porcelain"]).out).filter((line) =>
  line.startsWith("worktree "),
).length;
if (worktreeCount > 1) warns.push(`more than one worktree registered (${worktreeCount})`);

// ---------- 7. 待办笔记 ----------
const rfcRoot = join(repoRoot, "docs", "rfc");
const archiveRoot = join(repoRoot, "docs", "archive", "rfc");
const todos: string[] = [];
const sessionDirs: string[] = [];

if (!existsSync(rfcRoot)) {
  stops.push("docs/rfc not found");
} else {
  try {
    const mdFiles = walkFiles(rfcRoot, (name) => name.toLowerCase().endsWith(".md")).sort();
    for (const file of mdFiles) {
      const rel = relFromRoot(file);
      // 只认 docs/rfc/<会话目录>/ 下的笔记；README.md 等根说明文件不是待办
      if (!/^docs\/rfc\/[^/]+\/.+\.md$/i.test(rel)) continue;
      const text = readFileSync(file, "utf8");
      const proposed = text
        .split(/\r?\n/)
        .some((line) => /^Status:\s*proposed\b/i.test(line.trim()));
      if (!proposed) continue;

      todos.push(rel);

      const session = basename(dirname(file));
      if (!sessionDirs.includes(session)) sessionDirs.push(session);

      for (const heading of ["提案", "明确不在本次范围", "验收标准"]) {
        if (!text.includes(heading)) warns.push(`note lacks section '${heading}': ${rel}`);
      }
    }
  } catch (err) {
    stops.push(`failed to scan docs/rfc: ${String(err)}`);
  }
  if (todos.length === 0 && !stops.some((s) => s.startsWith("failed to scan"))) {
    stops.push('no RFC note with "Status: proposed" found (nothing to do)');
  }
}

for (const session of sessionDirs) {
  if (!existsSync(join(archiveRoot, session))) {
    warns.push(`archive dir does not exist yet (reviewer lands there): docs/archive/rfc/${session}`);
  }
}

// ---------- 8. 账本目录必须已被 git 忽略 ----------
// 账本写在仓库根下，靠 .gitignore 保证它既不进提交、也不会让「工作区干净」这条预检失真。
const ledgerRel = ".dsh-rfc-loop-temp/ledger.md";
const ignoreCheck = git(["check-ignore", "-q", "--", ledgerRel]);
if (ignoreCheck.code !== 0) {
  stops.push(
    ".dsh-rfc-loop-temp/ is not git-ignored: add '.dsh-rfc-loop-temp/' to .gitignore and commit it " +
      "(a stray ledger dir would otherwise break the clean-tree precondition)",
  );
}
if (existsSync(join(repoRoot, ".dsh-rfc-loop-temp", "ledger.md"))) {
  infos.push(`ledger exists: ${ledgerRel} (check it for unfinished RFCs before starting)`);
}

// ---------- 输出 ----------
if (stops.length > 0) {
  console.log("PREFLIGHT: STOP");
  for (const stop of stops) console.log(`STOP: ${stop}`);
  for (const warn of warns) console.log(`WARN: ${warn}`);
  for (const info of infos) console.log(`INFO: ${info}`);
  process.exit(STOP_EXIT);
}

console.log("PREFLIGHT: OK");
console.log(`REPO=${repoRoot}`);
console.log(`BRANCH=${branch}`);
console.log(`BASE_SHA=${baseSha}`);
console.log(`TODO_COUNT=${todos.length}`);
todos.forEach((todo, index) => console.log(`TODO[${index + 1}]=${todo}`));
for (const session of sessionDirs) console.log(`SESSION_DIR=${session}`);
console.log(`LEDGER=${ledgerRel}`);

if (todos.length > 0) {
  const slug = basename(todos[0]).replace(/\.md$/i, "");
  console.log(`NEXT_BRANCH=rfc/${slug}`);
}

for (const warn of warns) console.log(`WARN: ${warn}`);
for (const info of infos) console.log(`INFO: ${info}`);

console.log("--- paste into ledger ---");
console.log("# dsh-rfc-loop ledger");
console.log(`- base: ${baseSha}`);
console.log(`- started from branch: ${branch}`);
for (const todo of todos) {
  console.log(`- todo: ${todo} -> rfc/${basename(todo).replace(/\.md$/i, "")}`);
}

process.exit(0);
