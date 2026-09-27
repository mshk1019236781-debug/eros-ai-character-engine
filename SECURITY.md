# Security Notes

公开 GitHub 前请遵守：

- 不提交 `.env`、`.env.local`、真实 API key、数据库密码、JWT secret。
- 不提交 prompt logs、用户对话日志、本地测试身份令牌。
- 不提交 `node_modules/`、`dist/`、Rust `target/` 等构建产物。
- `.env.example` 只能保留占位符。
- 如果任何密钥曾经出现在待上传压缩包、Git 历史或聊天附件中，应在对应服务商后台立即轮换/撤销，而不是只删除文件。
