# 发布流水线说明

## 触发方式

- 自动触发：`push tag`，匹配 `v*`
- 手动触发：GitHub Actions 页面 `Release` -> `Run workflow`

## 流水线行为

`release.yml` 会执行：

1. 运行集成测试（`pytest -q tests/integration`）
2. 构建 Python 发布包（wheel + sdist）
3. 打包 npm tarball
4. 构建 Rust `alsp_adapter` 三平台二进制（Linux/Windows/macOS）
5. 若为 tag 触发，自动创建 GitHub Release 并上传所有产物
6. 若配置了密钥，发布到 PyPI 与 npm

## 必要密钥（Repository Secrets）

- `PYPI_API_TOKEN`：用于发布 Python 包
- `NPM_TOKEN`：用于发布 npm 包

## 建议发布步骤

1. 更新版本号（`pyproject.toml` 与 `package.json`）。
2. 提交并推送 `main`。
3. 打 tag 并推送：
   - `git tag v0.1.0`
   - `git push origin v0.1.0`
4. 在 Actions 页面确认 `Release` 工作流完成。

## 备注

- 未配置 `PYPI_API_TOKEN`/`NPM_TOKEN` 时，流水线会跳过对应发布步骤，不会失败。
- `lesscoder server` 当前仍依赖 Rust 环境；后续可改为下载预编译 adapter 二进制。
