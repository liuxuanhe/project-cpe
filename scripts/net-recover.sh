#!/bin/sh
# net-recover.sh - UDX710 / project-cpe 网络自愈脚本
#
# 解决的问题:
#   切换射频模式(LTE/NR/Auto)或快速重拨后, 模组可能出现
#   "数据连接 Active=true 但 IPv4 默认路由丢失" 的假活状态,
#   表现为 IPv6 能通、IPv4 全部超时 (ping: Network is unreachable)。
#   后台 watchdog 只检查 ofono 的 Active 属性, 检测不到该状态。
#
# 用法:
#   在模组上执行:
#     sh net-recover.sh                # 检查并修复一次
#     sh net-recover.sh --check        # 只检查, 不修复
#     sh net-recover.sh --watch 60     # 每 60 秒巡检, 异常自动修复
#
#   在电脑上远程执行 (需要电脑能访问模组管理地址):
#     BASE_URL=http://192.168.66.1 sh net-recover.sh --check
#
#   可选环境变量:
#     BASE_URL   后端地址, 默认自动探测 http://127.0.0.1 和 :3000
#     TIMEOUT    单次 API 超时秒数, 默认 15
#
# 退出码: 0=网络正常  1=已修复  2=修复失败
#
# 说明: 只依赖 busybox sh + curl, 适用于任意运行 project-cpe 的 UDX710 设备。

set -u

TIMEOUT="${TIMEOUT:-15}"
MODE="fix"
INTERVAL=60

log() {
    printf '[%s] %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*"
}

usage() {
    sed -n '3,24p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
}

while [ $# -gt 0 ]; do
    case "$1" in
        --check) MODE="check" ;;
        --watch)
            MODE="watch"
            if [ $# -gt 1 ] && [ "$2" -ge 5 ] 2>/dev/null; then
                INTERVAL="$2"
                shift
            fi
            ;;
        -h|--help) usage ;;
        *) log "未知参数: $1"; usage ;;
    esac
    shift
done

api_get() {
    curl -fsS -m "$TIMEOUT" "$BASE_URL$1" 2>/dev/null
}

api_post() {
    curl -fsS -m "$TIMEOUT" -X POST "$BASE_URL$1" \
        -H "Content-Type: application/json" -d "$2" 2>/dev/null
}

# 自动探测后端地址: 优先 BASE_URL, 其次本机 80 端口(成品模组), 最后 3000(默认)
detect_base_url() {
    if [ -n "${BASE_URL:-}" ]; then
        return 0
    fi
    for candidate in "http://127.0.0.1" "http://127.0.0.1:3000"; do
        if curl -fsS -m 3 "$candidate/api/health" >/dev/null 2>&1; then
            BASE_URL="$candidate"
            return 0
        fi
    done
    BASE_URL=""
    return 1
}

# 从 /api/connectivity 的 JSON 中提取某一路的结果: true/false
probe_result() {
    family="$1"
    body="$2"
    section="$(printf '%s' "$body" | sed -n "s/.*\"$family\":{\([^}]*\)}.*/\1/p")"
    case "$section" in
        *'"success":true'*) echo true ;;
        *) echo false ;;
    esac
}

check_once() {
    conn="$(api_get /api/connectivity)" || {
        log "无法调用 /api/connectivity, 后端不可达"
        return 2
    }
    ipv4="$(probe_result ipv4 "$conn")"
    ipv6="$(probe_result ipv6 "$conn")"
    log "连通性检查: IPv4=$ipv4 IPv6=$ipv6"

    if [ "$ipv4" = "true" ]; then
        return 0
    fi
    # 双栈全断通常是没注册/没数据连接, 不属于路由假活, 交给上层判断
    if [ "$ipv6" = "false" ]; then
        net="$(api_get /api/network)"
        data="$(api_get /api/data)"
        log "双栈均不通. 网络状态: $(printf '%s' "$net" | head -c 200)"
        log "数据连接: $(printf '%s' "$data" | head -c 200)"
        return 2
    fi
    log "检测到 IPv4 路由假活 (IPv6 正常)"
    return 1
}

wait_for_ipv4() {
    wait_seconds="$1"
    waited=0
    while [ "$waited" -lt "$wait_seconds" ]; do
        sleep 5
        waited=$((waited + 5))
        conn="$(api_get /api/connectivity)" || continue
        if [ "$(probe_result ipv4 "$conn")" = "true" ]; then
            log "IPv4 已恢复 (等待 ${waited}s)"
            return 0
        fi
    done
    return 1
}

# 修复策略: 先轻后重
repair() {
    log "步骤 1/3: 重新激活数据连接"
    api_post /api/data '{"active":false}' >/dev/null 2>&1
    sleep 3
    api_post /api/data '{"active":true}' >/dev/null 2>&1
    if wait_for_ipv4 20; then
        return 0
    fi

    log "步骤 2/3: 射频协议栈重启 (AT+CFUN=0/1)"
    api_post /api/at '{"cmd":"AT+CFUN=0"}' >/dev/null 2>&1
    sleep 5
    api_post /api/at '{"cmd":"AT+CFUN=1"}' >/dev/null 2>&1
    if wait_for_ipv4 60; then
        return 0
    fi

    log "步骤 3/3: 重启系统 (60 秒后自动恢复)"
    api_post /api/system/reboot '{}' >/dev/null 2>&1
    return 1
}

run_once() {
    check_once
    rc=$?
    [ "$rc" -eq 0 ] && return 0
    [ "$rc" -eq 2 ] && return 2

    if [ "$MODE" = "check" ]; then
        log "--check 模式: 只报告, 不执行修复"
        return 2
    fi

    repair
    if [ $? -eq 0 ]; then
        log "修复成功"
        return 1
    fi
    log "修复失败, 请检查信号/运营商状态后手动重启模组"
    return 2
}

detect_base_url || {
    log "找不到 project-cpe 后端, 请设置 BASE_URL(例如 http://192.168.66.1)"
    exit 2
}
log "使用后端: $BASE_URL"

if [ "$MODE" = "watch" ]; then
    log "进入巡检模式, 间隔 ${INTERVAL}s"
    while :; do
        run_once
        sleep "$INTERVAL"
    done
fi

run_once
exit $?
