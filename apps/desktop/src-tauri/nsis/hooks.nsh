; BETA-12 卸载流程 + BETA-78 服务安装/卸载流程（NSIS 安装/卸载器 hook，经
; tauri.conf.json > bundle.windows.nsis.installerHooks 挂载）。原文件名
; `uninstall-hooks.nsh`——BETA-78 起同时承载装机自动配置/注册后台服务的
; `NSIS_HOOK_POSTINSTALL`，改名 `hooks.nsh` 更贴切；仓内引用只有
; tauri.conf.json 一处，已同步改名。

; scoutd 个人模式数据目录：`%ProgramData%\Scout\scoutd`——必须与
; `apps/daemon/src/personal.rs::default_data_dir` 的 Windows 分支保持一致
; （改一处需同步改另一处）。NSIS 没有内置 `$PROGRAMDATA` 常量，用
; `ExpandEnvStrings` 展开 `%ProgramData%` 环境变量。
Var ScoutdDataDir

!macro ScoutResolveDataDir
  ExpandEnvStrings $ScoutdDataDir "%ProgramData%"
  StrCpy $ScoutdDataDir "$ScoutdDataDir\Scout\scoutd"
!macroend

; ============================================================
; 安装：装机自动配置个人模式 config.toml（幂等）+ 注册 Windows Service
; （LocalSystem、开机自启，幂等——已存在则更新配置并确保处于 Running）。
; "安装时自动装好、配置好、自动启动" 就是这两条命令。
;
; 两步都不带 --data-dir：不给时 scoutd.exe 自己按同一份
; `personal::default_data_dir()` 逻辑解析默认值，与本文件 $ScoutdDataDir
; 的算法保持单一信源（避免 NSIS 侧和 Rust 侧各算一遍、悄悄漂移）。
;
; 失败只记日志、不中断安装——桌面在服务未连接时仍可启动（连接态由桌面自身
; 探测展示，见 service_client.rs），不应该让服务装不上这件事拖累整个应用
; 装不上。真机验收：见 apps/daemon 与 apps/desktop 两侧 README/STATUS 的
; "安装器自动装服务" 验收记录（本改动本身未在真实安装包上跑过，标注见
; STATUS.md 当前 task）。
; ============================================================
!macro NSIS_HOOK_POSTINSTALL
  SetDetailsPrint both
  DetailPrint "配置 Scout 后台服务…"
  nsExec::ExecToLog '"$INSTDIR\scoutd.exe" bootstrap-personal-config'
  Pop $0
  DetailPrint "注册并启动 Scout 后台服务…"
  nsExec::ExecToLog '"$INSTDIR\scoutd.exe" install-service'
  Pop $0
!macroend

; ============================================================
; 卸载前：停止服务，释放对 scoutd.exe 的文件句柄——升级（$UpdateMode=1）与
; 真卸载都需要这一步：升级会用新文件覆盖旧文件，运行中的服务持有旧
; scoutd.exe 映像句柄会导致覆盖失败；真卸载则是为了后续能干净删除安装目录。
; 只停不删服务注册——升级场景新版装完后 POSTINSTALL 会原样把它重新拉起；
; 真卸载场景删除注册的动作在下方 POSTUNINSTALL（文件已删除后，用 sc.exe
; 而非 scoutd.exe 自身执行，避免依赖一个可能已被删掉的文件）。
; ============================================================
!macro NSIS_HOOK_PREUNINSTALL
  DetailPrint "停止 Scout 后台服务…"
  nsExec::ExecToLog 'sc.exe stop Scoutd'
  Pop $0
!macroend

; 卸载时删除本机派生数据：日志 / 审计（$APPDATA\${PRODUCTNAME} 目录、与桌面端
; dirs::data_dir().join("Scout") 同一位置），并清除用户同义词库与搜索历史
; （查询词属敏感数据）。**保留配置**：settings.json——2026-07-29 起设置 / 历史 / 同义词库
; 统一收进 $APPDATA\${PRODUCTNAME}（不再散落到 Tauri bundle id 派生的 $APPDATA\${BUNDLEID}
; 目录，那曾让隐私面板「数据存在哪」展示自相矛盾），settings.json 因此与 index.db /
; models 同目录、同卷 Rename 暂存 → 整删 → 移回，两条分支（保留模型索引 / 彻底删除）
; 都显式保留，不再是「活在另一个目录、天然不受影响」。
;
; **模型 + 索引默认保留**：
; - 模型（2026-07-06 cycle 9 真机反馈拍板）：公开权重、非用户敏感数据，整删导致重装
;   ~700MB 重下。
; - 索引 index.db（+ -wal/-shm）（用户反馈拍板）：重建是小时级本地重新提取 + 嵌入的
;   计算成本（非下载），「先卸载旧版再装新版」这类手动两步操作不该让用户平白丢掉这份
;   成本、逼一次全量重扫——与覆盖安装走 `$UpdateMode` 分支保留数据同等待遇。
; 卸载时 MessageBox 询问是否一并删除模型与索引，默认「否」（保留）；静默卸载（/S）走
; /SD IDNO = 保留。保留实现：models 子目录 + index.db 系列文件 + settings.json 同卷
; Rename 暂存 → 整目录 RMDir /r（敏感派生数据零遗漏、未来新增子项自动纳入删除面）→ 移回。
; **索引仍含从用户文档抽出的正文 / PII 实体标记（BETA-59）**——选「是」彻底删除时，索引
; 与模型一样整删，不会遗留任何文档正文片段；settings.json 两条分支都保留。
;
; BETA-78：同一个选择现在也覆盖 scoutd 服务侧数据（`%ProgramData%\Scout\scoutd`，
; 含它自己的 config.toml/index.db/models/connection.json）——选「是」彻底删除时
; 一并清空；选「否」（默认）保留，方便日后重装或重新 install-service 时索引接着用。
;
; $UpdateMode 守卫（不可去掉）：版本升级时安装器带 /UPDATE 调起旧卸载器，
; 此时绝不清数据——否则每次升级都会误删用户索引与已下载模型（数百 MB 重下 + 小时级重扫）。
;
; 对应的应用内清理入口见 src/uninstall.rs（macOS / 便携版走那条路径；应用内清理仍是
; 全删含模型与索引——§6.3「一键删除索引/日志/模型/配置」指标由它承担，是用户主动发起的
; 「清空我的数据」动作，与「卸载程序想留着索引下次重装接着用」语义不同，不做同等保留）。
; 仓内闸门：uninstall.rs 测试 nsis_uninstall_hook_is_wired_and_guarded 校验本文件在位 +
; 守卫在位 + 模型/索引/配置保留路径在位。
!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $UpdateMode <> 1
    ; BETA-78：真卸载（非升级）才删服务注册——用 sc.exe（本文件此时 scoutd.exe
    ; 已随 $INSTDIR 一起被主卸载流程删除，不能再依赖它自己的 uninstall-service）。
    DetailPrint "删除 Scout 后台服务注册…"
    nsExec::ExecToLog 'sc.exe delete Scoutd'
    Pop $0
    !insertmacro ScoutResolveDataDir

    SetShellVarContext current
    ; 问一次「模型 + 索引」去留（默认/静默 = 都保留）。settings.json 不受此选择影响，
    ; 两条分支都会经暂存 Rename 保留（见下方）。
    MessageBox MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON2 \
      "是否同时删除已下载的 AI 模型文件（约 700 MB）、搜索索引数据库，以及后台服务的索引数据？$\r$\n选「否」都保留，重装后模型无需重新下载、索引自动增量续跑（不用整库重扫）。" \
      /SD IDNO IDYES scout_delete_all
    ; 保留模型 + 索引 + 配置：同卷暂存 → 整删 → 移回（目标不存在时各步自然 no-op）。
    Rename "$APPDATA\${PRODUCTNAME}\models" "$APPDATA\${PRODUCTNAME}-models-keep"
    Rename "$APPDATA\${PRODUCTNAME}\index.db" "$APPDATA\${PRODUCTNAME}-index.db-keep"
    Rename "$APPDATA\${PRODUCTNAME}\index.db-wal" "$APPDATA\${PRODUCTNAME}-index.db-wal-keep"
    Rename "$APPDATA\${PRODUCTNAME}\index.db-shm" "$APPDATA\${PRODUCTNAME}-index.db-shm-keep"
    Rename "$APPDATA\${PRODUCTNAME}\settings.json" "$APPDATA\${PRODUCTNAME}-settings.json-keep"
    RMDir /r "$APPDATA\${PRODUCTNAME}"
    IfFileExists "$APPDATA\${PRODUCTNAME}-models-keep\*.*" 0 scout_restore_index
    CreateDirectory "$APPDATA\${PRODUCTNAME}"
    Rename "$APPDATA\${PRODUCTNAME}-models-keep" "$APPDATA\${PRODUCTNAME}\models"
scout_restore_index:
    IfFileExists "$APPDATA\${PRODUCTNAME}-index.db-keep" 0 scout_cleanup_config
    CreateDirectory "$APPDATA\${PRODUCTNAME}"
    Rename "$APPDATA\${PRODUCTNAME}-index.db-keep" "$APPDATA\${PRODUCTNAME}\index.db"
    IfFileExists "$APPDATA\${PRODUCTNAME}-index.db-wal-keep" 0 scout_restore_shm
    Rename "$APPDATA\${PRODUCTNAME}-index.db-wal-keep" "$APPDATA\${PRODUCTNAME}\index.db-wal"
scout_restore_shm:
    IfFileExists "$APPDATA\${PRODUCTNAME}-index.db-shm-keep" 0 scout_cleanup_config
    Rename "$APPDATA\${PRODUCTNAME}-index.db-shm-keep" "$APPDATA\${PRODUCTNAME}\index.db-shm"
    Goto scout_cleanup_config
scout_delete_all:
    ; 彻底删除模型 + 索引时，settings.json 仍保留（配置与「清空我的数据」语义不同）。
    Rename "$APPDATA\${PRODUCTNAME}\settings.json" "$APPDATA\${PRODUCTNAME}-settings.json-keep"
    RMDir /r "$APPDATA\${PRODUCTNAME}"
    ; BETA-78：连带清空服务侧数据目录（config.toml/index.db/models/connection.json）。
    RMDir /r "$ScoutdDataDir"
scout_cleanup_config:
    ; user-synonyms.yaml / search_history.json 不在任何暂存之列——两条分支的
    ; RMDir /r 都已把它们连带清除（敏感派生数据，卸载即视为要清）。下面两行是
    ; 旧版本（数据目录统一之前）遗留安装的兜底：那时这两个文件曾落在
    ; $APPDATA\${BUNDLEID}，此处 Delete 对新版安装天然 no-op。
    Delete "$APPDATA\${BUNDLEID}\user-synonyms.yaml"
    Delete "$APPDATA\${BUNDLEID}\search_history.json"
    IfFileExists "$APPDATA\${PRODUCTNAME}-settings.json-keep" 0 scout_done
    CreateDirectory "$APPDATA\${PRODUCTNAME}"
    Rename "$APPDATA\${PRODUCTNAME}-settings.json-keep" "$APPDATA\${PRODUCTNAME}\settings.json"
scout_done:
  ${EndIf}
!macroend
