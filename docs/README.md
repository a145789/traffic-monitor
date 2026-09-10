# docs

本目录中 [`rfc/`](rfc/)（现行提案）与历史备份性质不同：前者 AI 可读且应读，后者默认禁读。详见仓库根目录 [AGENTS.md](../AGENTS.md) 的「文档阅读策略」。

## 布局

| 路径 | 含义 |
| :--- | :--- |
| [`rfc/`](rfc/) | **现行提案**：尚未实施与刚实施完的 RFC 笔记，按会话分目录。约定见其 [`README.md`](rfc/README.md)。 |
| [`archive/`](archive/) | 历史 backup：RFC、审计、研究笔记、旧实现说明、unsafe policy 全文等。**可能过时，勿当现行规范。** |

## 现行规范在哪

- **给 AI / 开发者的硬约束**：[`AGENTS.md`](../AGENTS.md)
- **用户说明**：[`README.md`](../README.md)
- **行为真相**：`src/**`

需要翻历史时，直接打开 `archive/` 下对应文件，或对 AI 说清路径（例如「读 `docs/archive/rfc/0006-auto-update.md`」）。

需要查「现在打算改什么」时看 [`rfc/`](rfc/)；查待办：`grep -rn "^Status: *proposed" docs/rfc/`。
