# dtmb

把 **DTMB（GB 20600-2006）USB 电视棒** 变成 VLC / mpv / ffplay 能直接打开的 **MPEG-TS 串流服务器**。
无需内核驱动、无需固件下载、无需在 Windows 上折腾 DirectShow —— 一个 `libusb` 通道把 USB 上的 TS 直接
搬到 HTTP / UDP 上。

```
USB dongle (04b4:1004)  ──libusb bulk──▶  Aligner  ──▶  PSI demux  ──▶  broadcast hub  ──┬─▶ HTTP  /stream.ts
                                                                                          ├─▶ HTTP  /p/<sid>.ts
                                                                                          └─▶ UDP   <addr>
```

## 实测状态（macOS 26 / MBP，天线接在 698 MHz 央视单频网）

```
$ dtmb info
bus 020 addr 005  04b4:1004  vendor="NERC" product="DtmbUSB"
firmware: V3.8.3268   variant: Letv
tuned 698.000 MHz  lock=1 signal=1 strength=100% snr=16.9 dB
                   [carrier 0 | GI PN595 | FEC 0.8 | IL 720 | 4QAM ]

$ dtmb serve --freq-mhz 698 --http 0.0.0.0:8080
HTTP:  http://0.0.0.0:8080/stream.ts   (MRL for VLC / mpv / ffplay)

$ curl -s localhost:8080/status.json
{"bitrate_mbps":9.09,"locked":true,"programs":[
  {"service_id":401,"name":"CCTV-17","pmt_pid":257,"pcr_pid":512,
   "streams":[{"kind":"avs","pid":512},{"kind":"audio","pid":650}]},
  {"service_id":402,"name":"CCTV-9", ...}, {"service_id":403,"name":"CCTV-11", ...},
  {"service_id":404,"name":"CGTN", ...}]}
```

- 整复用流：9.1 Mbit/s，188 字节包，AVS+ 视频（stream_type 0x42）+ mp2 音频
- 单节目过滤：`/p/401.ts` ≈ 4 Mbit/s（只留该节目的 PMT/PCR/ES）
- 多客户端并发广播已验证（同一 `Bytes` 引用计数扇出，零拷贝）
- `ffprobe http://127.0.0.1:8080/stream.ts` 能正确列出 4 个节目及其 codec

> **AVS+ 播放提示**：节目是 AVS+（不是 AVS1）。ffmpeg / VLC 自带的 `cavs` 解码器只支持 AVS1，
> 会报 `unexpected start code`。要真正出画面，需要带 `libdavs2` 的播放器
> （自己编译 ffmpeg `--enable-libdavs2`，或用支持 AVS+ 的播放器）。音频 mp2 可以直接解。
> 服务器本身只做搬运，不做转码。

## 硬件 / 协议

设备：`04b4:1004`，厂商字符串 `NERC`，产品 `DtmbUSB`，一个 vendor-class 接口、两个 bulk-IN 端点
（`0x82` = MPEG-TS，`0x86` 未使用）。**枚举即为 warm 状态**，不需要上传 FX2 固件 —— 这也是
跨平台方案成立的前提。

所有命令都是 vendor 控制传输，`wValue = 0x00FE`、`wIndex = 0`：

| bRequest | 方向 | 长度 | 作用 |
|---|---|---|---|
| `0xED` | IN | 4 | 固件版本 `major.minor.xx.xx`，用于识别 Letv/Aiwa/CVB 变体 |
| `0xAD` / `0xAE` | OUT | 0 | 上电 / 断电 |
| `0xAB` / `0xAC` | OUT | 0 | 开流 / 关流 |
| `0xFC` | OUT | 4 | 设置频率（Hz，大端，474–866 MHz，8 MHz 栅格） |
| `0xEA` | OUT | 4 | **调谐 kick**（Letv 固件必须，NERC 可忽略） |
| `0xEA` | IN | 1 | 解调锁定标志（1 = locked） |
| `0xEC` | IN | 1 | has_signal |
| `0xEB` | IN | 4 | 信号强度，`b[3]` = 百分比 |
| `0xE8` | IN | 2 | SNR = `b[0] + b[1]/100` dB |
| `0xE7` | IN | 6 | 载波 / 保护间隔 / FEC / 交织 / 星座 / 相位 |
| `0xE9` | IN | 3 | 质量（变体相关） |
| `0xBE` / `0xBF` | OUT/IN | 9 / 22 | CVB 变体的激活 key / hash |

来源：Linux 驱动 [`hanwckf/hd2312`](https://github.com/hanwckf/hd2312)（NERC，CY68013A + HDIC HD2312）、
[`nxdong520/DtmbUSB`](https://github.com/nxdong520/DtmbUSB)（Letv/Aiwa/CVB）、
Windows 端 [`IcingTomato/LeDTMB`](https://github.com/IcingTomato/LeDTMB)（CyAPI 实现，UDP → :4567）。

## 编译

```bash
# macOS
brew install libusb
cargo build --release

# Debian/Ubuntu
sudo apt install libusb-1.0-0-dev pkg-config
cargo build --release

# Windows：先给设备绑定 WinUSB（Zadig → 04b4:1004 → WinUSB），
# 否则 libusb 打不开（设备本身没有 Windows 驱动）
cargo build --release
```

Linux 上如果装了 `dvb_usb_hd2312` / `DtmbUSB` 内核模块，本程序会自动 detach；两者不要同时使用。

## 用法

```bash
dtmb info                                   # 设备信息 + 当前频点状态
dtmb scan --start-mhz 474 --end-mhz 866     # 扫频，列出所有锁定频点
dtmb dump --freq-mhz 698 --seconds 10 --out cap.ts --stats
dtmb tsinfo cap.ts                          # 离线分析录制文件的节目表
dtmb serve --freq-mhz 698 --http 0.0.0.0:8080
dtmb serve  --http 0.0.0.0:8080             # 不指定频点 → 自动扫到第一个锁定频点
dtmb serve --freq-mhz 698 --udp 239.255.0.1:5000 --record cap.ts
```

播放：

```bash
vlc http://<host>:8080/stream.ts        # 整个复用流（4 个节目，9 Mbit/s）
vlc http://<host>:8080/p/401.ts         # 单节目过滤
vlc udp://@239.255.0.1:5000            # UDP 推送
```

HTTP 端点：

| 路径 | 内容 |
|---|---|
| `/` | 状态页（信号、码率、节目表） |
| `/status.json` | 同上，JSON |
| `/stream.ts` | 完整复用流，chunked MPEG-TS |
| `/p/<sid>.ts` | 单节目（PMT/PCR/ES 过滤），新客户端先注入缓存的 PAT/PMT |
| `/healthz` | 存活探针 |

## 代码结构

| 文件 | 职责 |
|---|---|
| `src/proto.rs` | 命令码、频率栅格、变体识别、状态结构 |
| `src/usb.rs` | rusb 封装：claim、控制传输（含 EP0 STALL 自动 clear + 重试）、bulk 读 |
| `src/ts.rs` | 188 字节重同步、PAT/PMT/SDT 解析（含 CRC32-MPEG）、PID 过滤、PSI 重打包 |
| `src/hub.rs` | 采集线程 + `tokio::sync::broadcast` 扇出 + 码率统计 |
| `src/serve.rs` | axum HTTP-TS / 状态页，UDP 推送 |
| `src/main.rs` | clap CLI |

## 待办

- RTSP（RTP over UDP/TCP interleaved）输出，给硬件播放器/机顶盒用
- HLS 切片（按 PCR/关键帧切 2 s）
- 多频点轮询 / 后台换台 API
- 断流自动恢复（USB 重插热插拔：`nusb` 的 hotplug 比 rusb 更好做）
- 录制按节目切片（现在 `--record` 只存整个复用流）
- Windows 端 CI（交叉编译 + WinUSB 驱动安装说明）
