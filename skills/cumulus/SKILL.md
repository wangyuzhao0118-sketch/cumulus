---
name: cumulus
description: 用 Cumulus 命令行在 Google Colab 上跑 ComfyUI：开关 GPU、提交工作流出图 / 出视频、管理 Google Drive 里的模型、把模型复制到 VM 本地 SSD 加速。用户想用 ComfyUI 生成图片或视频、下载模型、查看 Colab GPU 状态或 CU 余额、开机关机时使用。
---

# Cumulus：Colab 上的 ComfyUI

`cumulus` 是 Cumulus（macOS 应用）自带的命令行。所有命令都交给正在掌管 VM 的进程执行（开着窗口就是窗口里的程序，否则自动起一个后台进程），所以和窗口互不冲突，界面会同步显示你做的事。

## 先确认能用

```bash
cumulus status --json
```

找不到命令时，可执行文件在 `Cumulus.app/Contents/MacOS/cumulus`（开发目录里是 `target/Cumulus.app/...`），让用户 `ln -sf` 到 PATH 里，或直接用完整路径。

## 约定

- **加 `--json`**：stdout 只有一个 JSON 对象，出错时是 `{"ok": false, "error": "..."}`，退出码 1。等待中的日志和进度在 stderr。
- **会等到完成的命令**：`up`、`down`、`run`、`download`、`ssd add`、`ssd from`、`ssd profile use`。它们可能要几分钟，放到后台跑，或者设足够长的超时；加 `--no-wait` 会立即返回。
- **`exec` 的退出码**是 VM 上那条命令的退出码。

## 必须遵守

1. **开机就在花钱（CU）。** 只在用户要出图、要操作 VM 时才 `cumulus up`。活干完了，除非用户说要留着，否则 `cumulus down`。`status` 里的 `cu.balance`、`cu.rate_per_hour`、`idle_shutdown_in_s` 可以告诉用户还剩多少、多久会自动关机。
2. **GPU 由用户定。** 不要擅自换贵的 GPU（`--gpu A100/H100`），用户没说就用设置里的（`status` 的 `gpu_setting`）。
3. **Google Drive 授权只能用户自己点。** 新 VM 挂 Drive 时，`up` 会在 stderr 打印授权地址，`status --json` 的 `drive.auth_url` 也有。把地址告诉用户，请用户在浏览器里点「允许」，等用户说好了再运行 `cumulus drive-continue`。不要自己去点授权页。
4. **删除要先问。** `rm-model` 会把模型从 Drive 删掉（进回收站），必须加 `--yes`；先征得用户同意再执行。`ssd profile use` 会删掉 SSD 上不在方案里的副本（Drive 上的不动），切换前告诉用户。
5. **不要回显令牌。** `config set hf_token …` 可以设置令牌，但不要把值写进回复里；`config` 也不会显示它们。
6. **`exec` 以 root 在 VM 上执行。** 可以用来查看（`nvidia-smi`、`ls`、`df -h`、`tail`），会改东西或删东西的命令先问用户。

## 出图 / 出视频

ComfyUI 只认 **API 格式**的工作流：每个节点是 `"<id>": {"class_type": ..., "inputs": {...}}`。界面格式（有 `nodes` 和 `links`）会被拒绝；需要用户在 ComfyUI 里用「工作流 → 导出 (API)」。

```bash
cumulus up                         # 已经开着会直接返回
cumulus run workflow_api.json      # 提交，等跑完，打印同步回本机的文件路径（--json 有 outputs[].local）
```

- **改参数**：直接改 JSON 里节点的 `inputs`，比如 `CLIPTextEncode` 的 `text`、`KSampler` 的 `seed` / `steps`、`EmptyLatentImage` 的 `width` / `height`。改好存成新文件，或者用 `cumulus run -` 从 stdin 传。
- **看结果**：`run` 返回的本机路径可以直接用读图工具查看。`cumulus outputs -n 10` 列出最近同步回来的文件。
- **出错时**：提交被拒，错误里有节点错误（缺节点、参数不对）；跑的时候出错，会给出异常和节点。`cumulus logs --comfy -n 80` 看 ComfyUI 自己的日志。缺模型时看下一节。
- **中断**：`cumulus interrupt` 中断正在跑的任务。
- **SSD 预热**：`run` 提交前会先把工作流用到、还不在 SSD 上的模型复制到 SSD（第一次要等复制，之后加载快很多），`--no-ssd` 跳过。

## 模型

模型放在 Drive 的 `models/<类别>/` 下，命令里写成 `类别/文件名`，比如 `loras/x.safetensors`。

```bash
cumulus models --json                      # 列表：key、size、on_ssd、ssd_selected
cumulus download loras <URL> [--name 文件名]  # 在 VM 上下载到 Drive（Hugging Face / Civitai / 直链）
cumulus workflows                          # ComfyUI 里保存的工作流
cumulus ssd from <工作流> --dry-run         # 工作流用到哪些模型、Drive 上缺哪些（缺的常带下载地址）
```

常见类别：`checkpoints`、`diffusion_models`（UNet）、`text_encoders`（CLIP / T5）、`vae`、`loras`、`controlnet`、`upscale_models`。需要登录才能下的模型（FLUX dev、部分 Civitai 模型）要先在设置里配好令牌。

## SSD 加速

从 Drive 读没缓存过的大模型很慢（每个文件 35–50 MB/s）。复制到 VM 本地 SSD 后，ComfyUI 会优先从 SSD 读。SSD 容量有限，关机后副本清空。

```bash
cumulus ssd list                            # 已选的模型，合计多大，SSD 放不放得下
cumulus ssd from <工作流>                    # 把工作流用到的模型复制到 SSD
cumulus ssd add <类别/文件名>...             # 指定模型
cumulus ssd profile                         # 方案（按工作流分组的一套模型），* 是当前用的
cumulus ssd profile save <名字> --from <工作流>
cumulus ssd profile use <名字>              # 整组换上：SSD 上只留方案里的，别的删掉腾地方
```

放不下时命令会直接报错，不会复制一半；这时告诉用户合计多大、能放多少，让用户决定删掉哪些，或者换方案。设置 `ssd_learn`（运行时自动复制，默认开）打开时，ComfyUI 开始跑工作流也会自动把缺的模型复制过去。

## 典型流程

用户说「用 Flux 出一张猫的图」：

1. `cumulus status --json`：没开机就 `cumulus up`；遇到 Drive 授权，请用户点允许，再 `drive-continue`。
2. 找到或请用户提供 Flux 的 API 工作流，改提示词和种子，存成新文件。
3. `cumulus run cat.json --json`，拿到本机图片路径，查看后给用户。
4. 用户不再出图时 `cumulus down`，或者提醒用户 VM 还开着、多久后会自动关机。
