# dtmb

一根 DTMB 电视棒 + 一台小电脑（树莓派 / N100 小主机）+ WiFi = **你家的 IPTV 源**。
电视不用拉同轴线，也不用在电视上插 USB：安卓电视装个 IPTV 播放器，订阅一个 M3U 地址就行。

```
  同轴天线
     │
  ┌──▼──────────────┐        WiFi / LAN        ┌──────────────────┐
  │ 树莓派 + 电视棒  │ ─── HTTP MPEG-TS ───────▶ │ 安卓电视 / 盒子   │
  │ dtmb serve :8080│   http://dtmb.local:8080  │ IPTV 播放器      │
  └─────────────────┘   /playlist.m3u           └──────────────────┘
```

## 在树莓派上装（5 分钟）

```bash
# Raspberry Pi OS 64-bit，USB 口插上电视棒，天线插电视棒
sudo apt install -y libusb-1.0-0-dev pkg-config curl git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env

git clone <this repo> && cd dtmb
sudo ./deploy/install.sh presets/guangzhou-foshan.json
```

安装脚本做的事：编译 → 装到 `/usr/local/bin` → 建 `dtmb` 用户 → 装 udev 规则（免 root 访问 USB）
→ 装 systemd 服务并开机自启。

首次启动会自动扫频（约 2–3 分钟），之后 `journalctl -u dtmb -f` 可以看到：

```
table: 33 channel(s) / 4 mux(es)
mDNS: http://dtmb.local:8080/playlist.m3u
tuned 522.000 MHz lock=1 strength=100% snr=22.1 dB
```

不想用 mDNS（老路由不支持）就加 `--no-mdns`，用 `http://<树莓派IP>:8080/playlist.m3u`。

## 在安卓电视上订阅

装任意支持 M3U 的 IPTV 播放器（TiviMate / Kodi + PVR IPTV Simple / OTT Navigator / 电视直播类 App），
**添加 M3U 播放列表**：

```
http://dtmb.local:8080/playlist.m3u
```

播放列表里就是扫到的所有台（跨多个频点）。点台时服务器会自动把电视棒调到对应频点，
换台约 **1.5–2 秒**（调谐 + 等 PAT/PMT），之后就是普通 HTTP 流，暂停/切台都在播放器里操作。

> **AVS+ 解码**：国内 DTMB 视频是 AVS+，播放器必须带 AVS+ 解码器（电视盒子芯片一般硬解支持；
> 纯软件播放器如 ExoPlayer 默认不支持，会黑屏只有声音）。音频是 mp2，都能放。
> 这不是服务端问题——服务端只搬运、不转码。

## 工作原理

```
L6 mdns     dtmb.local 广播
L5 http     /playlist.m3u  /p/<sid>.ts  /status.json
L4 catalog  频道表（scan 产物）+ 台名补丁 + M3U 生成
L3 tuner    单 tuner 工作线程：懒调谐、换台、故障恢复、字节扇出
L2 ts       188 字节重同步、PAT/PMT/SDT、PID 过滤
L1 usb      vendor 控制协议 + bulk TS（libusb，无内核驱动）
```

设计要点：

- **单 tuner 语义**：一根棒同时只能收一个频点。`/p/<id>.ts` 发现节目在别的频点就自动重调；
  旧连接通过 `epoch` 断开，播放器重连即可，不会解出错误的节目。
- **懒调谐**：没人看时不占用 tuner（树莓派更凉快），第一个请求才开机调谐。
- **PID 过滤**：只给播放器发这个节目的 PMT/PCR/ES，整频点 9 Mbit/s → 单节目约 4 Mbit/s，
  多音轨/字幕的 PID 全保留（切音轨、字幕不用重启）。
- **自愈**：USB 读空 6 秒 → power-cycle 前端；USB 报错 → 重开设备；daemon 崩了 systemd 拉起。

## 天线调试（场强仪）

装天线最难的是"转到哪个角度"——这两个工具就是为它做的：

**手机上（推荐，人站在天线上）**

```
http://dtmb.local:8080/meter
```

点频点 → 点“开始”，然后慢慢转天线。页面实时显示强度条、SNR 条和**峰值保持**，
转一圈下来看峰值最大处就是最佳角度。点“恢复正常播放”退出调试。

**命令行（树莓派本机 / 笔记本；daemon 运行时会占用设备，先 `systemctl stop dtmb`）**

```bash
dtmb meter --freq-mhz 522          # 实时读数 + 峰值保持，Ctrl-C 退出
dtmb meter --scan                  # 全频段扫一遍，按 SNR 排序，标出最佳频点
dtmb meter --scan --start-mhz 470 --end-mhz 560 --settle-ms 500
dtmb meter --scan --json           # 给脚本用
```

输出示例：

```
★ 522.000 MHz  100%  SNR  21.9 dB  ███████████████░░░░░  4QAM PN595 0.8
  514.000 MHz  100%  SNR  21.5 dB  ██████████████░░░░░░  16QAM PN945 0.8
  506.000 MHz  100%  SNR  19.6 dB  █████████████░░░░░░░  16QAM PN420 0.8
```

怎么判断：

| 指标 | 含义 | 参考值 |
|---|---|---|
| `snr_db` | **最重要的指标**，决定画质 | ≥15 dB 稳定；10–15 临界；<10 花屏 |
| `strength_pct` | AGC 百分比，容易饱和到 100% | 参考意义有限，别只盯它 |
| `LOCK` | 解调器是否锁上 | 锁不上就换方向/换位置 |
| `modulation` | 该频点用 4QAM/16QAM/64QAM | 与天线无关，但 64QAM 对 SNR 要求更高 |

技巧：先看 SNR 条，把天线转到 SNR 最高；再看 `strength_pct` 微调 1–2 度；
测完记得点“恢复正常播放”，否则电视那边会一直暂停。

## 命令行

```bash
dtmb scan                      # 重新扫频建表（搬家/换天线后跑）
dtmb scan --names presets/guangzhou-foshan.json
dtmb scan --start-mhz 171 --end-mhz 219     # 单独扫 VHF（部分城市有）
dtmb serve                     # 前台跑（调试用，systemd 用同样参数）
dtmb meter --freq-mhz 522      # 天线调试场强仪
```

`/etc/dtmb/names.json` 可选：有些频点的 PMT 里没有 SDT 台名，用这个补丁按 `freq+sid` 命名
（`presets/guangzhou-foshan.json` 是按 asiatvro 广州/佛山台表整理的）。

## 硬件

- 电视棒：NERC / Letv / Aiwa / CVB 的 `04b4:1004`（CY68013A + HDIC HD2312 一类），几十块钱
- 主机：树莓派 3/4/5、N100 小主机、任何 Linux（macOS 也能跑，Windows 需先用 Zadig 绑 WinUSB）
- 天线：室内天线即可，`/status.json` 里 `strength_pct` / `snr_db` 判断信号质量；
  `snr_db` 低于 ~10 dB 会花屏，换位置或换高增益天线
- 树莓派供电要足（官方电源），USB 供电不足会掉设备

## 排错

| 现象 | 查什么 |
|---|---|
| `no DTMB dongle found` | `lsusb` 有没有 `04b4:1004`；udev 规则是否装了；是否装了 dvb-usb 内核模块（会抢设备） |
| 频道列表是空的 | 表没扫到：`journalctl -u dtmb` 看 scan 输出；先手动 `dtmb scan` 确认能锁定 |
| 点台黑屏 | 播放器不支持 AVS+；先用 `ffprobe http://…/p/<sid>.ts` 确认流是好的 |
| 换台很慢 / 经常失败 | 信号弱（`snr_db` 低），或 USB 供电不足 |
| 电视找不到 dtmb.local | 路由器不支持 mDNS：改用 IP；或 `--no-mdns` 后手动填 IP |

## 开发

```bash
cargo test                 # 重同步 / 跨包拼接 / PSI 打包 / GB2312 台名
cargo run -- serve --table /tmp/channels.json --no-mdns --bind 127.0.0.1:8080
```

协议细节（vendor 请求表、踩过的坑：改频必须先关流、失锁后上报旧 PSI、首包 2 KB FIFO 垃圾、
GB2312 台名、474+8n 栅格）见 `src/usb/proto.rs` 与 git 历史。

## 不做的事

- 不转码：AVS+ 原样送，CPU 零负担
- 不做多 tuner 并发：一个人/一家人看一个频点足够；真要并发就插多根棒，按频点复制 L3
- 不做录制、EPG、UDP 组播：这些用播放器或 TVHeadend 解决
