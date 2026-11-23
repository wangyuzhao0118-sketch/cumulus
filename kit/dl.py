# 在 Colab VM 上运行：把模型下载到指定目录（通常是 Drive 上的 models/<类别>/）。
# 参数：URL  目标目录  [文件名]
# 环境变量：HF_TOKEN（Hugging Face 私有 / 需授权的模型）、CIVITAI_TOKEN
# 输出：@@NAME <文件名>   @@DL <已下载字节> <总字节>   @@DONE <路径>   @@FAIL <原因>
# 先写到同目录的 .<文件名>.part，下完再改名；中断后再次下载会从断点续传。
import os, re, sys, time, urllib.parse, urllib.request

url, dest = sys.argv[1], sys.argv[2]
name = sys.argv[3] if len(sys.argv) > 3 and sys.argv[3].strip() else ""
headers = {"User-Agent": "cumulus/0.1"}
host = urllib.parse.urlparse(url).netloc
if "huggingface.co" in host and os.environ.get("HF_TOKEN"):
    headers["Authorization"] = "Bearer " + os.environ["HF_TOKEN"]
if "civitai.com" in host and os.environ.get("CIVITAI_TOKEN"):
    url += ("&" if "?" in url else "?") + "token=" + urllib.parse.quote(os.environ["CIVITAI_TOKEN"])
# Hugging Face 的网页地址换成直接下载地址
url = re.sub(r"(huggingface\.co/[^/]+/[^/]+)/blob/", r"\1/resolve/", url)


def fail(msg):
    print(f"@@FAIL {msg}", flush=True)
    sys.exit(1)


def guess_name(resp):
    cd = resp.headers.get("Content-Disposition", "")
    m = re.search(r"filename\*=UTF-8''([^;]+)", cd) or re.search(r'filename="?([^";]+)"?', cd)
    if m:
        return urllib.parse.unquote(m.group(1)).strip()
    return urllib.parse.unquote(os.path.basename(urllib.parse.urlparse(resp.geturl()).path)) or "model.bin"


os.makedirs(dest, exist_ok=True)
try:
    resp = urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=60)
except urllib.error.HTTPError as e:
    hint = {401: "需要登录或令牌（设置里填 HF / Civitai 令牌）", 403: "没有权限，可能需要先在网页上同意模型许可", 404: "地址不存在"}.get(e.code, "")
    fail(f"HTTP {e.code} {hint}")
except Exception as e:
    fail(f"连接失败：{e}")

name = os.path.basename(name or guess_name(resp))
print(f"@@NAME {name}", flush=True)
final = os.path.join(dest, name)
part = os.path.join(dest, f".{name}.part")
if os.path.exists(final):
    print(f"@@DONE {final}", flush=True)
    sys.exit(0)

total = int(resp.headers.get("Content-Length") or 0)
have = os.path.getsize(part) if os.path.exists(part) else 0
mode = "wb"
if have and total and have < total:
    resp.close()
    h = dict(headers, Range=f"bytes={have}-")
    resp = urllib.request.urlopen(urllib.request.Request(resp.geturl(), headers=h), timeout=60)
    if resp.status == 206:
        mode = "ab"
    else:
        have = 0
else:
    have = 0

done, last = have, 0.0
try:
    with open(part, mode) as f:
        while True:
            chunk = resp.read(8 << 20)
            if not chunk:
                break
            f.write(chunk)
            done += len(chunk)
            now = time.time()
            if now - last > 0.5:
                print(f"@@DL {done} {total}", flush=True)
                last = now
except Exception as e:
    fail(f"下载中断：{e}")
if total and done < total:
    fail(f"下载不完整（{done}/{total}）")
os.replace(part, final)
print(f"@@DL {done} {done}", flush=True)
print(f"@@DONE {final}", flush=True)
