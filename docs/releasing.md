# 发布流程

自动更新源固定为 `NovaBreeze/ScreenTranslation` 的 GitHub Releases。

1. 更新 `Cargo.toml` 中的版本号。
2. 本地运行：

   ```powershell
   cargo test
   cargo build --release
   ./scripts/package.ps1
   ```

3. 推送与版本一致的标签，例如：

   ```powershell
   git tag v0.2.0
   git push origin v0.2.0
   ```

4. Windows CI 会测试、构建 `ScreenTranslator-v0.2.0-win64.zip`，并上传到对应 GitHub Release。

应用按 `tag_name` 判断新版本，并选择文件名包含 `win64` 的 ZIP。更新前需要保持仓库及 Release 可公开读取；开发目录构建只提示下载，不会覆盖源码工作区。
