# dtmbd — 把 DTMB 电视棒变成局域网 IPTV（树莓派 · 安卓电视 · M3U）

[![License: GPL-2.0-or-later](https://img.shields.io/badge/license-GPL--2.0--or--later-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)
[![Platform](https://img.shields.io/badge/platform-linux%20%7C%20macos%20%7C%20windows-lightgrey.svg)](#支持的硬件)
[![No transcode](https://img.shields.io/badge/transcoding-none-brightgreen.svg)](#常见问题)

> **一句话**：一根几十块的 DTMB（地面波 / DMB-TH）USB 电视棒 + 一台树莓派 + WiFi，
> 就在局域网里发出 IPTV 源。安卓电视装个播放器、订阅
> `http://dtmbd.local:8080/playlist.m3u` 就能看 30+ 个频道 —— **不用给电视拉同轴线，
> 不用装内核驱动，也不转码**。
>
> **In English:** dtmbd turns a cheap DTMB/DMB-TH USB tuner (Cypress `04b4:1004`,
> HDIC HD2312 class, AVS+/DRA) plugged into a Raspberry Pi into an IPTV server on your
> LAN. Any Android TV, Kodi, TiviMate or VLC client subscribes to the M3U playlist and
> plays the raw MPEG-TS: **no coax run to the TV, no kernel driver, no transcoding.**

![514 MHz / 522 MHz 频道截帧（AVS+ 解码后）](https://raw.githubusercontent.com/tychenjiajun/dtmbd/main/docs/screenshot-cctv-514-522.jpg)

---

## 目录

- [这是什么，能做什么](#这是什么能做什么)
- [支持的硬件](#支持的硬件)
- [5 分钟部署（树莓派）](#5-分钟部署树莓派)
- [安卓电视 / 盒子订阅](#安卓电视--盒子订阅)
- [天线找方向（场强仪）](#天线找方向场强仪)
- [工作原理与分层](#工作原理与分层)
- [实测数据](#实测数据)
- [常见问题（FAQ）](#常见问题faq)
- [故障排查](#故障排查)
- [不做什么](#不做什么)
- [协议逆向笔记](#协议逆向笔记)
- [参考项目](#参考项目)
- [许可证](#许可证)

---

## 这是什么，能做什么

`dtmbd` 是一个**单二进制**的 DTMB 接收 + IPTV 服务程序：

- 直接驱动 USB 电视棒（libusb，**不装内核模块、不下载固件**）
- 扫频、建频道表、解析 PAT/PMT/SDT（含 GB2312 台名）
- 起一个 HTTP 服务：`/playlist.m3u`（IPTV 播放列表）+ `/p/<频道>.ts`（单频道 MPEG-TS）
- 单 tuner 智能换台：点别的频点的台会自动重调（约 1.5–2 s）
- 附带**天线调试场强仪**（命令行 + 手机网页），转天线时实时看 SNR
- 用 mDNS 广播 `dtmbd.local`，电视端不用记 IP

一条命令装到树莓派、开机自启、电视订阅一次就再也不用管。

**典型场景**：老小区 / 出租屋 / 店铺，墙上只有一根天线，电视位置又没法拉线；
或者想给多台电视、手机、平板同时供地面波频道。

## 支持的硬件

| 设备 | USB ID | 备注 |
|---|---|---|
| NERC DTV / 各类「DtmbUSB」棒 | `04b4:1004` | CY68013A + HDIC HD2312，本仓库实测机型 |
| Letv 乐视 DTMB 棒 | `04b4:1004` | 固件 `V3.8.32.68`，需调谐 kick（已内置） |
| Aiwa / CVB 棒 | `04b4:1004` | CVB 需激活 key 交换（已内置） |
| 其他同芯片棒 | 视固件而定 | 报 issue 附 `lsusb` 输出 |

主机：树莓派 3/4/5、N100 迷你主机、任何 Linux；macOS 可直接跑；Windows 需先用
[Zadig](https://zadig.akeo.ie/) 把设备绑到 WinUSB。

电视棒本身 20–60 元，一根室内天线即可；树莓派 Zero 2 W 也够用（码率 ~10 Mbit/s）。

## 5 分钟部署（树莓派）

```bash
# Raspberry Pi OS 64-bit；电视棒插 USB，天线插电视棒
sudo apt install -y libusb-1.0-0-dev pkg-config curl git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y && source ~/.cargo/env

git clone https://github.com/tychenjiajun/dtmbd.git && cd dtmbd
sudo ./deploy/install.sh presets/guangzhou-foshan.json
```

安装脚本会：编译 → 装到 `/usr/local/bin/dtmbd` → 建 `dtmbd` 用户 → 装 udev 规则
（免 root 访问 USB）→ 装 systemd 服务并开机自启。

首次启动自动扫频（2–3 分钟），`journalctl -u dtmbd -f` 可见：

```
table: 33 channel(s) / 4 mux(es)
mDNS: http://dtmbd.local:8080/playlist.m3u
tuned 522.000 MHz lock=1 strength=100% snr=22.1 dB
```

没有 mDNS 的老路由就用 `http://<树莓派IP>:8080/playlist.m3u`。

也可以只装二进制（不装 systemd 服务）：

```bash
cargo install --path .                    # 需要 libusb-1.0 开发包
cargo install --path . --features vendored # 没有 libusb-dev 时，从源码编译 libusb
```

## 安卓电视 / 盒子订阅

装任意支持 M3U 的 IPTV 播放器（TiviMate / Kodi + PVR IPTV Simple / OTT Navigator /
电视直播类 App），添加播放列表：

```
http://dtmbd.local:8080/playlist.m3u
```

- 列表里是扫到的**全部频道**（跨多个频点，含广播台）
- 点台自动换频点，约 1.5–2 秒出画面
- 一个频道一份流：整频点 9–11 Mbit/s，单频道 2–4 Mbit/s

> **AVS+ 提示**：国内地面波视频是 AVS+（AVS1-P16），播放器/盒子需要支持 AVS+ 解码。
> 正规安卓电视盒子的芯片一般硬解支持；纯软解播放器（如 ExoPlayer 默认）会只有声音。
> 服务端只搬运、不转码，这是刻意的设计。

## 天线找方向（场强仪）

手机打开 `http://dtmbd.local:8080/meter`，点频点 → 点「开始」→ 慢慢转天线，
看 **SNR 峰值**（≥15 dB 稳定，<10 dB 会花屏）。

命令行（daemon 停掉后）：

```bash
dtmbd meter --freq-mhz 522          # 实时读数 + 峰值保持
dtmbd meter --scan                  # 全频段扫一遍，按 SNR 排序
dtmbd meter --scan --json           # 给脚本用
```

## 工作原理与分层

```
天线 → 电视棒(04b4:1004) ──libusb bulk──▶ Aligner ──▶ PSI demux ──▶ 广播 ──▶ HTTP MPEG-TS
                                                │                            ├─ /playlist.m3u
                                                └─ PID 过滤 ──────────────────└─ /p/<sid>.ts
```

| 层 | 文件 | 职责 |
|---|---|---|
| L1 | `src/usb/` | vendor 控制协议 + bulk TS 传输（libusb，无内核驱动） |
| L2 | `src/ts.rs` | 188 字节重同步、PAT/PMT/SDT、PID 过滤 |
| L3 | `src/tuner.rs` | 单 tuner 工作线程：懒调谐、换台、自愈、字节扇出 |
| L4 | `src/catalog.rs` | 频道表、M3U 生成、无 SDT 台名补丁 |
| L5 | `src/http.rs` | 播放列表 / 单频道 TS / 状态 / 场强仪页面 |
| L6 | `src/mdns.rs` | `dtmbd.local` 广播 |

关键设计：

- **单 tuner 语义**：一根棒同时只能收一个频点。看别的频点时自动重调，旧连接用 epoch 断开
- **懒调谐**：没人看时不占设备，第一个请求才开机调谐
- **PID 过滤**：每个观众只收自己那个频道的 PMT/PCR/ES，多音轨与字幕 PID 全保留
- **自愈**：USB 读空 6 s → 前端 power-cycle；USB 报错 → 重开设备；进程崩溃 → systemd 拉起

## 实测数据

广州，室内天线，`04b4:1004` 棒（树莓派 4B / macOS 均可）：

| 指标 | 数值 |
|---|---|
| 扫到的频点 | 474 / 506 / 514 / 522 MHz |
| 频道数 | 33（22 视频 + 11 广播） |
| 整频点码率 | 9.3 Mbit/s |
| 单频道码率 | 2–4 Mbit/s |
| TS 丢包（CC 错误） | 0.008% |
| 换台延迟 | 1.5–2 s |
| CPU 占用 | 单核 <5%（不转码） |

![474 MHz / 506 MHz 频道截帧](https://raw.githubusercontent.com/tychenjiajun/dtmbd/main/docs/screenshot-gdtv-474-506.jpg)

## 常见问题（FAQ）

**Q：DTMB 电视棒能直接插树莓派吗？**
A：能。这根棒是 vendor-class 设备，`dtmbd` 用 libusb 直接控制，**不需要内核 DVB 驱动**，
也不需要上传固件；Linux / macOS / Windows（WinUSB）通用。

**Q：安卓电视怎么收地面波？**
A：电视本身不需要天线。树莓派负责接收，电视通过 WiFi 用 IPTV 播放器订阅
`http://dtmbd.local:8080/playlist.m3u`（M3U 播放列表）。

**Q：为什么有声音没画面？**
A：视频是 AVS+（AVS1-P16），播放器不支持就会这样。换支持 AVS+ 的盒子/播放器，
或用带 AVS+ 解码的 ffmpeg 转码（服务端默认不转码）。

**Q：一根电视棒能同时看两个不同频点的台吗？**
A：不能，一根棒只有一个 tuner。同一频点上的多个台可以同时看（一份流扇出）；
要多频点并发就插多根棒。

**Q：需要转码吗？占多少 CPU？**
A：不需要。TS 原样透传，树莓派单核几 % 占用；转码才会吃满 CPU。

**Q：和 TVHeadend / xTeVe 有什么区别？**
A：TVHeadend 需要设备有内核 DVB 驱动（`/dev/dvb`），而这根棒没有现成驱动；
`dtmbd` 直接走 libusb，一条命令装好，输出标准 M3U + MPEG-TS，也能作为
TVHeadend 的 IPTV 源。

**Q：支持 Windows / macOS 吗？**
A：支持。macOS 直接跑；Windows 用 Zadig 把 `04b4:1004` 绑到 WinUSB 即可。

**Q：能录节目 / 有节目单（EPG）吗？**
A：当前版本不做录制；EIT 节目单在 CCTV 频点里有（可解析），尚未做成 `/epg.xml`。

## 故障排查

| 现象 | 排查 |
|---|---|
| `no DTMB dongle found` | `lsusb` 是否有 `04b4:1004`；udev 规则是否安装；是否加载了 dvb-usb 内核模块（会抢设备） |
| 频道表是空的 | 看 `journalctl -u dtmbd`；先手动 `dtmbd scan` 确认能锁频 |
| 点台黑屏 | 播放器不支持 AVS+；先 `ffprobe http://…/p/<sid>.ts` 确认流正常 |
| 换台慢 / 失败 | 信号弱（`snr_db` 低）或 USB 供电不足（用官方电源） |
| 找不到 `dtmbd.local` | 路由器不支持 mDNS，改用 IP；或 `--no-mdns` |
| 画面卡顿/马赛克 | 检查 `snr_db`；确认不是 WiFi 拥塞；树莓派别插在 USB3 干扰源旁 |

## 不做什么

- **不转码**：AVS+ 原样送，CPU 零负担
- **不做多 tuner 并发**：需要就插多根棒（L3 已按单 tuner 抽象）
- **不做录制 / EPG / UDP 组播**：这些交给播放器或 TVHeadend

## 协议逆向笔记

协议来自对 Linux 驱动 [`hanwckf/hd2312`](https://github.com/hanwckf/hd2312)、
[`nxdong520/DtmbUSB`](https://github.com/nxdong520/DtmbUSB) 和 Windows 端
[`IcingTomato/LeDTMB`](https://github.com/IcingTomato/LeDTMB) 的阅读与实机验证，
命令码见 [`src/usb/proto.rs`](src/usb/proto.rs)。踩过的坑（都已处理）：

1. **改频必须先 `0xAC` 关流**，否则 FX2 FIFO 卡死，后续调谐只返回 2 KB 垃圾
2. 失败后要 `0xAE/0xAD` power-cycle 才能恢复
3. 解调器失锁后会**继续上报上一个频点的锁和 PSI**，扫描不能信 lock 位，要看真实 TS 字节
4. 首包有 2 KB FIFO 残留，必须做跨 buffer 的 188 字节重同步
5. 台名多为 **GB2312 无标记**，按 ASCII 过滤会把「广东体育-HD」变成「-HD」
6. 频道栅格是 **474 + 8n MHz**（UHF），错开 4 MHz 就一个都锁不上
7. 流式循环里任何阻塞（哪怕 20 ms）都会让 USB FIFO 溢出 → 2.9% 丢包，必须非阻塞取命令

## 参考项目

- [hanwckf/hd2312](https://github.com/hanwckf/hd2312) — NERC DTMB 棒的 Linux DVB 驱动（GPL-2.0）
- [nxdong520/DtmbUSB](https://github.com/nxdong520/DtmbUSB) — Letv/Aiwa/CVB 棒的 Linux 驱动（GPL-2.0）
- [IcingTomato/LeDTMB](https://github.com/IcingTomato/LeDTMB) — 乐视棒的 Windows 实现
- [xeonliu/libcavs](https://github.com/xeonliu/libcavs) — AVS-P16（AVS+）解码器，用于本仓库的截图验证
- [xeonliu/FFmpeg-AVSPlus](https://github.com/xeonliu/FFmpeg-AVSPlus) — 支持 AVS+ 的 FFmpeg，用于验证

## 许可证

[GPL-2.0-or-later](LICENSE)。与参考的 Linux DVB 驱动（均为 GPL-2.0）保持一致。

**关键词**：DTMB、DMB-TH、地面波、地面数字电视、电视棒、数字电视、IPTV、M3U、
MPEG-TS、树莓派、Raspberry Pi、安卓电视、Android TV、机顶盒、AVS+、AVS1-P16、
EIT、EPG、PID 过滤、单频网、libusb、04b4:1004、HD2312、TVHeadend、Kodi、TiviMate。
