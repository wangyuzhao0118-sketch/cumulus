#!/usr/bin/env bash
# 在 Colab VM 上运行（客户端通过 ssh 以 `bash -s` 方式送进来）。
# 安装 ComfyUI + 常用自定义节点，接上 Google Drive 里的模型，启动 ComfyUI（只监听 127.0.0.1:8188，
# 客户端用 SSH 端口转发访问）。
#
# 输入（环境变量）：
#   DRIVE_ROOT    Drive 上的 ComfyUI 资料目录（含 models/ input/ workflows/ 等），为空或不存在时不接 Drive
#   CUSTOM_NODES  每行一个 git 地址
#   EXTRA_ARGS    追加给 ComfyUI main.py 的参数
#   OUTPUT_DRIVE  1 = 出图直接写到 Drive 的 output/ 目录
#
# 输出协议（客户端逐行解析，其余行当日志显示）：
#   @@STAGE <key> <说明>     进入某个阶段
#   @@READY <秒数>           ComfyUI 可以访问了
#   @@FAIL <原因>            失败
set -uo pipefail
export PIP_DISABLE_PIP_VERSION_CHECK=1 PYTHONUNBUFFERED=1 GIT_TERMINAL_PROMPT=0
T0=$(date +%s)
COMFY=/content/ComfyUI
LOG=/content/comfyui.log
DRIVE_ROOT="${DRIVE_ROOT:-}"
fail() { echo "@@FAIL $*"; exit 1; }
stage() { echo "@@STAGE $1 $2"; }

up() { curl -sf -m 3 http://127.0.0.1:8188/system_stats >/dev/null 2>&1; }

busy() {
  curl -sf -m 3 http://127.0.0.1:8188/queue 2>/dev/null \
    | python3 -c 'import json,sys; q=json.load(sys.stdin); sys.exit(0 if q.get("queue_running") or q.get("queue_pending") else 1)' 2>/dev/null
}

if up; then
  # 旧版本生成的模型目录配置里没有本地 SSD：没有任务在跑就重启 ComfyUI 换上新配置
  if [ -n "$DRIVE_ROOT" ] && [ -d "$DRIVE_ROOT/models" ] \
     && ! grep -q '^cumulus_ssd:' "$COMFY/extra_model_paths.yaml" 2>/dev/null && ! busy; then
    echo "模型目录配置是旧版本，重启 ComfyUI 以启用本地 SSD"
    kill "$(cat /content/comfyui.pid 2>/dev/null)" 2>/dev/null || pkill -f 'main.py --listen 127.0.0.1 --port 8188'
    for _ in $(seq 1 20); do up || break; sleep 1; done
    if up; then pkill -9 -f 'main.py --listen 127.0.0.1 --port 8188'; sleep 2; fi
  else
    echo "ComfyUI 已经在运行"
    echo "@@READY 0"
    exit 0
  fi
fi

pipi() {
  # 优先用 uv（快很多），没有就用 pip；已经满足的包不会重装（不会动 Colab 自带的 torch）
  local out rc
  if command -v uv >/dev/null 2>&1; then
    out="$(uv pip install --system -q "$@" 2>&1)"; rc=$?
  else
    out="$(python3 -m pip install -q "$@" 2>&1)"; rc=$?
  fi
  [ -n "$out" ] && printf '%s\n' "$out" | tail -n 20
  return $rc
}

stage install "安装 ComfyUI"
[ -f "$COMFY/.nimbus_ok" ] && mv "$COMFY/.nimbus_ok" "$COMFY/.cumulus_ok"   # 改名前的安装标记
if [ ! -f "$COMFY/.cumulus_ok" ]; then
  if [ ! -d "$COMFY/.git" ]; then
    rm -rf "$COMFY"
    git clone -q --depth 1 https://github.com/comfyanonymous/ComfyUI "$COMFY" || fail "下载 ComfyUI 失败"
  fi
  echo "安装 ComfyUI 依赖…"
  pipi -r "$COMFY/requirements.txt" || fail "安装 ComfyUI 依赖失败"
  touch "$COMFY/.cumulus_ok"
else
  echo "ComfyUI 已安装，跳过"
fi
(cd "$COMFY" && echo "ComfyUI 版本：$(git log -1 --format='%h %cd' --date=short 2>/dev/null)")

stage nodes "安装自定义节点"
mkdir -p "$COMFY/custom_nodes"
while IFS= read -r url; do
  url="$(echo "$url" | xargs)"
  [ -z "$url" ] && continue
  case "$url" in \#*) continue ;; esac
  name="$(basename "${url%.git}")"
  dir="$COMFY/custom_nodes/$name"
  if [ -d "$dir/.git" ]; then
    echo "· $name 已存在"
    continue
  fi
  echo "· 安装 $name"
  git clone -q --depth 1 "$url" "$dir" || { echo "  ! 下载 $name 失败，跳过"; continue; }
  if [ -f "$dir/requirements.txt" ]; then
    pipi -r "$dir/requirements.txt" || echo "  ! $name 的依赖没装全"
  fi
done <<< "${CUSTOM_NODES:-}"

stage config "配置模型目录"
ARGS=(--listen 127.0.0.1 --port 8188 --disable-auto-launch)
# 没有 GPU（选了 CPU）时 ComfyUI 默认找 CUDA 会直接退出
if ! nvidia-smi -L >/dev/null 2>&1; then
  echo "没有 GPU，以 CPU 模式运行 ComfyUI"
  ARGS+=(--cpu)
fi
if [ -n "$DRIVE_ROOT" ] && [ -d "$DRIVE_ROOT" ]; then
  echo "Drive 目录：$DRIVE_ROOT"
  if [ -d "$DRIVE_ROOT/models" ]; then
    # models/ 下每个子目录都映射成同名的模型类别；text_encoders / diffusion_models 另外给旧名字。
    # 先写本地 SSD（$SSD_ROOT/models，客户端把选中的模型复制过来），再写 Drive：
    # 同名文件 ComfyUI 取先找到的，所以复制到 SSD 的模型从 SSD 读，其余照旧从 Drive 读。
    SSD_ROOT="${SSD_ROOT:-/content/cumulus_ssd}"
    mkdir -p "$SSD_ROOT/models"
    cats() {
      for d in "$DRIVE_ROOT"/models/*/; do
        [ -d "$d" ] || continue
        n="$(basename "$d")"
        echo "    $n: models/$n"
        case "$n" in
          text_encoders) echo "    clip: models/$n" ;;
          diffusion_models) echo "    unet: models/$n" ;;
        esac
      done
    }
    {
      echo "cumulus_ssd:"
      echo "    base_path: \"$SSD_ROOT\""
      cats
      echo "cumulus_drive:"
      echo "    base_path: \"$DRIVE_ROOT\""
      cats
    } > "$COMFY/extra_model_paths.yaml"
    sed 's/^/  /' "$COMFY/extra_model_paths.yaml"
  fi
  # 工作流保存在 Drive 上，下次开机还在
  mkdir -p "$DRIVE_ROOT/workflows" "$COMFY/user/default"
  if [ ! -L "$COMFY/user/default/workflows" ]; then
    if [ -d "$COMFY/user/default/workflows" ]; then
      cp -n "$COMFY/user/default/workflows/"* "$DRIVE_ROOT/workflows/" 2>/dev/null || true
      rm -rf "$COMFY/user/default/workflows"
    fi
    ln -s "$DRIVE_ROOT/workflows" "$COMFY/user/default/workflows"
  fi
  # 上传的参考图也放 Drive
  mkdir -p "$DRIVE_ROOT/input"
  ARGS+=(--input-directory "$DRIVE_ROOT/input")
  if [ "${OUTPUT_DRIVE:-0}" = "1" ]; then
    mkdir -p "$DRIVE_ROOT/output"
    ARGS+=(--output-directory "$DRIVE_ROOT/output")
    echo "出图目录：$DRIVE_ROOT/output"
  fi
else
  echo "没有接入 Drive（目录不存在：${DRIVE_ROOT:-未设置}），只用 VM 本地的 models/"
  rm -f "$COMFY/extra_model_paths.yaml"
fi

stage start "启动 ComfyUI"
nvidia-smi --query-gpu=name,memory.total --format=csv,noheader 2>/dev/null | sed 's/^/GPU：/' || echo "GPU：无"
# shellcheck disable=SC2206
EXTRA=(${EXTRA_ARGS:-})
: > "$LOG"
cd "$COMFY"
setsid nohup python3 main.py "${ARGS[@]}" "${EXTRA[@]}" >> "$LOG" 2>&1 < /dev/null &
PID=$!
echo "$PID" > /content/comfyui.pid
echo "python3 main.py ${ARGS[*]} ${EXTRA[*]}"

# 一边等端口一边把 ComfyUI 的日志转出来
SEEN=0
for _ in $(seq 1 900); do
  N=$(wc -l < "$LOG")
  if [ "$N" -gt "$SEEN" ]; then
    sed -n "$((SEEN + 1)),${N}p" "$LOG"
    SEEN=$N
  fi
  if up; then
    echo "@@READY $(( $(date +%s) - T0 ))"
    exit 0
  fi
  if ! kill -0 "$PID" 2>/dev/null; then
    tail -n 30 "$LOG"
    fail "ComfyUI 进程退出了"
  fi
  sleep 1
done
fail "等了 15 分钟 ComfyUI 还没起来"
