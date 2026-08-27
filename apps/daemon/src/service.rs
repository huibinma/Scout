//! BETA-78：Windows Service 注册与运行。
//!
//! 读 NTFS MFT（`scout-native-index`）需要打开 `\\.\C:` 卷句柄，硬性要求管理员
//! 权限。桌面 GUI 以普通用户权限运行不可能满足这个前提，因此把索引/检索整体
//! 挪进一个以 `LocalSystem` 账户常驻的 Windows Service——`LocalSystem` 天然在
//! 本机管理员信任边界内，且不需要管理服务账户密码（对开源个人工具而言，运维
//! 复杂度是要件）。
//!
//! 非 Windows 平台下方提供同签名 stub（返回明确错误），让 `cli.rs` 的子命令
//! 分发代码无需 `#[cfg]` 散落各处。

pub const SERVICE_NAME: &str = "Scoutd";
pub const SERVICE_DISPLAY_NAME: &str = "Scout 后台索引与检索服务";
pub const SERVICE_DESCRIPTION: &str =
    "Scout 本地语义检索：索引构建/更新 + hybrid 检索 + MCP，随系统自动启动（Deep Local Search）。";

#[cfg(windows)]
mod imp {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::{Context, Result};
    use tracing::{error, info, warn};
    use windows_service::service::{
        ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
        ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_service::{define_windows_service, service_dispatcher};

    use super::{SERVICE_DESCRIPTION, SERVICE_DISPLAY_NAME, SERVICE_NAME};

    /// 注册/更新 Windows Service（`LocalSystem`、开机自启）。**幂等**：服务已
    /// 存在（重装/升级场景）就更新配置而非报错，随后确保处于 Running。
    pub fn install_service(data_dir: &Path) -> Result<()> {
        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )
        .context("连接 Service Control Manager 失败（需要管理员权限）")?;

        let exe_path = std::env::current_exe().context("获取当前可执行文件路径失败")?;
        let launch_arguments = vec![
            OsString::from("service"),
            OsString::from("--data-dir"),
            data_dir.as_os_str().to_owned(),
        ];
        let info = ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from(SERVICE_DISPLAY_NAME),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: exe_path,
            launch_arguments,
            dependencies: vec![],
            // None = LocalSystem：不需要管理服务账户密码，且天然满足 MFT 读取所需
            // 的管理员信任边界。
            account_name: None,
            account_password: None,
        };

        let access = ServiceAccess::CHANGE_CONFIG
            | ServiceAccess::START
            | ServiceAccess::QUERY_STATUS
            | ServiceAccess::STOP;
        let service = match manager.create_service(&info, access) {
            Ok(s) => {
                info!(service = SERVICE_NAME, "Windows Service 已注册");
                s
            }
            Err(create_err) => {
                warn!(
                    error = %create_err,
                    "注册服务失败（可能已存在，尝试更新已有服务配置）"
                );
                let existing = manager
                    .open_service(SERVICE_NAME, access)
                    .context("服务不存在也无法创建、且无法打开已有同名服务")?;
                existing
                    .change_config(&info)
                    .context("更新已存在服务的配置失败")?;
                info!(
                    service = SERVICE_NAME,
                    "已存在的 Windows Service 配置已更新"
                );
                existing
            }
        };
        if let Err(e) = service.set_description(SERVICE_DESCRIPTION) {
            warn!(error = %e, "设置服务描述失败（不影响服务本身可用性）");
        }

        // 崩溃/异常退出后自动恢复：此前完全未配置（SCM 默认三次失败都"不采取操作"）——
        // 一次性的启动失败已经靠上面几处改动尽量收窄，但进程崩溃这类真正意外情况
        // （如 llama.cpp 原生代码 abort、栈溢出等 Rust panic hook 都兜不住的失败）此前
        // 只会让服务停在 Stopped、开机前都不会再自己起来。配置为：失败后 10s / 30s 重启，
        // 第三次起彻底不再自动重启（避免真正持续性故障时无限重启刷日志/占资源）、
        // 24 小时内无新失败则计数器清零。`set_failure_actions_on_non_crash_failures(true)`
        // 让"进程正常退出但 exit code 非零"（见上面 `run_service` 的 `ServiceSpecific(1)`）
        // 也纳入这套 recovery——不加这一步，同款失败在部分 Windows 版本上不会触发 restart。
        let failure_actions = ServiceFailureActions {
            reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
            reboot_msg: None,
            command: None,
            actions: Some(vec![
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(10),
                },
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(30),
                },
                ServiceAction {
                    action_type: ServiceActionType::None,
                    delay: Duration::default(),
                },
            ]),
        };
        if let Err(e) = service.update_failure_actions(failure_actions) {
            warn!(error = %e, "配置服务失败恢复策略失败（不影响服务本身可用性，仅崩溃后不会自动重启）");
        }
        if let Err(e) = service.set_failure_actions_on_non_crash_failures(true) {
            warn!(error = %e, "开启「非崩溃失败也触发恢复策略」失败");
        }

        let needs_start = match service.query_status() {
            Ok(status) => status.current_state != ServiceState::Running,
            Err(_) => true,
        };
        if needs_start {
            service
                .start(&[] as &[&std::ffi::OsStr])
                .context("启动服务失败")?;
            info!(service = SERVICE_NAME, "服务已启动");
        } else {
            info!(service = SERVICE_NAME, "服务已在运行，跳过启动");
        }
        Ok(())
    }

    /// 停止并删除已注册的服务。服务本不存在视为成功（卸载幂等）。
    pub fn uninstall_service() -> Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .context("连接 Service Control Manager 失败（需要管理员权限）")?;
        let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
        let service = match manager.open_service(SERVICE_NAME, access) {
            Ok(s) => s,
            Err(e) => {
                info!(error = %e, "服务不存在，视为已卸载");
                return Ok(());
            }
        };

        if let Ok(status) = service.query_status() {
            if status.current_state != ServiceState::Stopped {
                service.stop().context("停止服务失败")?;
                for _ in 0..30 {
                    std::thread::sleep(Duration::from_secs(1));
                    if service
                        .query_status()
                        .is_ok_and(|s| s.current_state == ServiceState::Stopped)
                    {
                        break;
                    }
                }
            }
        }
        service.delete().context("删除服务失败")?;
        info!(service = SERVICE_NAME, "服务已停止并删除");
        Ok(())
    }

    define_windows_service!(ffi_service_main, service_main);

    /// `service_main` 签名由宏固定（只接收 SCM 传的 `Vec<OsString>`），没有
    /// 余地传自定义 Rust 类型；用 `OnceLock` 在 `run_dispatcher` 里存一次
    /// `data_dir`，`service_main` 里取——单进程单次调用，无竞争。
    static DATA_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

    /// SCM 派发入口（由 `windows-service` 生成的 FFI 包装调用）。真正逻辑在
    /// [`run_service`]；这里只负责兜底记录 panic/error（SCM 侧无法接收
    /// 详细错误信息，只能看事件日志）。
    fn service_main(_arguments: Vec<OsString>) {
        if let Err(e) = run_service() {
            error!(error = %e, "scoutd service 运行失败");
        }
    }

    /// `service` 子命令真正的入口：把当前进程注册为 SCM 服务、阻塞直到
    /// 服务被停止。**必须**直接从 `main()` 的同步上下文调用（`service_dispatcher::start`
    /// 会另起线程调用 `service_main`，在拿到 SCM 控制权前不能有任何 tokio
    /// runtime 或其它长阻塞逻辑跑在当前线程）。
    pub fn run_dispatcher(data_dir: PathBuf) -> Result<()> {
        // 忽略 set 失败（同进程只应调用一次；重复调用是编程错误而非运行时故障）。
        let _ = DATA_DIR.set(data_dir);
        service_dispatcher::start(SERVICE_NAME, ffi_service_main).inspect_err(|e| {
            // 调用方 `main()` 只把这个 Err 交给 Rust 运行时默认打印到 stderr——
            // service 模式下 stderr 没人看。这里补一条 `tracing::error!`，让它也能
            // 落进 `init_tracing_to_file` 已经指好的 `scoutd.log`（此时 subscriber
            // 与 guard 均已就绪）。
            error!(error = %e, "向 SCM 注册 service_main 失败");
        })
        .context("向 SCM 注册 service_main 失败（本进程是否由 SCM 直接拉起？手动前台调试请直接跑今天的 --config/--root 前台模式，或先 --install-service 走正规安装再由 SCM 拉起）")
    }

    fn run_service() -> Result<()> {
        let data_dir = DATA_DIR
            .get()
            .cloned()
            .unwrap_or_else(crate::personal::default_data_dir);

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let stop_tx = Arc::new(Mutex::new(Some(stop_tx)));

        let event_handler = {
            let stop_tx = stop_tx.clone();
            move |control_event: ServiceControl| -> ServiceControlHandlerResult {
                match control_event {
                    ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                    ServiceControl::Stop | ServiceControl::Shutdown => {
                        if let Some(tx) = stop_tx
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .take()
                        {
                            let _ = tx.send(());
                        }
                        ServiceControlHandlerResult::NoError
                    }
                    _ => ServiceControlHandlerResult::NotImplemented,
                }
            }
        };
        let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
            .context("注册 SCM 控制处理器失败")?;

        set_status(
            status_handle,
            ServiceState::StartPending,
            ServiceControlAccept::empty(),
            1,
            Duration::from_secs(15),
            ServiceExitCode::NO_ERROR,
        );

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("创建 tokio runtime 失败")?;

        let result = runtime.block_on(run_async(data_dir, status_handle, stop_rx));

        // 之前这里恒报 Win32(0)（成功），哪怕 `result` 是 `Err`——SCM/`sc query`/事件查看器
        // 侧看到的是一次"干净退出"，唯一能看出真失败的地方只有 `scoutd.log`（需要先知道
        // 去查）。这本身不影响本轮排查的"服务起不来"场景（那类失败根本到不了这里），
        // 但一旦真出现"启动后又迅速自己退出"的故障，如实报非零 exit code 能让 SCM 的
        // failure actions（见 `install_service` 里新增的 `update_failure_actions`）正确
        // 识别为一次失败、以及让 Windows 侧工具看到"服务不是正常停止"。
        let exit_code = if result.is_ok() {
            ServiceExitCode::NO_ERROR
        } else {
            ServiceExitCode::ServiceSpecific(1)
        };
        set_status(
            status_handle,
            ServiceState::Stopped,
            ServiceControlAccept::empty(),
            0,
            Duration::default(),
            exit_code,
        );
        result
    }

    fn set_status(
        handle: windows_service::service_control_handler::ServiceStatusHandle,
        state: ServiceState,
        controls_accepted: ServiceControlAccept,
        checkpoint: u32,
        wait_hint: Duration,
        exit_code: ServiceExitCode,
    ) {
        let status = ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted,
            exit_code,
            checkpoint,
            wait_hint,
            process_id: None,
        };
        if let Err(e) = handle.set_service_status(status) {
            error!(error = %e, ?state, "上报 SCM 服务状态失败");
        }
    }

    /// 真正的异步主体：构造个人模式 `ServerCtx`（期间持续给 SCM 打 `StartPending`
    /// checkpoint，覆盖首次全量索引可能超过 SCM 默认 30s 启动超时的风险）→
    /// bind → 写 `connection.json` → 报 `Running` → 跑 server 直到 SCM Stop。
    async fn run_async(
        data_dir: PathBuf,
        status_handle: windows_service::service_control_handler::ServiceStatusHandle,
        stop_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<()> {
        let checkpoint = Arc::new(AtomicU32::new(1));
        let ticker = {
            let checkpoint = checkpoint.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    let cp = checkpoint.fetch_add(1, Ordering::SeqCst) + 1;
                    set_status(
                        status_handle,
                        ServiceState::StartPending,
                        ServiceControlAccept::empty(),
                        cp,
                        Duration::from_secs(15),
                        ServiceExitCode::NO_ERROR,
                    );
                }
            })
        };

        let built = crate::build_personal_service(data_dir.clone()).await;
        ticker.abort();
        let (ctx, bind_addr, token) = built?;

        let listener = tokio::net::TcpListener::bind(bind_addr)
            .await
            .with_context(|| format!("绑定监听地址失败：{bind_addr}"))?;
        crate::personal::write_connection_file(&data_dir, bind_addr, &token)
            .context("写 connection.json 失败")?;
        info!(%bind_addr, "scoutd（service 模式）监听就绪");

        set_status(
            status_handle,
            ServiceState::Running,
            ServiceControlAccept::STOP,
            0,
            Duration::default(),
            ServiceExitCode::NO_ERROR,
        );

        scout_server::app::serve_bound(listener, ctx, async move {
            let _ = stop_rx.await;
        })
        .await
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::{Path, PathBuf};

    use anyhow::{bail, Result};

    pub fn install_service(_data_dir: &Path) -> Result<()> {
        bail!("Windows Service 仅支持 Windows 平台")
    }

    pub fn uninstall_service() -> Result<()> {
        bail!("Windows Service 仅支持 Windows 平台")
    }

    pub fn run_dispatcher(_data_dir: PathBuf) -> Result<()> {
        bail!("Windows Service 仅支持 Windows 平台；本机请直接前台运行 `scoutd --config ...`")
    }
}

pub use imp::{install_service, run_dispatcher, uninstall_service};
