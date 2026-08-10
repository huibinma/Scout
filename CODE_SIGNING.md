# Code signing policy

Scout 的 Windows Release 使用 Authenticode 代码签名。Free code signing provided by
[SignPath.io](https://about.signpath.io/), certificate by
[SignPath Foundation](https://signpath.org/)。

## 适用范围

- 只签名从本仓库公开源码和本仓库构建脚本生成的 Scout Windows NSIS 安装包。
- 签名构建只能由本仓库 GitHub Actions 的 GitHub-hosted Windows runner 产生。
- 未经 SignPath 完成来源校验和人工批准的 Windows 安装包不得上传到 GitHub Release。
- 第三方开源依赖不会以 Scout 项目名义单独签名；依赖许可见
  [第三方依赖授权清单](./docs/third-party-licenses.md)。

## 团队角色

Scout 当前由单一维护者维护：

- Authors / committers：[huibinma](https://github.com/huibinma)
- Reviewers：[huibinma](https://github.com/huibinma)；外部贡献必须经维护者审核后合并
- Approvers：[huibinma](https://github.com/huibinma)；每个 Release 的 SignPath 签名请求必须人工批准

所有拥有源码写权限或 SignPath 签名权限的成员必须在 GitHub 和 SignPath 启用多因素认证。

## 隐私与联网

Scout 不向项目维护者上传文件名、路径、文件内容、搜索词、索引数据或使用统计。只有用户
主动点击模型下载时，桌面应用才会从公开模型站点下载所选模型；daemon 是否把检索结果
发送给外部 LLM 由部署者配置。完整说明见 [PRIVACY.md](./PRIVACY.md)。

## 发布与核验

1. 维护者在 GitHub 创建 `v*` tag。
2. GitHub Actions 在 `windows-latest` 构建 NSIS 安装包并上传为 workflow artifact。
3. SignPath GitHub connector 校验构建来源，SignPath approver 人工批准签名请求。
4. 工作流验证 Authenticode 状态为 `Valid` 后，才把签名安装包上传到对应 GitHub Release。

用户可在 Windows PowerShell 中核验签名：

```powershell
Get-AuthenticodeSignature .\Scout_*_x64-setup.exe | Format-List Status,SignerCertificate
```

预期 `Status` 为 `Valid`，签名证书发布者显示 SignPath Foundation。
