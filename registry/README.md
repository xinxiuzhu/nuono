# Nuono 官方扩展目录

本目录维护 Nuono 商店默认拉取的业务插件（`plugin`）与平台适配器（`adapter`）仓库清单，直接随主仓库管理，无需维护单独的注册表仓库。

## 提交流程

1. 在本仓库使用「**提交插件或适配器**」Issue 模板填写公开 GitHub 仓库地址（展示名称与简介可选）。
2. 维护者审核源码确认无误后，为该 Issue 添加 `extension:approved` 标签。
3. GitHub Actions（[.github/workflows/extension-catalog.yml](../.github/workflows/extension-catalog.yml)）会自动拉取仓库并从 `Cargo.toml` 提取扩展类型、ID、版本号、作者、开源协议与最新提交，写入 `registry/entries/`，并生成 `extensions.json` 发布到本仓库的 `catalog` 分支。
4. 扩展仓库后续推送新版本后，定时任务也会自动同步最新版本号与提交到商店目录，无需重复提交。

## 登记条目格式

每个条目位于 `registry/entries/`：
- 业务插件：`<id>.json`
- 平台适配器：`adapter.<id>.json`

最简条目只需要仓库地址，其余元信息在构建目录时自动从仓库的 `Cargo.toml` 提取：

```json
{
  "repo": "https://github.com/alice/example.git"
}
```

如需覆盖展示名称、简介或临时锁定到指定提交，可按需添加可选字段：

```json
{
  "repo": "https://github.com/alice/example.git",
  "name": "示例插件",
  "description": "一句话说明功能",
  "commit": "0123456789abcdef0123456789abcdef01234567"
}
```

## 本地命令

```bash
# 离线校验全部登记条目格式
cargo xtask registry validate

# 拉取各登记仓库最新元数据并生成目录文件到指定目录
cargo xtask registry build --out target/catalog-dist
```
