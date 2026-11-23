# 在 Colab VM 上运行：把 Drive 上的模型复制到 VM 本地 SSD（ComfyUI 优先从这里读）。
# 参数：源文件  目标文件
# 输出：@@DL <已复制字节> <总字节>   @@DONE <路径>   @@FAIL <原因>   其余行是说明
#
# 为什么不直接从 /content/drive 读：Colab 的 Drive 挂载（DriveFS）每个文件只走一条连接，
# 没缓存过的文件只有 35–50 MB/s。这里改为直接调 Drive API，对同一个文件开 16 条连接分块下载
# （实测 L4 上约 300 MB/s）。访问令牌用 DriveFS 自己用的那个（来自 Colab VM 的元数据服务，
# 只在 VM 上使用）。任何一步不行就退回从挂载目录顺序复制。
#
# 中断后再次复制会接着来：API 方式按 128 MB 分块，已完成的块记在 .<文件名>.part.chunks；
# 顺序复制方式从 .part 的末尾接着写。
import json, os, sys, threading, time, urllib.parse, urllib.request

src, dst = sys.argv[1], sys.argv[2]
MYDRIVE = "/content/drive/MyDrive/"
CHUNK = int(os.environ.get("CUMULUS_CHUNK_MB", "128")) << 20
CONNS = int(os.environ.get("CUMULUS_CONNS", "16"))


def out(line):
    print(line, flush=True)


def fail(msg):
    out(f"@@FAIL {msg}")
    sys.exit(1)


try:
    total = os.path.getsize(src)
except OSError as e:
    fail(f"Drive 上找不到这个文件：{e.strerror}")

if os.path.exists(dst) and os.path.getsize(dst) == total:
    out(f"@@DL {total} {total}")
    out(f"@@DONE {dst}")
    sys.exit(0)

d = os.path.dirname(dst)
os.makedirs(d, exist_ok=True)
part = os.path.join(d, "." + os.path.basename(dst) + ".part")
chunks_log = part + ".chunks"
st = os.statvfs(d)
# .part 可能是稀疏文件（按块写入），按实际占用算
allocated = os.stat(part).st_blocks * 512 if os.path.exists(part) else 0
if st.f_bavail * st.f_frsize < total + (2 << 30) - allocated:
    fail("本地 SSD 空间不够")


# ---------------------------------------------------------------- Drive API

class Api:
    def __init__(self):
        self.lock = threading.Lock()
        self.tok = None
        self.refresh()

    def refresh(self):
        req = urllib.request.Request(
            "http://172.28.0.1:8009/computeMetadata/v1/instance/service-accounts/default/token",
            headers={"Metadata-Flavor": "Google"},
        )
        tok = json.loads(urllib.request.urlopen(req, timeout=15).read())["access_token"]
        with self.lock:
            self.tok = tok

    def open(self, url, extra=None, retry_auth=True):
        with self.lock:
            h = {"Authorization": "Bearer " + self.tok}
        h.update(extra or {})
        try:
            return urllib.request.urlopen(urllib.request.Request(url, headers=h), timeout=60)
        except urllib.error.HTTPError as e:
            if e.code == 401 and retry_auth:
                self.refresh()
                return self.open(url, extra, False)
            raise

    def resolve(self, rel, size):
        """MyDrive 下的相对路径 → 文件 ID（同名时取大小一致的那个）"""
        parent, names = "root", rel.split("/")
        for i, name in enumerate(names):
            esc = name.replace("\\", "\\\\").replace("'", "\\'")
            q = f"'{parent}' in parents and name = '{esc}' and trashed = false"
            url = "https://www.googleapis.com/drive/v3/files?" + urllib.parse.urlencode(
                {"q": q, "fields": "files(id,size,mimeType)", "supportsAllDrives": "true", "pageSize": "50"}
            )
            files = json.loads(self.open(url).read())["files"]
            last = i == len(names) - 1
            if last:
                files = [f for f in files if int(f.get("size") or -1) == size]
            else:
                files = [f for f in files if f.get("mimeType") == "application/vnd.google-apps.folder"]
            if not files:
                raise LookupError(f"Drive API 里找不到 {name}")
            parent = files[0]["id"]
        return parent


def api_copy():
    rel = os.path.realpath(src)
    if not rel.startswith(MYDRIVE):
        raise LookupError("不在「我的云端硬盘」里")
    api = Api()
    fid = api.resolve(rel[len(MYDRIVE):], total)
    url = f"https://www.googleapis.com/drive/v3/files/{fid}?alt=media&supportsAllDrives=true"

    n = (total + CHUNK - 1) // CHUNK
    done_set = set()
    if os.path.exists(part) and os.path.exists(chunks_log):
        with open(chunks_log) as f:
            done_set = {int(x) for x in f.read().split() if x.isdigit() and int(x) < n}
    else:
        # 没有分块记录的 .part（顺序复制留下的）无法按块续传，重来
        for p in (part, chunks_log):
            if os.path.exists(p):
                os.remove(p)
    fd = os.open(part, os.O_RDWR | os.O_CREAT, 0o644)
    os.ftruncate(fd, total)
    log = open(chunks_log, "a")
    todo = [i for i in range(n) if i not in done_set]
    progress = {"bytes": sum(min(CHUNK, total - i * CHUNK) for i in done_set), "err": None}
    lock = threading.Lock()
    out(f"Drive API 直连下载：{len(todo)}/{n} 块，{CONNS} 条连接")

    def worker():
        while True:
            with lock:
                if not todo or progress["err"]:
                    return
                i = todo.pop(0)
            start = i * CHUNK
            end = min(total, start + CHUNK) - 1
            for attempt in range(4):
                got = 0
                try:
                    r = api.open(url, {"Range": f"bytes={start}-{end}"})
                    if r.status != 206 and not (start == 0 and end == total - 1):
                        raise IOError(f"服务器没有按范围返回（{r.status}）")
                    pos = start
                    while True:
                        b = r.read(4 << 20)
                        if not b:
                            break
                        os.pwrite(fd, b, pos)
                        pos += len(b)
                        got += len(b)
                        with lock:
                            progress["bytes"] += len(b)
                    if pos != end + 1:
                        raise IOError(f"数据不完整（{pos - start}/{end - start + 1}）")
                    with lock:
                        log.write(f"{i}\n")
                        log.flush()
                    break
                except Exception as e:
                    with lock:
                        progress["bytes"] -= got
                    if attempt == 3:
                        with lock:
                            progress["err"] = f"第 {i} 块下载失败：{e}"
                        return
                    time.sleep(2 * (attempt + 1))

    ts = [threading.Thread(target=worker, daemon=True) for _ in range(min(CONNS, max(1, len(todo))))]
    for t in ts:
        t.start()
    while any(t.is_alive() for t in ts):
        time.sleep(0.5)
        out(f"@@DL {progress['bytes']} {total}")
    log.close()
    os.fsync(fd)
    os.close(fd)
    if progress["err"]:
        fail(progress["err"] + "（再次复制会接着下载）")
    os.replace(part, dst)
    os.remove(chunks_log)


# ---------------------------------------------------------------- 顺序复制（备用）

def fuse_copy():
    if os.path.exists(chunks_log):
        # API 方式留下的 .part 是按块随机写的，不能从末尾接着写
        for p in (part, chunks_log):
            if os.path.exists(p):
                os.remove(p)
    have = os.path.getsize(part) if os.path.exists(part) else 0
    if have > total:
        have = 0
    done, last = have, 0.0
    try:
        with open(src, "rb", buffering=0) as fi, open(part, "ab" if have else "wb") as fo:
            fi.seek(have)
            while True:
                chunk = fi.read(32 << 20)
                if not chunk:
                    break
                fo.write(chunk)
                done += len(chunk)
                now = time.time()
                if now - last > 0.5:
                    out(f"@@DL {done} {total}")
                    last = now
    except Exception as e:
        fail(f"复制中断：{e}")
    if done != total:
        fail(f"复制不完整（{done}/{total}）")
    os.replace(part, dst)


try:
    api_copy()
except SystemExit:
    raise
except Exception as e:
    out(f"Drive API 直连不可用（{e}），改为从挂载目录复制")
    fuse_copy()

if os.path.getsize(dst) != total:
    os.remove(dst)
    fail("复制后大小不对，已删除，请重试")
out(f"@@DL {total} {total}")
out(f"@@DONE {dst}")
