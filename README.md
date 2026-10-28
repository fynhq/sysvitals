# SysVitals

[English](#english) | [中文](#中文)

<p align="center">
  <img src="logo/sysvitals_logo_1024.png" alt="SysVitals logo" width="160" />
</p>

## English

A lightweight CPU / memory / network monitor for the Windows taskbar and macOS menu bar. It shows live percentages and transfer rates, follows system font, DPI and appearance, and opens a CPU / memory history chart when clicked. Built in Rust, it aims to look native and stay cheap to run 24/7.

### Supported systems

- Windows 10 2004+ / 11 (x64 or arm64)
- macOS 13+ (Apple Silicon or Intel)

### Footprint

Measured with the release build running normally (all groups on, default 2 s sampling, no popup):

- Windows 11 taskbar: **~9 MB** resident
- macOS 27 (Apple Silicon) menu bar: **~18 MB** resident

### Install

**Windows**

1. Download the zip for your architecture from the [Releases page](https://github.com/fynhq/sysvitals/releases) and extract it.
2. The archive contains one `SysVitals/` folder with `SysVitals.exe`, `SysVitalsSettings.exe`, `SysVitalsHistory.exe`, and `LICENSE`. Keep the three executables together.
3. Run `SysVitals.exe`; Settings and History are launched by the indicator.
4. The build is unsigned. If SmartScreen appears, choose **More info → Run anyway**.

**macOS**

1. Download the zip for your Mac from the [Releases page](https://github.com/fynhq/sysvitals/releases) and extract `SysVitals.app`.
2. Move it to `/Applications` and launch the `.app` (not the binaries inside `Contents/MacOS`).
3. The build is unsigned and not notarized. If macOS blocks it, try right-click → **Open**; otherwise open **System Settings → Privacy & Security → Open Anyway**. If it is still blocked, remove the quarantine flag:

   ```sh
   xattr -d -r com.apple.quarantine /Applications/SysVitals.app
   ```

   If you get a permission error, run the same command with `sudo`.

4. SysVitals appears in the menu bar; the indicator has no Dock icon (Settings may show one while open).

### Upgrade

SysVitals has no built-in auto-update; install a newer release manually.

**Windows**

1. Quit SysVitals.
2. Download the new zip and replace the existing `SysVitals/` folder; keep the three executables together.
3. Run `SysVitals.exe` again. Settings, history and logs in `%APPDATA%\SysVitals` are kept.

**macOS**

1. Quit SysVitals.
2. Download the new zip and replace `/Applications/SysVitals.app`; leave `~/Library/Application Support/SysVitals` untouched.
3. A newly downloaded app is quarantined again, so the command **must be re-run**:

   ```sh
   xattr -d -r com.apple.quarantine /Applications/SysVitals.app
   ```

   If you get a permission error, run the same command with `sudo`.

4. Launch the new `SysVitals.app`.

### Usage

- Click the indicator to open the CPU / memory history chart; click again, click away, or press Esc to close.
- On Windows, every visible taskbar gets its own indicator (enable "Show my taskbar on all displays" to show it on secondary screens); clicking an indicator on another screen moves the history chart there.
- Right-click for the menu:
  - Windows: Task Manager, Settings, Restart, Quit.
  - macOS: Activity Monitor, Settings, Restart, Quit.
- Settings save immediately; there is no Apply button. Quit closes the indicator and any open panels.
- Configure language, sampling interval (1/2/3 s; default 2 s), layout, spacing, prefixes, indicator theme (Windows), and Windows taskbar anchor/margin. Vertical taskbars are supported.
- Settings and history follow system appearance and accent color; the Windows indicator theme can be auto/light/dark.
- History ranges: 5 minutes, 1 hour, 1 day, 1 month; up to one month is retained.
- The copy button exports the visible range as tab-separated rows with a UTF-8 BOM.
- Network rates are live-only; history and CSV cover CPU and memory.
- All data stays local; no telemetry, cloud service, or account.

### Verify the download

Each release provides a single `SHA256SUMS` file covering all archives. Download it and the archive(s) you want to check into the same folder.

**Windows (PowerShell)**

```powershell
Get-Content .\SHA256SUMS | ForEach-Object {
  $hash, $file = $_ -split '\s+', 2
  $file = $file.Trim()
  if (Test-Path ".\$file") {
    $actual = (Get-FileHash ".\$file" -Algorithm SHA256).Hash.ToLower()
    "{0}: {1}" -f $file, $(if ($actual -eq $hash) { "OK" } else { "MISMATCH" })
  }
}
```

**macOS**

```sh
shasum -a 256 -c --ignore-missing SHA256SUMS
```

Archives you did not download are skipped. Checksums verify download integrity, not a signature.

### Data and uninstall

**Windows**

- Data: `%APPDATA%\SysVitals`.
- Uninstall: quit SysVitals, delete the extracted `SysVitals/` folder, and remove the data folder to delete settings, history and logs.

**macOS**

- Data: `~/Library/Application Support/SysVitals`.
- Uninstall: quit SysVitals, move `SysVitals.app` to the Trash, and remove the data folder.

History is kept for up to one month; logs older than 7 days are removed.

### Known limitations

- Launch at login is not implemented.

### Build from source

Requires a stable Rust toolchain. Windows uses MSVC; macOS uses the Xcode command-line tools.

```sh
cargo build
cargo test
cargo run -p sysvitals-core --example monitor
```

`cargo build` and `cargo test` cover the cross-platform core crate. The official Windows/macOS packages are built by CI; contributors can build a platform crate on the matching OS if needed:

```sh
cargo build -p app-win
cargo build -p app-mac
cargo build -p app-settings
cargo build -p app-history
```

### Design notes

Counter differencing, wrap-around protection, NIC selection, units and layout live in the pure-Rust `core` crate and are unit-tested. The status bar is drawn by thin platform-native code: a topmost layered window on Windows and an `NSStatusItem` on macOS. Settings and history run as separate GPUI processes, keeping the resident indicator minimal.

### License

Released under the MIT License. See [LICENSE](LICENSE).

---

## 中文

常驻 Windows 任务栏 / macOS 菜单栏的轻量 CPU、内存、网速监控。实时显示占用百分比与上下行速度，字号、DPI 与亮暗跟随系统，点击可查看 CPU / 内存历史走势图；Rust 构建，目标是与系统原生一致且长期运行开销低。

### 支持系统

- Windows 10 2004+ / 11（x64 / arm64）
- macOS 13+（Apple Silicon / Intel）

### 资源占用

真机实测，release 构建正常运行（指标全开、默认 2 秒采样、无弹窗）：

- Windows 11 任务栏：常驻 **约 9 MB**
- macOS 27（Apple Silicon）菜单栏：常驻 **约 18 MB**

### 安装

**Windows**

1. 从 [Releases 页面](https://github.com/fynhq/sysvitals/releases) 下载对应架构的 zip 并解压。
2. 压缩包内是单一 `SysVitals/` 目录，含 `SysVitals.exe`、`SysVitalsSettings.exe`、`SysVitalsHistory.exe` 和 `LICENSE`；三个 exe 请保持在一起。
3. 运行 `SysVitals.exe`；设置与走势图由指示器自动拉起。
4. 构建未签名。首次启动如出现 SmartScreen，选择“更多信息”→“仍要运行”。

**macOS**

1. 从 [Releases 页面](https://github.com/fynhq/sysvitals/releases) 下载对应 Mac 的 zip，解压 `SysVitals.app`。
2. 移到 `/Applications`，启动 `.app`（不要直接运行 `Contents/MacOS` 里的二进制）。
3. 构建未签名/未公证。如被拦截，先右键→“打开”；否则到“系统设置→隐私与安全性→仍要打开”。如仍被拦截，移除隔离标记：

   ```sh
   xattr -d -r com.apple.quarantine /Applications/SysVitals.app
   ```

   如遇权限问题，可尝试在命令前加 `sudo` 执行。

4. SysVitals 出现在菜单栏；指示器无 Dock 图标（设置窗打开时可能显示）。

### 升级

SysVitals 没有内置自动更新；需要手动安装新版。

**Windows**

1. 退出 SysVitals。
2. 下载新 zip，替换旧的 `SysVitals/` 目录；三个 exe 保持在一起。
3. 重新运行 `SysVitals.exe`。`%APPDATA%\SysVitals` 中的设置、历史与日志保留。

**macOS**

1. 退出 SysVitals。
2. 下载新 zip，替换 `/Applications/SysVitals.app`；保留 `~/Library/Application Support/SysVitals`。
3. 新下载的 app 会重新带上隔离标记，**必须重新执行**：

   ```sh
   xattr -d -r com.apple.quarantine /Applications/SysVitals.app
   ```

   如遇权限问题，可尝试在命令前加 `sudo` 执行。

4. 启动新版 `SysVitals.app`。

### 使用

- 单击指示器打开 CPU / 内存走势图；再次单击、点击别处或按 Esc 关闭。
- Windows 上每个可见任务栏都有独立指示器（在系统中开启“在所有显示器上显示任务栏”后副屏才会出现）；点击其他屏幕的指示器会把走势图移到该屏。
- 右键菜单：
  - Windows：任务管理器、设置、重启、退出。
  - macOS：活动监视器、设置、重启、退出。
- 设置即时保存，没有应用按钮。退出会关闭指示器及所有打开的窗口。
- 可配置语言、采样间隔（1/2/3 秒，默认 2 秒）、布局、间距、前缀、指示器主题（Windows）、Windows 任务栏锚位/边距。支持垂直任务栏。
- 设置与走势图跟随系统亮暗与强调色；Windows 指示器主题可选自动/亮色/暗色。
- 走势图范围：5 分钟、1 小时、1 天、1 个月；历史最多保留一个月。
- 复制按钮导出当前可视范围，Tab 分隔 + UTF-8 BOM。
- 网速只实时显示；历史与 CSV 覆盖 CPU / 内存。
- 所有数据都在本地；无遥测、云服务或账号。

### 校验下载

每次发布提供一个 `SHA256SUMS`，覆盖全部压缩包。把它与要校验的压缩包下载到同一目录。

**Windows（PowerShell）**

```powershell
Get-Content .\SHA256SUMS | ForEach-Object {
  $hash, $file = $_ -split '\s+', 2
  $file = $file.Trim()
  if (Test-Path ".\$file") {
    $actual = (Get-FileHash ".\$file" -Algorithm SHA256).Hash.ToLower()
    "{0}: {1}" -f $file, $(if ($actual -eq $hash) { "OK" } else { "MISMATCH" })
  }
}
```

**macOS**

```sh
shasum -a 256 -c --ignore-missing SHA256SUMS
```

未下载的压缩包会被跳过。校验只保证下载完整，不代表签名。

### 数据与卸载

**Windows**

- 数据：`%APPDATA%\SysVitals`。
- 卸载：退出 SysVitals，删除解压出的 `SysVitals/` 目录；如需清除设置、历史与日志，再删除数据目录。

**macOS**

- 数据：`~/Library/Application Support/SysVitals`。
- 卸载：退出 SysVitals，把 `SysVitals.app` 移到废纸篓；如需清除数据，再删除数据目录。

历史最多保留一个月；日志超过 7 天自动清理。

### 已知限制

- 尚未实现开机自启。

### 从源码构建

要求 stable Rust 工具链。Windows 用 MSVC，macOS 用 Xcode 命令行工具。

```sh
cargo build
cargo test
cargo run -p sysvitals-core --example monitor
```

`cargo build` / `cargo test` 覆盖跨平台 core crate；官方 Windows/macOS 包由 CI 构建；贡献者如需本地调试，可在对应系统显式编译：

```sh
cargo build -p app-win
cargo build -p app-mac
cargo build -p app-settings
cargo build -p app-history
```

### 设计说明

计数器差分、回绕保护、网卡选择、单位与排版都放在纯 Rust 的 `core` crate 中并做单元测试。状态栏由薄的平台原生代码绘制：Windows 为 topmost 分层窗口，macOS 为 `NSStatusItem`。设置与走势图是独立 GPUI 进程，常驻指示器保持精简。

### 许可

基于 MIT 许可发布，见 [LICENSE](LICENSE)。
