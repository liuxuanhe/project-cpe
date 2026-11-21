# UDX710 后台管理系统

这是一个为市面上成品 5G CPE 设备开发的高级后台管理系统项目。本项目旨在为现有的 5G CPE 设备提供更多高级功能和可玩性，让用户能够更好地控制和定制他们的 5G CPE 设备。

基于 Rust + Axum + zbus 的 5G/LTE 调制解调器后端服务，通过 ofono D-Bus 接口控制。

> 当前版本：**3.5.0**
>
> 对比基线：**3.4.0（commit `9402522`）**
>
> 目标架构：`aarch64-unknown-linux-musl`

powered by Cursor Claude Opus 4.5 & Sonnet 4.5 & OpenAI GPT-5.1/5.2

欢迎pr 和 issue 看到后会尽快处理。

## 3.5.0 版本说明

3.5.0 以 3.4.0（commit `9402522`）为基线，重点解决 UDX710 在射频模式切换、基站重选和数据重拨后的 IPv4 路由“假活”问题，并加固 OTA 安装与 Modem 操作超时。该版本不修改厂商基带固件，所有恢复逻辑均在后台服务层完成。

### 问题背景

在部分网络事件后，ofono 的 `ConnectionContext.Active` 仍为 `true`，但 Linux 侧 IPv4 默认路由可能完全缺失、指向旧网关，或只恢复 IPv6。此时传统 watchdog 看到 `Active=true` 不会触发恢复，表现为：

- IPv6 可以访问；
- IPv4 `ping` 返回 `Network is unreachable`；
- Web 页面、App 和依赖 IPv4 的服务不可用。

### 3.4.0 与 3.5.0 对比

| 能力 | 3.4.0 | 3.5.0 |
| --- | --- | --- |
| IPv4 默认路由恢复 | 依赖 ofono/connman，模式切换后可能缺失 | NetGuard 每秒检查；事件后 10 秒内 250ms 快轮询；缺失或网关错误立即 `ip route replace` |
| “假活”识别 | 主要看 `Active` 属性 | 真实 ICMP 探测 + 期望/实际网关校验 |
| 数据恢复 | 单层 data watchdog | 路由修复 → 数据重拨 → CFUN 分级恢复 |
| 操作超时 | 无统一上限 | 重拨 8s、CFUN 单步 5s、路由检查 5s |
| 恢复竞争 | watchdog 与手动操作可能并发 | NetGuard 统一负责数据恢复；旧 watchdog 仅清理 iptables |
| 手动关闭数据 | 可能被 watchdog 重新打开 | 进入 `paused_by_user`，不再自动拉起 |
| 模式切换恢复 | 现场观测可能需要数十秒 | 目标 ≤15s；本机验证 1.4–4.7s |
| OTA 覆盖运行中二进制 | 可能报 `Text file busy` | 临时文件 + `rename` 原子替换 |
| 可观测性 | 无专门状态接口 | `GET /api/netguard/status` + 256KB 轮转日志 |
| 探测目标 | 固定/单目标 | 主 `223.5.5.5` + 备 `119.29.29.29`，均可配置 |

### 1. NetGuard 三层自愈

#### RouteReconcile：路由快速修复

- 读取 `/proc/net/route`，失败时回退到 `ip -4 route`。
- 每秒检查默认路由；收到 ofono 事件或模式切换后，10 秒内切换到 250ms 快轮询。
- 期望网关优先取 ofono `Settings.Gateway`；缺失时依次回退到 WAN scope-link 主机路由和 WAN 本地 IPv4 地址。
- 路由缺失或网关不匹配时执行 `ip route replace default ...`，成功后增加 `route_repairs`，不进入冷却。

#### DataReactivate：健康探测与数据重拨

- 每 2 秒探测 `223.5.5.5` 和 `119.29.29.29`，任一成功即视为连通。
- 连续失败 2 次触发修复；连续成功 3 次恢复 `healthy`。
- 数据连接未激活时也会主动拉起，不再只处理“Active=true 但路由缺失”的场景。
- 单次数据重拨预算 8 秒，避免长时间占用 D-Bus/AT 串行锁。

#### RadioReset：协议栈重启

- 仅在数据重拨连续失败 2 次、注册状态正常且注册已稳定 10 秒后触发。
- 执行 `AT+CFUN=0` / `AT+CFUN=1`，单步超时 5 秒。
- 10 分钟冷却，避免频繁重启协议栈。

### 2. 事件驱动快速窗口

以下操作成功后会立即安排 1 秒后的 reconcile：

- `POST /api/data`
- `POST /api/radio-mode`
- `POST /api/band-lock`
- `POST /api/cell-lock`
- `POST /api/cell-lock/unlock-all`

同时监听 ofono `ConnectionContext.PropertyChanged`，通过 `tokio::sync::Notify` 唤醒路由循环，不再依赖固定 40 秒延迟。

### 3. 消除重复恢复竞争

当 `network_guard.enabled=true`：

- `data_connection_watchdog` 只保留 iptables 清理，不再激活数据连接。
- 数据恢复统一由 NetGuard 负责，避免两个流程同时调用 `SetProperty("Active", true)`。

### 4. 操作超时与防锁死

- `serial::with_serial_timeout` 在全局 D-Bus/AT 锁上增加超时；超时后取消操作并释放锁。
- `set_data_connection_with_timeout` 默认 8 秒。
- `send_at_command_with_timeout` 默认 5 秒。
- 路由检查外层 5 秒超时。

这些限制用于避免串口/D-Bus 操作长时间阻塞，导致后续请求排队。

### 5. 手动数据开关语义

- `POST /api/data {"active": false}` 会设置 `manual_data_off=true`，状态进入 `paused_by_user`。
- 自动激活暂停，直到用户显式设置 `{"active": true}` 或服务重启。
- `GET /api/netguard/status` 中的 `manual_data_off` 可直接观察该状态。

### 6. OTA 原子安装

- 新二进制先写入 `/home/root/.udx710.new`，`chmod 755` 后通过 `rename` 覆盖 `/home/root/udx710`。
- 前端先复制到 `/home/root/www.new`，再切换到 `/home/root/www`。
- 运行中的旧进程继续持有旧 inode，避免覆盖正在执行的二进制时出现 `Text file busy`。
- 保留旧版 loader hook 兼容逻辑。

### 7. 可观测性

- `GET /api/netguard/status` 返回状态机阶段、探测结果、路由修复次数、恢复耗时、CFUN 冷却和手动关闭状态。
- `/mnt/data/netguard.log` 记录 `route_reconcile`、`data_reactivate`、`radio_reset` 等事件，默认 256KB 轮转。
- 状态接口示例：

```json
{
  "status": "ok",
  "message": "Success",
  "data": {
    "enabled": true,
    "state": "healthy",
    "health": "ok",
    "consecutive_failures": 0,
    "consecutive_successes": 3,
    "last_error": null,
    "last_recovery_ms": 2152,
    "route_repairs": 1,
    "data_reactivate_attempts": 2,
    "data_reactivate_successes": 2,
    "cfun_last_triggered_at": null,
    "cfun_in_cooldown": false,
    "probe_targets": ["223.5.5.5", "119.29.29.29"],
    "last_route_check_at": "2026-11-21T17:33:12Z",
    "expected_gateway": "10.0.0.1",
    "observed_gateway": "10.0.0.1",
    "route_ok": true,
    "manual_data_off": false
  }
}
```

### 8. 状态机

| 状态 | 含义 |
| --- | --- |
| `initializing` | 服务启动中 |
| `waiting_registration` | 等待网络注册 |
| `waiting_data` | 已注册，等待数据接口获得 IPv4 |
| `activating_data` | 正在激活数据连接 |
| `data_inactive` | 数据连接未激活，等待重试 |
| `probing` | 数据已激活，正在探测连通性 |
| `healthy` | 探测成功，路由正常 |
| `route_reconcile` | 正在修复默认路由 |
| `route_reconcile_stalled` | 路由修复失败 |
| `data_reactivate_stalled` | 数据重拨失败 |
| `radio_reset_stalled` | CFUN 后仍未恢复 |
| `paused_by_user` | 用户手动关闭数据，自动激活暂停 |

### 9. 配置示例

旧版 `config.json` 不包含 `network_guard` 时会自动使用默认值，不需要手动迁移。

```json
{
  "network_guard": {
    "enabled": true,
    "probe_target": "223.5.5.5",
    "probe_backup_target": "119.29.29.29",
    "route_poll_ms": 1000,
    "health_interval_ms": 2000,
    "fast_window_ms": 10000,
    "fast_poll_ms": 250,
    "health_failure_threshold": 2,
    "health_recovery_threshold": 3,
    "data_reactivate_timeout_ms": 8000,
    "cfun_step_timeout_ms": 5000,
    "cfun_fail_threshold": 2,
    "cfun_cooldown_secs": 600,
    "log_path": "/mnt/data/netguard.log",
    "log_max_kb": 256
  }
}
```

### 10. 现场验证

以下为 UDX710 + 中国联通网络上的验证结果，仅代表该环境：

| 场景 | 结果 |
| --- | --- |
| 重启后数据自动拉起 | 1.0–3.0 秒进入 `healthy` |
| 删除默认路由 | 0.01 秒补回（connman 即时），NetGuard 同时记录修复 |
| 写入错误网关 | 0.13–0.56 秒纠正 |
| 切换 LTE-only | 1.4 秒恢复 |
| 切回 5G/LTE auto | 4.7 秒恢复 |
| 手动关闭数据 | 10 秒内保持关闭，自愈不抢跑 |
| 重新打开数据 | 1.7 秒路由恢复，6.1 秒完全健康 |
| ICMP 丢包 | `223.5.5.5` 20/20、0%；`119.29.29.29` 10/10、0% |
| 模组温度 | 约 42.7–46.8°C |
| CFUN | 未触发，冷却状态 `false` |

### 11. 已知边界

- 3.5.0 不修改厂商基带固件；如果后台服务未运行，厂商固件的路由缺陷仍可能出现。
- CFUN 是最后手段，默认 10 分钟最多执行一次；频繁触发应检查信号、SIM、APN 和运营商状态。
- `usb0` 的 `rx_errors` 是 RNDIS/USB gadget 统计，不等同于蜂窝链路丢包；排障应结合 WAN 接口统计和实际 `ping` 结果。
- 部分 DNS 会屏蔽 ICMP，备用探测目标可按网络环境调整。
- 路由修复依赖 Linux `ip` 命令和 root 权限，成品模组默认满足。

---

## 免责声明

本项目仅供技术交流和学习使用，不得用于任何非法用途。任何使用本项目造成的任何后果，均与本项目无关，由使用者自行承担。

且目前测试通过的设备仅有：

- 华为5G 通讯壳 P50 P60 Mate系列

其余设备由于缺少设备，本人未做测试，你如果手里有多余的设备，可尝试*小心的*尝试使用，但不提供任何 担保或保证。对设备的造成任何的损坏 本人不承担任何责任。

或者愿意捐献设备来测试，可联系我，我将在第一时间进行测试并更新本项目。

## ⚖️ 开源协议声明

本项目采用 GNU General Public License v3.0 (GPLv3) 开源协议

鉴于目前大部分人对版权意识薄弱，特此声明

本项目采用 GPLv3 开源协议，您可以自由使用、研究、修改本软件，但必须保留所有版权声明和许可证声明，并且公开源代码，任何基于本项目的衍生作品也必须使用 GPLv3 协议。

### ✅ 您可以

- 自由使用、研究、修改本软件
- 分发本软件的副本
- 分发修改后的版本

### ⚠️ 但您必须

1. **保留所有版权声明和许可证声明** - 不得删除或修改原作者的版权信息
2. **公开源代码** - 如果您分发本软件或其修改版本，必须以 GPLv3 协议公开完整源代码
3. **使用相同协议** - 任何基于本项目的衍生作品也必须使用 GPLv3 协议
4. **标注修改** - 修改后的版本必须明确标注修改内容和修改日期
5. **提供许可证副本** - 分发时必须附带完整的 GPLv3 许可证文本

### ❌ 严禁以下行为

- **禁止闭源商业化**：不得将本项目或其衍生版本闭源后进行商业销售
- **禁止删除版权信息**：不得移除原作者的版权声明
- **禁止更改许可证**：不得将本项目改为其他许可证（如 MIT、Apache 等）
- **禁止专有软件化**：不得将本项目整合到专有/闭源软件中而不开源

## 🚀 快速开始

### 构建后端

```bash
# 交叉编译 (macOS -> Linux aarch64)
./scripts/build.sh

# 带 UPX 压缩
./scripts/build.sh --upx
```

### 构建前端

```bash
cd frontend && pnpm install && pnpm run build
```

### 部署

```bash
./scripts/deploy.sh
```

### 打包 OTA

```bash
./scripts/pack-ota.sh
```

输出文件：`release/udx710-ota-<version>.tar.gz`

---

## 📁 项目结构

| 目录 | 说明 |
| --- | --- |
| `backend/` | Rust + Axum 后端，包含 ofono D-Bus、NetGuard、OTA、短信与通话等模块 |
| `frontend/` | React 前端管理界面 |
| `scripts/` | 构建、部署、OTA 打包与网络恢复脚本 |
| `bruno-api/` | Bruno API 调试集合 |
| `release/` | 本地 OTA 产物目录（不纳入版本库） |

---

## 🔄 升级与回滚

### 从 3.4.0 升级到 3.5.0

1. 保留当前运行的 OTA 包，并记录 SHA256/MD5。
2. 构建后端、前端并生成 OTA 包：

```bash
./scripts/build.sh
cd frontend && pnpm install && pnpm run build && cd ..
./scripts/pack-ota.sh
```

3. 通过 Web 管理页的 OTA 页面，或调用 `POST /api/ota/upload` 上传 `release/udx710-ota-3.5.0.tar.gz`。
4. 调用 `POST /api/ota/apply`，或使用页面上的“安装并重启”。
5. 重启后检查：

```bash
curl http://192.168.66.1/api/health
curl http://192.168.66.1/api/netguard/status
```

### 回滚到上一版本

优先使用 OTA 页面重新上传上一版本 OTA 包并安装。若后台服务无法启动，可在已确认备份完整的前提下，通过 ADB 恢复设备内保留的二进制和前端目录：

```sh
adb shell 'cp /home/root/udx710.3.4.0.bak /home/root/udx710'
adb shell 'chmod 755 /home/root/udx710'
adb shell 'rm -rf /home/root/www && cp -a /home/root/www.3.4.0.bak /home/root/www'
adb shell 'sync && reboot'
```

> 回滚前请确认备份文件存在且校验通过，并保持设备供电稳定。`backup/`、`release/` 属于本地运维产物，不应提交到版本库。

---

## 🔧 环境配置 (macOS)

```bash
# 1. 安装 Rust
brew install rust rustup
rustup default stable
rustup target add aarch64-unknown-linux-musl

# 2. 安装交叉编译工具链
brew tap messense/macos-cross-toolchains
brew install aarch64-unknown-linux-musl

# 3. 验证
rustup target list --installed
which aarch64-unknown-linux-musl-gcc
```

---

## 📡 ofono D-Bus 接口

### 核心接口

| 接口 | 说明 |
|------|------|
| `org.ofono.Manager` | 调制解调器管理 |
| `org.ofono.Modem` | Modem 属性和控制 |
| `org.ofono.NetworkRegistration` | 网络注册状态 |
| `org.ofono.SimManager` | SIM 卡管理 |
| `org.ofono.ConnectionManager` | 数据连接管理 |
| `org.ofono.VoiceCallManager` | 语音通话管理 |
| `org.ofono.MessageManager` | 短信管理 |

### 常用 D-Bus 命令

```bash
# 查看 Modem 属性
dbus-send --system --print-reply \
  --dest=org.ofono /ril_0 org.ofono.Modem.GetProperties

# 查看网络状态
dbus-send --system --print-reply \
  --dest=org.ofono /ril_0 org.ofono.NetworkRegistration.GetProperties

# 查看 SIM 卡信息
dbus-send --system --print-reply \
  --dest=org.ofono /ril_0 org.ofono.SimManager.GetProperties

# 设置飞行模式
dbus-send --system --print-reply \
  --dest=org.ofono /ril_0 org.ofono.Modem.SetProperty \
  string:"Online" variant:boolean:false

# 发送 AT 指令
dbus-send --system --print-reply \
  --dest=org.ofono /ril_0 org.ofono.Modem.SendAtcmd \
  string:"AT+CGSN"
```

### 监控 D-Bus

```bash
# 监听 ofono 发出的所有信号
dbus-monitor --system "sender='org.ofono'"

# 监听发给 ofono 的调用
dbus-monitor --system "destination='org.ofono'"

# 监听短信信号
dbus-monitor --system "interface='org.ofono.MessageManager'"
```

---

## 📶 频段锁定

仅供参考 真实性有待考证，请以实际设备为准

### LTE (4G) 频段

| 频段 | 位掩码 | 说明 |
|------|--------|------|
| B1 | 1 | FDD 2100MHz |
| B3 | 4 | FDD 1800MHz |
| B5 | 16 | FDD 850MHz |
| B8 | 128 | FDD 900MHz |
| B38 | 32 (TDD) | TDD 2600MHz |
| B40 | 128 (TDD) | TDD 2300MHz |
| B41 | 256 (TDD) | TDD 2500MHz |

### NR (5G) 频段

| 频段 | 位掩码 | 说明 |
|------|--------|------|
| N1 | 1 (FDD) | 2100MHz |
| N28 | 512 (FDD) | 700MHz |
| N41 | 16 (TDD) | 2500MHz |
| N77 | 128 (TDD) | 3700MHz |
| N78 | 256 (TDD) | 3500MHz |
| N79 | 512 (TDD) | 4500MHz |

### AT 指令

```bash
# 查询当前 LTE 频段
AT+SPLBAND=0

# 查询当前 NR 频段
AT+SPLBAND=3

# 锁定 LTE B1+B3
AT+SPLBAND=1,0,0,0,0,5,0

# 锁定 NR N78
AT+SPLBAND=2,0,0,256,0

# 解锁所有频段
AT+SPLBAND=1,0,0,0,0,0,0
AT+SPLBAND=2,0,0,0,0
```

---

## 📚 API 接口文档

### 基础信息
| 接口 | 方法 | 说明 |
|------|------|------|
| `/api/health` | GET | 健康检查 |
| `/api/device` | GET | 设备信息 (IMEI/ICCID/型号) |
| `/api/device/imeisv` | GET | 软件版本号 |
| `/api/sim` | GET | SIM 卡信息 |
| `/api/sim/slot` | GET | SIM 卡槽状态 |
| `/api/sim/slot/switch` | POST | 切换 SIM 卡槽 |

### 网络状态
| 接口 | 方法 | 说明 |
|------|------|------|
| `/api/network` | GET | 网络注册信息 |
| `/api/network/interfaces` | GET | 网络接口信息 |
| `/api/network/signal-strength` | GET | 信号强度 |
| `/api/network/nitz` | GET | 网络时间 |
| `/api/network/operators` | GET | 运营商列表 |
| `/api/network/operators/scan` | GET | 扫描运营商 (耗时) |
| `/api/network/register-manual` | POST | 手动注册运营商 |
| `/api/network/register-auto` | POST | 自动注册运营商 |
| `/api/cells` | GET | 基站信息 |
| `/api/location/cell-info` | GET | 基站定位参数 |
| `/api/qos` | GET | QoS 信息 |

### 模块控制
| 接口 | 方法 | 说明 |
|------|------|------|
| `/api/data` | GET/POST | 数据连接开关 |
| `/api/roaming` | GET/POST | 漫游开关 |
| `/api/airplane-mode` | GET/POST | 飞行模式开关 |
| `/api/radio-mode` | GET/POST | 射频模式 (4G/5G/自动) |
| `/api/band-lock` | GET/POST | 频段锁定 |
| `/api/cell-lock` | GET/POST | 小区锁定 |
| `/api/cell-lock/unlock-all` | POST | 解锁所有小区 |
| `/api/apn` | GET/POST | APN 配置 |
| `/api/usb-mode` | GET/POST | USB 模式切换 |
| `/api/usb-advance` | POST | 高级 USB 模式设置 |

### 通话功能
| 接口 | 方法 | 说明 |
|------|------|------|
| `/api/calls` | GET | 当前通话列表 |
| `/api/call/dial` | POST | 拨打电话 |
| `/api/call/hangup` | POST | 挂断指定电话 |
| `/api/call/hangup-all` | POST | 挂断所有电话 |
| `/api/call/answer` | POST | 接听来电 |
| `/api/call/volume` | GET/POST | 通话音量设置 |
| `/api/call/forwarding` | GET/POST | 呼叫转移设置 |
| `/api/call/settings` | GET/POST | 通话设置 |
| `/api/call/history` | GET | 通话记录列表 |
| `/api/call/history/{id}` | DELETE | 删除指定通话记录 |
| `/api/call/history/clear` | POST | 清空通话记录 |

### 短信功能
| 接口 | 方法 | 说明 |
|------|------|------|
| `/api/sms/send` | POST | 发送短信 |
| `/api/sms/list` | GET | 短信列表 |
| `/api/sms/conversation` | GET | 短信会话列表 |
| `/api/sms/stats` | GET | 短信统计 |
| `/api/sms/clear` | POST | 清空短信 |

### IMS/VoLTE
| 接口 | 方法 | 说明 |
|------|------|------|
| `/api/ims/status` | GET | IMS 状态 |
| `/api/voicemail/status` | GET | 语音信箱状态 |

### 系统信息
| 接口 | 方法 | 说明 |
|------|------|------|
| `/api/stats` | GET | 系统统计（网速/内存/运行时间） |
| `/api/stats/cpu` | GET | CPU 信息 |
| `/api/connectivity` | GET | 网络连通性检查 |
| `/api/netguard/status` | GET | NetGuard 状态机、路由、探测与恢复统计 |
| `/api/system/reboot` | POST | 重启系统 |
| `/api/at` | POST | 执行 AT 指令 |

### Webhook 配置
| 接口 | 方法 | 说明 |
|------|------|------|
| `/api/webhook/config` | GET/POST | Webhook 配置管理 |
| `/api/webhook/test` | POST | 测试 Webhook |

### OTA 更新
| 接口 | 方法 | 说明 |
|------|------|------|
| `/api/ota/status` | GET | OTA 更新状态 |
| `/api/ota/upload` | POST | 上传 OTA 包 (最大 50MB) |
| `/api/ota/apply` | POST | 应用 OTA 更新 |
| `/api/ota/cancel` | POST | 取消 OTA 更新 |

---

## 🛠 开发指南

### D-Bus 操作序列化

所有 D-Bus/AT 操作必须通过 `with_serial` 串行执行：

```rust
use crate::serial::with_serial;

pub async fn send_at_command(conn: &Connection, cmd: &str) -> zbus::Result<String> {
    with_serial(async {
        let proxy = Proxy::new(conn, "org.ofono", "/ril_0", "org.ofono.Modem").await?;
        proxy.call("SendAtcmd", &(cmd)).await
    }).await
}
```

### API 响应格式

```rust
#[derive(Serialize)]
pub struct ApiResponse<T> {
    pub status: String,   // "ok" 或 "error"
    pub message: String,
    pub data: Option<T>,
}
```

### 测试

```bash
# 后端单元测试
cd backend && cargo test

# 前端生产构建
cd frontend && pnpm run build
```

NetGuard 的单元测试覆盖路由表解析、网关匹配、点对点 scope-link 网关推断、CFUN 触发条件和旧配置兼容性。

---

## 📦 依赖

- **zbus 5.x** - D-Bus 客户端
- **tokio 1.48** - 异步运行时
- **axum 0.8** - Web 框架
- **rusqlite 0.32** - SQLite (bundled)
- **tower-http 0.6** - HTTP 中间件

---

## license 许可证

GNU General Public License v3.0
