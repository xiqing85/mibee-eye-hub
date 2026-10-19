#!/usr/bin/env bash
# =============================================================================
# 覆盖率棘轮门禁（roadmap mibee-rec #6 M1 · 开发规范：最低 85%、目标 93%）
#
# 用法：
#   tools/check-coverage.sh            # 测量并与基线比较（CI 用）
#   tools/check-coverage.sh --update   # 测量并把新基线写回（棘轮上调时用）
#
# 口径：cargo llvm-cov --workspace --lib（默认 feature）。feature 门控模块
# （ocr/webrtc/mse/faces 等）在本口径下计 0%——它们的执行面在 CI 的
# feature-matrix job；棘轮只对口径一致性负责，别换算绝对值。
# 棘轮：基线只升不降（--update 仅在数字上涨时写入）；新模块（src/runtime/*
# 与未来能力 crate）一律 ≥85% 起步、目标 93%（评审规则，代码评审执行）。
# 权威口径 = CI coverage job：本地与 CI 有环境差（desktop/dbus/音频设备
# 路径，实测本地 79.37 vs CI 78.82）——调棘轮看 CI 数字，别用本地值。
# 退出码：0 = 达到或超过基线；1 = 低于基线。
# =============================================================================
set -u

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd) || { echo "✗ 无法定位脚本目录"; exit 1; }
root=$(git -C "$script_dir" rev-parse --show-toplevel 2>/dev/null) || { echo "✗ 不在 git 仓库内"; exit 1; }
cd "$root"

baseline_file="$script_dir/coverage-baseline.txt"
baseline=$(cat "$baseline_file" 2>/dev/null | grep -oP '^\d+(\.\d+)?' || true)
[ -n "$baseline" ] || { echo "✗ 基线文件无效：$baseline_file"; exit 1; }

if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
    echo "✗ 未安装 cargo-llvm-cov（rustup component add llvm-tools-preview && cargo install cargo-llvm-cov）"
    exit 1
fi

summary=$(cargo llvm-cov --workspace --lib --summary-only 2>/dev/null) || { echo "✗ 覆盖率测量失败"; exit 1; }

# TOTAL 行的 Lines Cover% 列（Region %、Functions % 之后第三个百分数）。
measured=$(echo "$summary" | awk '
    $1 == "TOTAL" {
        pct = ""; n = 0
        for (i = 2; i <= NF; i++) {
            if ($i ~ /%$/) { n++; if (n == 3) { pct = $i; break } }
        }
        gsub(/%/, "", pct); print pct
    }')
[ -n "$measured" ] || { echo "✗ 未能从汇总解析出 TOTAL 行覆盖率"; echo "$summary" | tail -3; exit 1; }

below=0
awk -v m="$measured" -v b="$baseline" 'BEGIN {
    exit !(m + 0.0001 >= b + 0)
}' || below=1

if [ "${1:-}" = "--update" ]; then
    if [ "$below" = "0" ]; then
        echo "$measured" > "$baseline_file"
        echo "✓ 基线上调：$baseline% → $measured%"
    else
        echo "✗ 测量值 $measured% 低于当前基线 $baseline%，拒绝下调棘轮"
        exit 1
    fi
elif [ "$below" = "0" ]; then
    echo "✓ 覆盖率 $measured% ≥ 基线 $baseline%（行覆盖，--workspace --lib，默认 feature）"
else
    echo "✗ 覆盖率 $measured% < 基线 $baseline% — 补测试，或确认数字上涨后 tools/check-coverage.sh --update 棘轮（只升不降）"
    exit 1
fi
