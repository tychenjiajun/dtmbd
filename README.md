# dtmb

把 **DTMB（GB 20600-2006）USB 电视棒** 变成 VLC / mpv / Kodi / TiviMate 能直接打开的
**IPTV 串流服务器**。无需内核驱动、无需固件下载、macOS / Linux / Windows 同一份代码。

```
USB dongle 04b4:1004 ──libusb bulk──▶ Aligner ──▶ PSI demux ──▶ broadcast hub ──┬─▶ /stream.ts   整频点
                                                                                 ├─▶ /p/<sid>.ts 单节目(PID过滤)
                                                                                 ├─▶ /playlist.m3u IPTV 播放列表
                                                                                 └─▶ UDP <addr>
```

## 实测（广州，天线 + 一根 NERC/Letv 棒）

```
$ dtmb scan --names presets/guangzhou-foshan.json
506.000 MHz  LOCK  str 100%  snr  20.5 dB  tsid 1  16QAM  4 ch
   └ 广东体育-HD / 广东卫视-HD / 广东少儿-HD / 广东影视-HD
514.000 MHz  LOCK  str 100%  snr  21.4 dB  tsid 3  16QAM  19 ch
   └ CCTV-1/2/4/10/12/13/14/15 + CNR01…CNR16（广播）
522.000 MHz  LOCK  str 100%  snr  22.1 dB  tsid 4  4QAM   4 ch
   └ CCTV-17 / CCTV-9 / CCTV-11 / CGTN
names: applied 23 name(s) from presets/guangzhou-foshan.json
3 locked mux(es), 27 channel(s) → ~/.dtmb/channels.json

$ dtmb serve --table ~/.dtmb/channels.json
table: 27 channels, 3 muxes
tuned 506.000 MHz lock=1 strength=100% snr=20.1 dB
HTTP:  http://127.0.0.1:8080/playlist.m3u
$ vlc http://127.0.0.1:8080/p/522-401   # 自动换台到 522 MHz 出流
```

- 整频点 9.1 Mbit/s，单节目 ≈4 Mbit/s，188 字节包，AVS+ (0x42) + mp2
- 单 tuner：`/p/<sid>.ts` 发现节目在别的频点 → 自动重调（约 1.5–2 s），旧连接由 epoch 断开
- 多客户端看同一节目只占一份上游带宽（`Bytes` 引用计数扇出）
- 多音轨 / 字幕：PID 过滤按 PMT 取**全部** ES，并每 800 ms 重读 PMT，所以切音轨、加字幕不需要重启

> **AVS+ 播放提示**：节目是 AVS+，ffmpeg/VLC 自带的 `cavs` 只解 AVS1，会报
> `unexpected start code`。要出画面需带 `libdavs2` 的 ffmpeg 或支持 AVS+ 的播放器
> （PotPlayer / 部分电视盒子）。音频 mp2 可直接解。服务器只搬运不转码。

## 用法

```bash
dtmb info                                    # 设备/固件/当前频点
dtmb scan --names presets/guangzhou-foshan.json   # 扫频建台表（默认存 ~/.dtmb/channels.json）
dtmb scan --start-mhz 171 --end-mhz 219      # 单独扫 VHF（从化/花都的 179/203 频点）
dtmb playlist                                # 打印 M3U
dtmb serve --table ~/.dtmb/channels.json     # 起服务（单 tuner 换台）
dtmb serve --freq-mhz 698 --udp 239.255.0.1:5000 --record cap.ts   # 固定频点 + UDP + 录制
dtmb dump --freq-mhz 522 --seconds 10 --out cap.ts
dtmb tsinfo cap.ts                           # 离线分析录制文件
```

HTTP 端点：

| 路径 | 内容 |
|---|---|
| `/playlist.m3u` | IPTV 播放列表（跨所有频点，按 provider 分组） |
| `/p/<sid>.ts`、`/p/<name>.ts` | 单节目；必要时自动换台 |
| `/stream.ts` | 当前频点整复用流 |
| `/` | 状态页 + 频道表 |
| `/status.json`、`/api/channels` | 机器可读状态 / 台表 |
| `/healthz` | 探针 |

## 协议（逆向自 hd2312 / DtmbUSB / LeDTMB）

设备 `04b4:1004`，vendor-class，两个 bulk-IN（`0x82` = TS，`0x86` 未用）。
**枚举即 warm 态，不需要上传 FX2 固件**，这是跨平台的前提。vendor 控制传输，`wValue=0xFE`：

| bRequest | 方向 | 长度 | 作用 |
|---|---|---|---|
| `0xED` | IN | 4 | 固件版本；`0x03`→Letv、`0x05`→Aiwa、`0x06`→CVB，其余按 NERC 处理 |
| `0xFC` | OUT | 4 | 频率（Hz，大端） |
| `0xEA` | OUT | 1 | 调谐 kick（Letv 固件必须；实测比 4 字节快 ~200 ms） |
| `0xEA` | IN | 1 | 锁定标志 |
| `0xAB` / `0xAC` | OUT | 0 | 开流 / 关流（**改频前必须关流，否则 FX2 会卡死**） |
| `0xAD` / `0xAE` | OUT | 0 | 上下电（配合 150/350 ms 延时做 power-cycle 恢复） |
| `0xEB` `0xE8` `0xE7` `0xEC` `0xE9` | IN | 4/2/6/1/3 | 强度 / SNR / 参数 / has_signal / 质量 |
| `0xBE` / `0xBF` | OUT/IN | 9/22 | CVB 变体激活 key / hash |

踩过的坑（都已在代码里处理）：

1. **改频必须先 `0xAC` 关流**，否则 FX2 FIFO 卡死，后续调谐只返回 2 KB 垃圾。
2. 失败后要 `0xAE/0xAD` power-cycle 才能恢复（`Dtmb::power_cycle`）。
3. 解调器在失锁后会**继续上报上一个频点的锁/PSI**，所以扫描不能信 lock 位，要看真实 TS 字节 + PAT 是否变化（`tsid` 去重）。
4. 首包有 2 KB `00ff00ff…` FIFO 残留，必须做跨 buffer 的 188 字节重同步。
5. 中文台名多为 **GB2312 无标记**，按 ASCII 过滤会把“广州综合HD”变成“-HD”——已用 `encoding_rs` 正确解码。
6. 频道栅格是 **474 + 8n**（UHF），从 470 起扫会全部错开 4 MHz，一个都锁不上。

## 编译

```bash
brew install libusb && cargo build --release        # macOS
sudo apt install libusb-1.0-0-dev pkg-config && cargo build --release   # Debian/Ubuntu
# Windows：先用 Zadig 把 04b4:1004 绑到 WinUSB，再 cargo build --release
```

Linux 上若装了 `dvb_usb_hd2312` / `DtmbUSB` 内核模块，本程序会自动 detach，两者不要同时用。

## 代码结构

| 文件 | 职责 |
|---|---|
| `src/proto.rs` | 命令码、栅格、变体识别 |
| `src/usb.rs` | rusb 封装：claim、EP0 STALL 恢复、关流改频、power-cycle、bulk 读 |
| `src/ts.rs` | 188 字节重同步、PAT/PMT/SDT（CRC32 + GB2312）、PID 过滤、PSI 重打包 |
| `src/registry.rs` | 频道表（JSON）、M3U 生成、外部台名补丁 |
| `src/hub.rs` | 采集线程、`tokio::broadcast` 扇出、换台命令与 epoch |
| `src/serve.rs` | axum HTTP-TS / 播放列表 / 状态页，UDP 推送 |
| `presets/` | 台名补丁（asiatvro 广州/佛山整理） |

## 已知限制 / 待办

- 单 tuner：同一时刻只能一个频点在线（一个人看足够；多频并发需要多根棒，`Hub` 已按频点抽象，扩展时用 `HashMap<freq, Hub>`）
- AVS+ 不转码；要兼容浏览器可加 HLS + `libdavs2` 分支
- VHF 频点（asiatvro 上的 179/203 MHz 从化/花都）本机收不到，未验证
- EIT → XMLTV 节目单未做
- RTSP 输出未做（老机顶盒）
- 断流自动恢复：目前靠下一轮 read 报错重试，热插拔需重启
