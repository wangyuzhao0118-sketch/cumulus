# 在 Colab VM 上运行（客户端每 3 秒通过 ssh 送进来一次）：一次性收集状态，按行输出。
# 参数：输出目录  只列出修改时间晚于此的文件（秒）  是否附带 ComfyUI 日志末尾(0/1)
#   gpu <利用率%>\t<显存已用MB>\t<显存总MB>\t<温度>\t<功耗W>\t<名称>
#   mem <已用字节>\t<总字节>
#   disk <已用字节>\t<总字节>
#   comfy up|down
#   queue <运行中>\t<排队>
#   qmodel <prompt_id>\t<模型文件名>   排队 / 运行中的工作流用到的模型（客户端据此自动复制到 SSD）
#   out <相对路径>\t<大小>\t<修改时间>
#   log <一行 ComfyUI 日志>
import json, os, shutil, subprocess, sys, urllib.request

out_dir = sys.argv[1] if len(sys.argv) > 1 else "/content/ComfyUI/output"
since = float(sys.argv[2]) if len(sys.argv) > 2 else 0.0
want_log = len(sys.argv) > 3 and sys.argv[3] == "1"

try:
    r = subprocess.run(
        ["nvidia-smi", "--query-gpu=utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw,name",
         "--format=csv,noheader,nounits"], capture_output=True, text=True, timeout=5)
    line = r.stdout.strip().splitlines()[0] if r.returncode == 0 and r.stdout.strip() else ""
    if line:
        print("gpu " + "\t".join(p.strip() for p in line.split(",")))
except Exception:
    pass

try:
    info = {}
    for l in open("/proc/meminfo"):
        k, v = l.split(":", 1)
        info[k] = int(v.split()[0]) * 1024
    print(f"mem {info['MemTotal'] - info['MemAvailable']}\t{info['MemTotal']}")
except Exception:
    pass

try:
    d = shutil.disk_usage("/content")
    print(f"disk {d.used}\t{d.total}")
except Exception:
    pass

try:
    q = json.load(urllib.request.urlopen("http://127.0.0.1:8188/queue", timeout=2))
    print("comfy up")
    print(f"queue {len(q.get('queue_running', []))}\t{len(q.get('queue_pending', []))}")
    EXTS = (".safetensors", ".sft", ".ckpt", ".pt", ".pth", ".bin", ".gguf", ".onnx", ".pkl")

    def names(v, out):
        if isinstance(v, dict):
            for x in v.values():
                names(x, out)
        elif isinstance(v, list):
            for x in v:
                names(x, out)
        elif isinstance(v, str) and "://" not in v and "\n" not in v and v.lower().endswith(EXTS):
            out.add(v.replace("\\", "/"))

    for item in q.get("queue_running", []) + q.get("queue_pending", []):
        try:
            found = set()
            names(item[2], found)
            for n in sorted(found):
                print(f"qmodel {item[1]}\t{n}")
        except Exception:
            pass
except Exception:
    print("comfy down")

MEDIA = (".png", ".jpg", ".jpeg", ".webp", ".gif", ".mp4", ".webm", ".mov", ".mkv", ".wav", ".mp3", ".flac", ".glb", ".obj")
n = 0
if os.path.isdir(out_dir):
    for root, dirs, files in os.walk(out_dir):
        dirs[:] = [x for x in dirs if not x.startswith(".")]
        for f in files:
            if f.startswith(".") or not f.lower().endswith(MEDIA):
                continue
            p = os.path.join(root, f)
            try:
                st = os.stat(p)
            except OSError:
                continue
            if st.st_mtime > since:
                print(f"out {os.path.relpath(p, out_dir)}\t{st.st_size}\t{st.st_mtime:.3f}")
                n += 1
                if n >= 500:
                    break
        if n >= 500:
            break

if want_log:
    try:
        with open("/content/comfyui.log", "rb") as fh:
            fh.seek(0, 2)
            size = fh.tell()
            fh.seek(max(0, size - 64 * 1024))
            data = fh.read().decode("utf-8", "replace").replace("\r", "\n")
        for l in data.splitlines()[-300:]:
            print("log " + l)
    except Exception:
        pass
