# Agent Note：加固发布工作流（draft 发布、去 clobber、tag 走 env、失败可恢复）

Status: proposed

本 note 只含**机械性**的发布面加固；README 的镜像披露拆到 10 篇（那是产品口径，需要所有者定稿，与 workflow 改动不是同一类工作）。

## 问题

**其一，先发布后补资产，而 release 一创建就是 `latest`。** `.github/workflows/release.yml:176-195` 的顺序是：`gh release create --latest`（`:181-190`）→ `gh release upload --clobber`（`:191-195`）。资产在 release 已经是 latest 之后才逐个上传，而客户端拉的是 `releases/latest/download/version.txt`（`src/update/mod.rs:61-65`）。

**旧稿在这里有一条错误论断，必须改掉**：旧稿称「`version.txt` 恰好是三个资产里最后上传的那个，所以『看到新版元数据但资产缺失』的窗口实际为零」。`gh release upload` **不是**按参数顺序串行上传：`cli/cli` 的 `pkg/cmd/release/upload/upload.go` 调用 `shared.ConcurrentUpload(httpClient, host, uploadURL, opts.Concurrency, opts.Assets)`，而 `NewCmdUpload` 把 `opts.Concurrency` 固定为 `5`。并发上传下，只有几十字节的 `version.txt` 很可能**先**落地，于是真实存在一个「客户端已读到新版本号、安装包仍在传」的 404 窗口。窗口很短，客户端也有重试与错误冷却（`src/update/mod.rs:209-221`、`:172-178`），所以这不是安全事故；但它说明「靠上传顺序保证原子性」这个论证不成立，发布的完整性必须由流程而非运气保证。

**其二，`--clobber` 把一个已发布的 tag 的资产变成可静默替换。** 同一条 `gh` 命令的官方说明原文是：使用 `--clobber` 时**先删除**同名既有资产再上传新资产，**若上传失败，原资产会丢失**。在**没有签名**的前提下（`.github/workflows/release.yml` 全流程无 signtool/WinVerifyTrust，README 也无签名说明），这意味着「tag 内容可变」这件事没有任何可核验的锚点，而且失败时还会毁掉能用的旧资产。

**其三，tag 直接拼进 `run:` 脚本。** `release.yml:121` 写 `VERSION="${{ steps.version.outputs.VERSION }}"`，`:181-193` 多处直接插入 `${{ github.ref_name }}`。这是 GitHub Actions 官方点名的脚本注入反模式。公平地说，同一文件的版本一致性校验步骤反而刻意走 `env:`（`:30-32`）并注明「避免 tag 特殊字符造成 script-injection」——作者知道这个坑，只是只在那一处应用了。当前它不可直接利用：`:28-40` 的校验要求 tag（去 `v` 前缀）与 `Cargo.toml`、`installer.iss` 三方全等，恶意 tag 会先让校验失败、后续步骤不执行。但 `release.yml:8-9` 是 `contents: write`，一旦将来调整门禁顺序或放宽一致校验，这条就升级为可写仓库的注入面。

**其四，`gh release create` 没有 `--verify-tag`。** 缺了它时，若 tag 不存在，`gh` 会**从默认分支新建**一个同名 tag 再发布——发布一个与 `Cargo.toml`/`installer.iss` 对应的提交无关的 tag，正是「三方版本一致」校验想防的事。

**其五，去掉 `--clobber` 之后没有任何恢复设计。** 现状是「重跑即覆盖」，去掉它以后，失败重跑会因为「同名资产已存在」直接报错（`gh` 的行为就是在任何上传之前先检查并拒绝），于是需要显式的修复入口，否则一次网络抖动就让人卡在手工删资产上。

生产消费者：以上全在发布路径上；客户端侧只通过 `releases/latest/download/*` 受影响（`src/update/mod.rs:61-65`、`:275-283`）。非生产消费者：`docs/` 下的记录不构成对用户的披露。

## 提案

1. **Draft → 上传全部资产 → 校验 → Publish。** `release.yml:181-190` 的创建改为 `gh release create <tag> --draft --verify-tag --title ... --notes-file ...`（**不带** `--latest`）；`:191-195` 上传三个资产；新增一步校验：三个资产都存在、`sha256sum` 与 `version.txt` 内容一致、`version.txt` 首行等于 `Cargo.toml` 版本；最后 `gh release edit <tag> --draft=false --latest`。
2. **默认路径去掉 `--clobber`，把「重建同一 tag」变成显式意图。** 新增 `workflow_dispatch` 输入（例如 `repair: true`）或在 workflow 里新增独立的 repair 入口，只有该入口才允许 `--clobber`；默认 `push tag` 路径下，同名资产已存在应当**失败**而不是静默覆盖。
3. **失败恢复写清楚**（这是旧稿缺的一节）：某一轮失败会留下一个 draft + 部分资产。重跑时默认路径仍会因资产已存在而失败，因此实施时要加一步「按需清理」：读取 `gh release view <tag> --json assets`，只删除**本次将要重传的同名资产**（`gh release delete-asset <tag> <name> -y`）后重传，其余资产不动；这一步只在 `repair` 意图下执行，避免默认路径获得覆盖能力。
4. **tag / 版本号全部经 `env:` 传入**，照抄 `release.yml:30-32` 的既有写法；`:121` 与 `:181-193` 的 `${{ }}` 内插一并清掉（`github.ref_name` 改由 `env: TAG: ${{ github.ref_name }}` 或既有的 `GITHUB_REF_NAME` 环境变量取得，后者连 `env:` 都不需要）。
5. **预发布试跑的边界写清楚**：`--latest` 与 `--prerelease` 是两套语义，**不要把「预发布 tag 发布后应成为 latest」写成验收**；验收试跑要在 fork 仓库上做，或在完成后删除试跑的 tag 与 release（GitHub 的 latest 指针由最近一次非 prerelease 的正式发布决定，试跑一个正式 tag 会污染它）。

## 明确不在本次范围

- **不加 Authenticode 签名**：项目所有者明确不为此付费，且这是已记录的产品决定。本 note 只消除「同一 tag 资产可静默替换」这一在无签名前提下才显得危险的自由度。
- **不加 `--prerelease` 自动判定**：什么时候发预发布是产品决策，不由 workflow 猜版本号后缀（见 05 篇：`scripts/package.ts` 接受任意 tag，靠后缀判定会误伤）。
- **不改 `v*` 的 tag 触发方式，不改 `:28-40` 的三方版本一致校验**（它是当前拦住 tag 注入的实际门禁）。
- **不改客户端侧**：不引入「校验发布者」、不改哈希校验方式（`src/update/cache.rs:25-30` 的锁定句柄重验已是既有正确做法）。
- **不在本 note 里加镜像披露**：见 10 篇（产品口径需所有者定稿，与机械加固拆开评审）。
- **不改 `release.yml:46-47` 只构建不跑测试的现状**（那是 CI 覆盖面问题，属另一类讨论）。

## 为什么不保留？

1. 「上传窗口只有几秒，后果只是 404 重试」——成立，本 note 也没有把它列为安全事故，而是列为**发布可核验性**：Draft→Publish 让「资产齐了才算发布」成为流程事实。旧稿用错误的上传顺序论证来把它降级成「窗口为零」，这条论证必须撤回。
2. 「tag 注入有版本校验挡着」——依赖的是另一段脚本的执行顺序与严格性，属纵深防御的经典「现在没事」论证；把 `${{ }}` 换成 `env:`/`GITHUB_REF_NAME` 的成本是几行，收益是让这段逻辑不再依赖「门禁恰好很严」。
3. 「`--clobber` 是为了重跑修复发布」——重跑修复是真实需求，但 `gh` 自己的文档写着它会**先删后传、失败即丢失**；它应该是一个需要显式意图的入口，而不是默认路径的副作用。
4. 「`--verify-tag` 没意义，workflow 本来就是 tag push 触发的」——`workflow_dispatch` 与将来可能的分支触发都会走到同一段脚本；加一个参数就把「发布一个不存在的 tag 时自动建在默认分支」这条路径关掉，成本为零。
5. 「失败恢复自动化太重，人工删资产就行」——人工步骤正是现在被诟病的那种「没有写在流程里、只有当事人知道」的隐性知识；把 `repair` 入口写成显式输入，比在文档里记一句话更不容易失传。

## 验收标准

- `grep -n "gh release create" .github/workflows/release.yml` 命中行同时含 `--draft` 与 `--verify-tag`；`grep -n "release edit" .github/workflows/release.yml` 命中 1 处且含 `--draft=false` 与 `--latest`。
- `grep -n "clobber" .github/workflows/release.yml` 命中 0 处，或仅出现在 `repair` 显式入口所覆盖的步骤内（人工核对：默认 push-tag 路径不带该参数）。
- `grep -n '\${{ github.ref_name }}\|\${{ steps.version.outputs.VERSION }}' .github/workflows/release.yml` 只出现在 `env:` 段内，或已改用 `GITHUB_REF_NAME` 环境变量，`run:` 脚本体内 0 处。
- 失败恢复可执行：人为让一次上传失败（例如临时把某个资产路径改错），确认留下的是 draft + 部分资产、`releases/latest/download/version.txt` 仍指向上一版本；再用 `repair` 入口重跑一次，确认能补齐三个资产并 publish。
- 走一次完整发布（**在 fork 仓库上**，或在完成后删除试跑 tag 与 release）：Draft 期间 `releases/latest/download/version.txt` 仍指向上一版本，Publish 后指向新版本，且三个资产齐备、`version.txt` 的哈希与安装包实际哈希一致。
- 不把「预发布 tag 发布后成为 latest」写入任何验收项（GitHub 语义不同）。

## 风险

- Draft 流程会让「重跑修复资产」多一步手动 publish；若有人依赖「重推 tag 自动修复」，需要同步更新发布文档（`AGENTS.md` 的「安装包与发布」一节只描述了 `bun scripts/release.ts`，未描述 workflow 内部顺序，实施时确认是否要补一句）。
- `gh` 版本的 `--draft`/`--verify-tag`/`release edit --draft=false --latest` 组合在 runner 上的实际行为需要实测（`ubuntu-latest`/`windows-latest` 预装的 `gh` 版本可能不同）；若 `release edit` 不接受同时设置这两个标志，退路是分两步：先 `--draft=false`，再 `--latest`。
- 去掉 `--clobber` 后，**任何**因为资产名不同而残留的旧资产（例如历史命名）都会让重跑失败；`repair` 入口的清理范围因此必须精确到「本次要重传的同名资产」，不能写成「清空全部资产」。
- 预发布试跑若在正式仓库执行且事后忘记删除 tag，会永久留下一个与代码无关的 tag；这一条只能靠流程纪律，本 note 无技术手段强制。
