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
# 棘轮历史追加多个数字行时取最新（末行）——首行曾是历史最低， silently
# 冻结了棘轮（2026-10-21 抓到：多次上调全部无效，CI 一直比 76.5）。
baseline=$(grep -oE '^[0-9]+([.][0-9]+)?' "$baseline_file" 2>/dev/null | tail -1 || true)
[ -n "$baseline" ] || { echo "✗ 基线文件无效：$baseline_file"; exit 1; }

if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
    echo "✗ 未安装 cargo-llvm-cov（rustup component add llvm-tools-preview && cargo install cargo-llvm-cov）"
    exit 1
fi

# --ignore-filename-regex：组合根文件是装配序列（full.rs=引擎构造→桥
# 接线→起服务；web_server.rs=34 参依赖结构+原样转发），行为由设备冒烟
# 套件验证，不计入单测分母——否则共享装配会把棘轮拖成"为装配行数
# 买单"。有可测逻辑的装配模块（models/protocols 等）照常计入。

# 模型门控推理体（2026-10-21 二豁免类）：face/decision/audio_ai(classify
# 与 mod)/ocr(mod 与 det) 的未覆盖行几乎全部在「模型文件在位才执行」的
# 推理路径里——CI 无模型资产，这些行在默认口径下结构性不可执行（编译
# 面由 feature-matrix job 保证，执行面由带模型的本地 rig 实测）。与组合
# 根同纪律：文件整体出分母（含其已覆盖行）。非推理逻辑后迁入这些文件
# 时应搬出而不是扩豁免。
# 设备门控 I/O 体（同日三豁免类）：capture/{video,audio,audio_monitor}
# 与 gb28181_talkback 的未覆盖行是 V4L2/ALSA/cpal 的设备交互路径——CI
# runner 无摄像头/麦克风/扬声器，这些行结构性不可执行（执行面由 rig
# 冒烟套件验证，同组合根纪律）。同理：纯逻辑迁入这些文件时搬出测试而
# 不是扩豁免。
summary=$(cargo llvm-cov --workspace --lib --summary-only \
    --ignore-filename-regex 'src/runtime/(full|web_server)\.rs|streaming/src/(face|decision|meeting)\.rs|streaming/src/vlm/mod\.rs|streaming/src/audio_ai/(classify|mod)\.rs|streaming/src/ocr/(mod|det)\.rs|capture/src/(video|audio|audio_monitor)\.rs|web/src/gb28181_talkback\.rs' 2>/dev/null) || { echo "✗ 覆盖率测量失败"; exit 1; }

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
