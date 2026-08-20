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
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
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
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)
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
        );

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("创建 tokio runtime 失败")?;

        let result = runtime.block_on(run_async(data_dir, status_handle, stop_rx));

        set_status(
            status_handle,
            ServiceState::Stopped,
            ServiceControlAccept::empty(),
            0,
            Duration::default(),
        );
        result
    }

    fn set_status(
        handle: windows_service::service_control_handler::ServiceStatusHandle,
        state: ServiceState,
        controls_accepted: ServiceControlAccept,
        checkpoint: u32,
        wait_hint: Duration,
    ) {
        let status = ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted,
            exit_code: ServiceExitCode::Win32(0),
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
