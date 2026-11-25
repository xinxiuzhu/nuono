# Nuono 官方扩展目录

本目录维护 Nuono 商店默认拉取的业务插件（`plugin`）与平台适配器（`adapter`）登记条目，直接随主仓库管理，无需维护单独的注册表仓库。

## 提交流程

1. 在本仓库使用「**提交插件或适配器**」Issue 模板填写 GitHub 仓库、版本号、完整 40 位 Commit SHA 与简介。
2. 维护者审核源码确认无误后，为该 Issue 添加 `extension:approved` 标签。
3. GitHub Actions（[.github/workflows/extension-catalog.yml](../.github/workflows/extension-catalog.yml)）会自动校验条目、写入 `registry/entries/`，并生成 `extensions-v2.json` 与 `catalog.json` 发布到本仓库的 `catalog` 分支。

## 条目格式

每个条目位于 `registry/entries/`：
- 业务插件：`<id>.json`
- 平台适配器：`adapter.<id>.json`

```json
{
  "kind": "plugin",
  "id": "example",
  "name": "Example Plugin",
  "version": "0.1.0",
  "description": "一句话说明功能",
  "author": "alice",
  "license": "Apache-2.0",
  "nuono_version": "0.0.1",
  "repo": "https://github.com/alice/example.git",
  "commit": "0123456789abcdef0123456789abcdef01234567"
}
```

## 本地命令

```bash
# 校验全部条目
cargo xtask registry validate

# 生成目录文件到指定目录
cargo xtask registry build --out target/catalog-dist
```
