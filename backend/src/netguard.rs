//! 网络自愈守护
//!
//! 背景：UDX710 固件在切换射频模式 / 基站重选后，可能只恢复 IPv6 路由，
//! IPv4 默认路由未重建。此时 ofono 的 `Active` 属性仍为 true，传统 watchdog
//! 无法发现，表现为 IPv6 正常、IPv4 全部超时（Network is unreachable）。
//!
//! 本模块用“路由快速修复 + 健康探测 + 分级 Modem 恢复”三层状态机处理：
//! 1. `RouteReconcile`：发现网关缺失或不匹配立即补回，不进入 cooldown。
//! 2. `DataReactivate`：重新激活数据连接，单次超时严格受限。
//! 3. `RadioReset`：AT+CFUN=0/1 重启协议栈，仅在重拨连续失败达阈值时执行。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::Utc;
use futures_util::StreamExt;
use serde::Serialize;
use tokio::sync::{Notify, RwLock};
use tokio::task::spawn_blocking;
use tokio::time::{sleep, timeout};
use tracing::{info, warn};
use zbus::zvariant::OwnedValue;
use zbus::{Connection, MessageStream, Proxy};

use crate::config::{ConfigManager, NetworkGuardConfig};
use crate::dbus::{
    get_data_connection_status, get_network_info_data, send_at_command_with_timeout,
    set_data_connection_with_timeout,
};

/// 蜂窝数据接口名（UDX710 平台固定）
const WAN_INTERFACE: &str = "sipa_eth0";

#[derive(Debug, Clone)]
struct Ipv4Route {
    interface: String,
    gateway: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct NetguardStatus {
    pub enabled: bool,
    pub state: String,
    pub health: String,
    pub consecutive_failures: u32,
    pub consecutive_successes: u32,
    pub last_error: Option<String>,
    pub last_recovery_ms: Option<u64>,
    pub route_repairs: u64,
    pub data_reactivate_attempts: u64,
    pub data_reactivate_successes: u64,
    pub cfun_last_triggered_at: Option<String>,
    pub cfun_in_cooldown: bool,
    pub probe_targets: Vec<String>,
    pub last_route_check_at: Option<String>,
    pub expected_gateway: Option<String>,
    pub observed_gateway: Option<String>,
    pub route_ok: bool,
    pub manual_data_off: bool,
}

impl Default for NetguardStatus {
    fn default() -> Self {
        Self {
            enabled: false,
            state: "initializing".to_string(),
            health: "unknown".to_string(),
            consecutive_failures: 0,
            consecutive_successes: 0,
            last_error: None,
            last_recovery_ms: None,
            route_repairs: 0,
            data_reactivate_attempts: 0,
            data_reactivate_successes: 0,
            cfun_last_triggered_at: None,
            cfun_in_cooldown: false,
            probe_targets: Vec::new(),
            last_route_check_at: None,
            expected_gateway: None,
            observed_gateway: None,
            route_ok: false,
            manual_data_off: false,
        }
    }
}

#[derive(Clone)]
pub struct NetguardHandle {
    inner: Arc<NetguardInner>,
}

struct NetguardInner {
    conn: Arc<Connection>,
    config: Arc<ConfigManager>,
    status: RwLock<NetguardStatus>,
    notify: Arc<Notify>,
    last_event: std::sync::RwLock<Option<Instant>>,
    manual_data_off: AtomicBool,
}

impl NetguardHandle {
    pub fn spawn(conn: Arc<Connection>, config: Arc<ConfigManager>) -> Self {
        let inner = Arc::new(NetguardInner {
            conn,
            config,
            status: RwLock::new(NetguardStatus::default()),
            notify: Arc::new(Notify::new()),
            last_event: std::sync::RwLock::new(None),
            manual_data_off: AtomicBool::new(false),
        });
        let handle = Self {
            inner: Arc::clone(&inner),
        };

        let route_inner = Arc::clone(&inner);
        let health_inner = Arc::clone(&inner);
        let signal_notify = Arc::clone(&inner.notify);

        tokio::spawn(async move {
            route_loop(route_inner).await;
        });
        tokio::spawn(async move {
            health_loop(health_inner).await;
        });
        tokio::spawn(async move {
            dbus_signal_listener(signal_notify).await;
        });

        handle
    }

    /// 从 API handler 触发的快速重调和健康检查。
    pub fn schedule_reconcile(&self, delay: Duration) {
        let inner = Arc::clone(&self.inner);
        let notify = Arc::clone(&inner.notify);

        tokio::spawn(async move {
            sleep(delay).await;
            if let Ok(mut last_event) = inner.last_event.write() {
                *last_event = Some(Instant::now());
            }
            notify.notify_one();
        });
    }

    pub async fn snapshot(&self) -> NetguardStatus {
        self.inner.status.read().await.clone()
    }

    /// 用户手动关闭数据时暂停自动激活，直到用户重新打开。
    pub async fn set_data_paused(&self, paused: bool) {
        self.inner.manual_data_off.store(paused, Ordering::Relaxed);
        let mut status = self.inner.status.write().await;
        status.manual_data_off = paused;
        if paused {
            status.health = "paused".to_string();
            status.state = "paused_by_user".to_string();
        } else {
            status.health = "down".to_string();
            status.state = "activating_data".to_string();
        }
    }
}

async fn run_cmd(program: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
    let program = program.to_string();
    let args: Vec<String> = args.iter().map(|value| value.to_string()).collect();
    spawn_blocking(move || std::process::Command::new(program).args(args).output())
        .await
        .unwrap_or_else(|error| {
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("command task failed: {error}"),
            ))
        })
}

async fn ping_ok(target: &str) -> bool {
    run_cmd("ping", &["-c", "1", "-W", "2", target])
        .await
        .map(|output| output.status.success())
        .unwrap_or(false)
}

async fn wait_ping_ok(targets: &[String], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        for target in targets {
            if ping_ok(target).await {
                return true;
            }
        }
        sleep(Duration::from_millis(500)).await;
    }
    false
}

async fn probe_ok(targets: &[String]) -> bool {
    for target in targets {
        if ping_ok(target).await {
            return true;
        }
    }
    false
}

async fn wan_has_ipv4() -> bool {
    run_cmd("ip", &["-4", "-o", "addr", "show", "dev", WAN_INTERFACE])
        .await
        .map(|output| String::from_utf8_lossy(&output.stdout).contains("inet "))
        .unwrap_or(false)
}

fn route_matches(route: &Ipv4Route, expected_gateway: Option<&str>) -> bool {
    if route.interface != WAN_INTERFACE {
        return false;
    }

    match expected_gateway.filter(|gateway| !gateway.trim().is_empty()) {
        Some(gateway) => route.gateway.as_deref() == Some(gateway),
        None => true,
    }
}

fn hex_to_ipv4(value: &str) -> Option<String> {
    if value.len() != 8 {
        return None;
    }
    let number = u32::from_str_radix(value, 16).ok()?;
    Some(format!(
        "{}.{}.{}.{}",
        number & 0xff,
        (number >> 8) & 0xff,
        (number >> 16) & 0xff,
        (number >> 24) & 0xff
    ))
}

fn parse_proc_default_route(text: &str) -> Option<Ipv4Route> {
    let mut lines = text.lines();
    lines.next();

    for line in lines {
        let columns: Vec<&str> = line.split_whitespace().collect();
        if columns.len() < 8 {
            continue;
        }

        let destination = columns[1];
        let gateway = columns[2];
        let flags = u32::from_str_radix(columns[3], 16).unwrap_or(0);

        if destination != "00000000" || flags & 0x1 == 0 {
            continue;
        }

        let gateway = if gateway == "00000000" {
            None
        } else {
            hex_to_ipv4(gateway)
        };

        return Some(Ipv4Route {
            interface: columns[0].to_string(),
            gateway,
        });
    }

    None
}

fn parse_ip_default_route(text: &str) -> Option<Ipv4Route> {
    for line in text.lines() {
        if !line.starts_with("default ") {
            continue;
        }

        let fields: Vec<&str> = line.split_whitespace().collect();
        let gateway = fields
            .iter()
            .position(|field| *field == "via")
            .and_then(|index| fields.get(index + 1))
            .map(|value| value.to_string());
        let interface = fields
            .iter()
            .position(|field| *field == "dev")
            .and_then(|index| fields.get(index + 1))
            .map(|value| value.to_string());

        if let Some(interface) = interface {
            return Some(Ipv4Route { interface, gateway });
        }
    }

    None
}

async fn current_default_route() -> Option<Ipv4Route> {
    if let Ok(text) = tokio::fs::read_to_string("/proc/net/route").await {
        if let Some(route) = parse_proc_default_route(&text) {
            return Some(route);
        }
    }

    let output = run_cmd("ip", &["-4", "route", "show", "default"])
        .await
        .ok()?;
    parse_ip_default_route(&String::from_utf8_lossy(&output.stdout))
}

/// 从 ofono 的 internet context 读取网关地址。
async fn ofono_context_gateway(conn: &Connection) -> Option<String> {
    let path = crate::dbus::find_internet_context(conn).await.ok()?;
    let proxy = Proxy::new(
        conn,
        "org.ofono",
        path.as_str(),
        "org.ofono.ConnectionContext",
    )
    .await
    .ok()?;

    let settings: HashMap<String, OwnedValue> = proxy.get_property("Settings").await.ok()?;

    settings
        .get("Gateway")
        .and_then(|value| String::try_from(value.clone()).ok())
        .filter(|gateway| !gateway.trim().is_empty())
}

/// 点对点 WAN 的 scope-link 主机路由通常就是对端网关。
fn parse_scope_link_gateway(text: &str, interface: &str) -> Option<String> {
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 5 {
            continue;
        }
        if fields.get(1) != Some(&"dev")
            || fields.get(2) != Some(&interface)
            || !fields.contains(&"scope")
            || !fields.contains(&"link")
            || fields.contains(&"via")
        {
            continue;
        }

        let destination = fields[0];
        if !destination.contains('/') && destination.parse::<std::net::Ipv4Addr>().is_ok() {
            return Some(destination.to_string());
        }
    }

    None
}

async fn wan_scope_link_gateway() -> Option<String> {
    let output = run_cmd("ip", &["-4", "route", "show", "dev", WAN_INTERFACE])
        .await
        .ok()?;
    parse_scope_link_gateway(&String::from_utf8_lossy(&output.stdout), WAN_INTERFACE)
}

async fn wan_ipv4_address() -> Option<String> {
    let output = run_cmd(
        "ip",
        &["-4", "-o", "addr", "show", "dev", WAN_INTERFACE],
    )
    .await
    .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let fields: Vec<&str> = text.split_whitespace().collect();
    let index = fields.iter().position(|field| *field == "inet")?;
    let address = fields.get(index + 1)?;
    Some(address.split('/').next()?.to_string())
}

async fn context_gateway(conn: &Connection) -> Option<String> {
    if let Some(gateway) = ofono_context_gateway(conn).await {
        return Some(gateway);
    }
    if let Some(gateway) = wan_scope_link_gateway().await {
        return Some(gateway);
    }
    wan_ipv4_address().await
}

fn candidate_replace_commands(expected_gateway: Option<&str>) -> Vec<Vec<String>> {
    let mut candidates = Vec::new();

    if let Some(gateway) = expected_gateway.filter(|gateway| !gateway.trim().is_empty()) {
        candidates.push(
            vec![
                "-4",
                "route",
                "replace",
                "default",
                "via",
                gateway,
                "dev",
                WAN_INTERFACE,
                "onlink",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
        );
        candidates.push(
            vec![
                "-4",
                "route",
                "replace",
                "default",
                "via",
                gateway,
                "dev",
                WAN_INTERFACE,
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
        );
    }

    candidates.push(
        vec![
            "-4",
            "route",
            "replace",
            "default",
            "dev",
            WAN_INTERFACE,
            "onlink",
        ]
        .into_iter()
        .map(str::to_string)
        .collect(),
    );
    candidates.push(
        vec!["-4", "route", "replace", "default", "dev", WAN_INTERFACE]
            .into_iter()
            .map(str::to_string)
            .collect(),
    );

    candidates
}

async fn restore_default_route(expected_gateway: Option<&str>) -> bool {
    for args in candidate_replace_commands(expected_gateway) {
        let args_ref: Vec<&str> = args.iter().map(|value| value.as_str()).collect();
        let _ = run_cmd("ip", &args_ref).await;
        sleep(Duration::from_millis(80)).await;

        if let Some(route) = current_default_route().await {
            if route_matches(&route, expected_gateway) {
                info!(command = %args.join(" "), "netguard: IPv4 route restored");
                return true;
            }
        }
    }

    false
}

async fn reconcile_route_once(inner: &Arc<NetguardInner>) -> bool {
    let expected_gateway = context_gateway(&inner.conn).await;
    let route = current_default_route().await;
    let observed_gateway = route.as_ref().and_then(|route| route.gateway.clone());
    let route_ok = route
        .as_ref()
        .is_some_and(|route| route_matches(route, expected_gateway.as_deref()));

    {
        let mut status = inner.status.write().await;
        status.last_route_check_at = Some(Utc::now().to_rfc3339());
        status.expected_gateway = expected_gateway.clone();
        status.observed_gateway = observed_gateway.clone();
        status.route_ok = route_ok;
    }

    if route_ok {
        return true;
    }

    if !wan_has_ipv4().await {
        let mut status = inner.status.write().await;
        if status.health != "ok" && !status.manual_data_off {
            status.state = "waiting_data".to_string();
        }
        return false;
    }

    let repaired = restore_default_route(expected_gateway.as_deref()).await;
    if repaired {
        let mut status = inner.status.write().await;
        status.route_repairs = status.route_repairs.saturating_add(1);
        status.route_ok = true;
        status.observed_gateway = expected_gateway.clone();
        status.state = "route_reconcile".to_string();
        status.last_error = None;
        log_event(inner, "route_reconcile: default route restored");
    } else {
        let mut status = inner.status.write().await;
        status.route_ok = false;
        status.state = "route_reconcile_stalled".to_string();
        status.last_error = Some(format!(
            "default route mismatch: expected {expected_gateway:?}, observed {observed_gateway:?}"
        ));
    }

    repaired
}

fn fast_window_active(inner: &Arc<NetguardInner>, config: &NetworkGuardConfig) -> bool {
    let last_event = inner
        .last_event
        .read()
        .ok()
        .and_then(|value| *value)
        .unwrap_or(Instant::now() - Duration::from_secs(3_600));

    last_event.elapsed() <= Duration::from_millis(config.fast_window_ms)
}

async fn route_loop(inner: Arc<NetguardInner>) {
    info!("netguard: route reconcile loop started");
    loop {
        let config = inner.config.get_network_guard();
        if !config.enabled {
            sleep(Duration::from_secs(1)).await;
            continue;
        }

        let poll_ms = if fast_window_active(&inner, &config) {
            config.fast_poll_ms
        } else {
            config.route_poll_ms
        };

        let timer = timeout(Duration::from_millis(poll_ms), inner.notify.notified()).await;
        if timer.is_ok() {
            if let Ok(mut last_event) = inner.last_event.write() {
                *last_event = Some(Instant::now());
            }
        }

        let _ = timeout(Duration::from_secs(5), reconcile_route_once(&inner)).await;
    }
}

async fn data_active(conn: &Connection) -> bool {
    timeout(Duration::from_secs(3), get_data_connection_status(conn))
        .await
        .ok()
        .and_then(|result| result.ok())
        .unwrap_or(false)
}

async fn is_registered(conn: &Connection) -> bool {
    match timeout(Duration::from_secs(3), get_network_info_data(conn)).await {
        Ok(Ok(info)) => matches!(info.registration_status.as_str(), "registered" | "roaming"),
        _ => false,
    }
}

async fn reactivate_data(
    conn: &Connection,
    config: &NetworkGuardConfig,
    force_off: bool,
) -> bool {
    let total_budget = config.data_reactivate_timeout_ms;
    let started = Instant::now();

    if force_off {
        let off_timeout = Duration::from_millis(total_budget.min(3_000));

        if set_data_connection_with_timeout(conn, false, off_timeout)
            .await
            .is_err()
        {
            return false;
        }

        sleep(Duration::from_millis(300)).await;
    }

    let elapsed_ms = started.elapsed().as_millis() as u64;
    let remaining = total_budget.saturating_sub(elapsed_ms);
    if remaining < 1_000 {
        return false;
    }
    let on_timeout = Duration::from_millis(remaining.min(total_budget));

    if set_data_connection_with_timeout(conn, true, on_timeout)
        .await
        .is_err()
    {
        return false;
    }

    wait_ping_ok(
        &[
            config.probe_target.clone(),
            config.probe_backup_target.clone(),
        ],
        Duration::from_millis(remaining),
    )
    .await
}

async fn reset_radio_stack(conn: &Connection, config: &NetworkGuardConfig) -> bool {
    let step_timeout = Duration::from_millis(config.cfun_step_timeout_ms);

    let _ = send_at_command_with_timeout(conn, "AT+CFUN=0", step_timeout).await;
    sleep(Duration::from_secs(2)).await;
    let _ = send_at_command_with_timeout(conn, "AT+CFUN=1", step_timeout).await;

    wait_ping_ok(
        &[
            config.probe_target.clone(),
            config.probe_backup_target.clone(),
        ],
        Duration::from_secs(60),
    )
    .await
}

fn should_reset_radio(
    fail_count: u32,
    threshold: u32,
    cooldown_ok: bool,
    allow_cfun: bool,
) -> bool {
    allow_cfun && cooldown_ok && fail_count >= threshold
}

async fn health_repair(
    inner: &Arc<NetguardInner>,
    config: &NetworkGuardConfig,
    data_fail_count: &mut u32,
    cfun_last_at: &mut Option<Instant>,
    data_retry_at: &mut Option<Instant>,
    force_off: bool,
    allow_cfun: bool,
) -> bool {
    let now = Instant::now();
    if inner.manual_data_off.load(Ordering::Relaxed) {
        return false;
    }
    if data_retry_at.is_some_and(|until| now < until) {
        return false;
    }

    let _ = timeout(Duration::from_secs(5), reconcile_route_once(inner)).await;

    if inner.manual_data_off.load(Ordering::Relaxed) {
        return false;
    }

    let mut status = inner.status.write().await;
    status.data_reactivate_attempts = status.data_reactivate_attempts.saturating_add(1);
    drop(status);

    let recovered = reactivate_data(&inner.conn, config, force_off).await;
    if recovered {
        *data_fail_count = 0;
        *data_retry_at = None;
        let elapsed = now.elapsed().as_millis() as u64;
        let mut status = inner.status.write().await;
        status.data_reactivate_successes = status.data_reactivate_successes.saturating_add(1);
        status.last_recovery_ms = Some(elapsed);
        status.state = "healthy".to_string();
        status.last_error = None;
        log_event(
            inner,
            &format!("data_reactivate: recovered in {elapsed} ms"),
        );
        return true;
    }

    *data_fail_count = data_fail_count.saturating_add(1);
    *data_retry_at = Some(now + Duration::from_secs(5));

    let can_reset_radio = cfun_last_at
        .map(|last| now.duration_since(last) >= Duration::from_secs(config.cfun_cooldown_secs))
        .unwrap_or(true);

    if should_reset_radio(
        *data_fail_count,
        config.cfun_fail_threshold,
        can_reset_radio,
        allow_cfun,
    ) {
        warn!("netguard: data reactivation repeatedly failed, restarting radio stack");
        let started = Instant::now();
        let recovered = reset_radio_stack(&inner.conn, config).await;
        *cfun_last_at = Some(Instant::now());
        *data_fail_count = 0;
        *data_retry_at = if recovered {
            None
        } else {
            Some(Instant::now() + Duration::from_secs(30))
        };

        let mut status = inner.status.write().await;
        status.cfun_last_triggered_at = Some(Utc::now().to_rfc3339());
        status.cfun_in_cooldown = true;
        status.last_recovery_ms = if recovered {
            Some(started.elapsed().as_millis() as u64)
        } else {
            None
        };
        status.state = if recovered {
            "healthy".to_string()
        } else {
            "radio_reset_stalled".to_string()
        };
        status.last_error = if recovered {
            None
        } else {
            Some("radio stack restart did not restore connectivity".to_string())
        };
        log_event(
            inner,
            &format!(
                "radio_reset: {}",
                if recovered { "recovered" } else { "failed" }
            ),
        );
        return recovered;
    }

    let mut status = inner.status.write().await;
    status.state = "data_reactivate_stalled".to_string();
    status.cfun_in_cooldown = !can_reset_radio;
    status.last_error = Some("data reactivation failed".to_string());
    log_event(inner, "data_reactivate: failed");
    false
}

async fn health_loop(inner: Arc<NetguardInner>) {
    info!("netguard: health loop started");
    let mut failures: u32 = 0;
    let mut successes: u32 = 0;
    let mut data_fail_count: u32 = 0;
    let mut cfun_last_at: Option<Instant> = None;
    let mut data_retry_at: Option<Instant> = None;
    let mut registered_since: Option<Instant> = None;

    loop {
        let config = inner.config.get_network_guard();
        let probe_targets = vec![
            config.probe_target.clone(),
            config.probe_backup_target.clone(),
        ];

        {
            let mut status = inner.status.write().await;
            status.enabled = config.enabled;
            status.probe_targets = probe_targets.clone();
            status.consecutive_failures = failures;
            status.consecutive_successes = successes;
            status.manual_data_off = inner.manual_data_off.load(Ordering::Relaxed);
            status.cfun_in_cooldown = cfun_last_at
                .map(|last| last.elapsed() < Duration::from_secs(config.cfun_cooldown_secs))
                .unwrap_or(false);
            if successes >= config.health_recovery_threshold {
                status.state = "healthy".to_string();
            }
        }

        if !config.enabled {
            failures = 0;
            successes = 0;
            data_fail_count = 0;
            data_retry_at = None;
            registered_since = None;
            sleep(Duration::from_secs(1)).await;
            continue;
        }

        sleep(Duration::from_millis(config.health_interval_ms)).await;

        if inner.manual_data_off.load(Ordering::Relaxed) {
            failures = 0;
            successes = 0;
            data_fail_count = 0;
            data_retry_at = None;
            let mut status = inner.status.write().await;
            status.health = "paused".to_string();
            status.state = "paused_by_user".to_string();
            status.consecutive_failures = 0;
            status.consecutive_successes = 0;
            status.last_error = None;
            continue;
        }

        if !is_registered(&inner.conn).await {
            registered_since = None;
            failures = 0;
            successes = 0;
            data_fail_count = 0;
            data_retry_at = None;
            let mut status = inner.status.write().await;
            status.health = "down".to_string();
            status.state = "waiting_registration".to_string();
            status.consecutive_failures = 0;
            status.consecutive_successes = 0;
            status.last_error = None;
            continue;
        }

        let registered_for = registered_since
            .get_or_insert_with(Instant::now)
            .elapsed();

        if !data_active(&inner.conn).await {
            failures = 0;
            successes = 0;

            {
                let mut status = inner.status.write().await;
                status.health = "down".to_string();
                status.state = "activating_data".to_string();
                status.consecutive_failures = 0;
                status.consecutive_successes = 0;
            }

            let recovered = health_repair(
                &inner,
                &config,
                &mut data_fail_count,
                &mut cfun_last_at,
                &mut data_retry_at,
                false,
                registered_for >= Duration::from_secs(10),
            )
            .await;

            if recovered {
                successes = 1;
            } else {
                let mut status = inner.status.write().await;
                status.state = "data_inactive".to_string();
                if status.last_error.is_none() {
                    status.last_error = Some("data connection is inactive".to_string());
                }
            }
            continue;
        }

        if probe_ok(&probe_targets).await {
            failures = 0;
            successes = successes
                .saturating_add(1)
                .min(config.health_recovery_threshold);
            let mut status = inner.status.write().await;
            status.health = "ok".to_string();
            status.last_error = None;
            if successes >= config.health_recovery_threshold {
                status.state = "healthy".to_string();
                data_fail_count = 0;
                data_retry_at = None;
            }
            continue;
        }

        failures = failures.saturating_add(1);
        successes = 0;
        let mut status = inner.status.write().await;
        status.health = "degraded".to_string();
        status.state = "probing".to_string();
        status.last_error = None;
        drop(status);

        if failures < config.health_failure_threshold {
            continue;
        }

        failures = 0;
        let recovered = health_repair(
            &inner,
            &config,
            &mut data_fail_count,
            &mut cfun_last_at,
            &mut data_retry_at,
            true,
            true,
        )
        .await;

        if recovered {
            successes = 1;
        }
    }
}

fn append_log(path: &str, max_kb: u64, line: &str) {
    use std::io::Write;

    let _ = std::fs::create_dir_all("/mnt/data");

    if let Ok(metadata) = std::fs::metadata(path) {
        if metadata.len() >= max_kb.saturating_mul(1_024) {
            let _ = std::fs::rename(path, format!("{path}.1"));
        }
    }

    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

fn log_event(inner: &Arc<NetguardInner>, message: &str) {
    let config = inner.config.get_network_guard();
    let timestamp = Utc::now().to_rfc3339();
    append_log(
        &config.log_path,
        config.log_max_kb,
        &format!("{timestamp} {message}"),
    );
}

async fn dbus_signal_listener(notify: Arc<Notify>) {
    let Ok(conn) = Connection::system().await else {
        warn!("netguard: failed to open D-Bus connection for signal listener");
        return;
    };
    let Ok(proxy) = Proxy::new(
        &conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await
    else {
        return;
    };

    let rule = "type='signal',sender='org.ofono',interface='org.ofono.ConnectionContext',member='PropertyChanged'";
    if proxy.call::<_, _, ()>("AddMatch", &(rule,)).await.is_err() {
        return;
    }

    let mut stream = MessageStream::from(&conn);
    loop {
        let Some(message) = stream.next().await else {
            break;
        };
        if message.is_ok() {
            notify.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_default_route_with_gateway() {
        let table = "\
Iface   Destination     Gateway         Flags   RefCnt  Use     Metric  Mask            MTU     Window  IRTT
eth0    00000000        0101A8C0        0003    0       0       0       00000000        0       0       0
";
        let route = parse_proc_default_route(table).expect("default route");
        assert_eq!(route.interface, "eth0");
        assert_eq!(route.gateway.as_deref(), Some("192.168.1.1"));
    }

    #[test]
    fn parses_ip_default_route_with_gateway() {
        let route = parse_ip_default_route("default via 10.20.30.1 dev eth1 proto static")
            .expect("default route");
        assert_eq!(route.interface, "eth1");
        assert_eq!(route.gateway.as_deref(), Some("10.20.30.1"));
    }

    #[test]
    fn route_matches_expected_gateway_and_interface() {
        let route = Ipv4Route {
            interface: WAN_INTERFACE.to_string(),
            gateway: Some("10.1.2.3".to_string()),
        };

        assert!(route_matches(&route, Some("10.1.2.3")));
        assert!(!route_matches(&route, Some("10.1.2.4")));
        assert!(!route_matches(
            &Ipv4Route {
                interface: "usb0".to_string(),
                gateway: Some("10.1.2.3".to_string()),
            },
            Some("10.1.2.3")
        ));
    }

    #[test]
    fn radio_reset_requires_threshold_cooldown_and_settled_registration() {
        assert!(!should_reset_radio(1, 2, true, true));
        assert!(!should_reset_radio(2, 2, false, true));
        assert!(!should_reset_radio(2, 2, true, false));
        assert!(should_reset_radio(2, 2, true, true));
        assert!(should_reset_radio(3, 2, true, true));
    }

    #[test]
    fn parses_point_to_point_scope_link_gateway() {
        let table = "\
10.168.0.22 dev wan0 scope link
202.96.64.68 via 10.168.0.22 dev wan0
10.168.0.0/24 dev wan0 scope link
10.1.1.1 dev other0 scope link
";
        assert_eq!(
            parse_scope_link_gateway(table, "wan0").as_deref(),
            Some("10.168.0.22")
        );
        assert_eq!(parse_scope_link_gateway(table, "missing0"), None);
    }

    #[test]
    fn restore_command_candidates_prefer_ofono_gateway() {
        let candidates = candidate_replace_commands(Some("10.1.2.3"));
        assert_eq!(candidates[0][4], "via");
        assert_eq!(candidates[0][5], "10.1.2.3");
        assert!(candidates
            .iter()
            .any(|args| args.contains(&"onlink".to_string())));
    }
}
