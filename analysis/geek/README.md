# Geek 与 BCUninstaller：项目适用性分析

分析日期：2026-10-26。目标是评估轻量 Windows 卸载、清理及软件数据迁移工具的开发路线。

## 结论

Geek 更适合作为轻量产品形态和交互参考；BCUninstaller 更适合作为可审查、按许可证评估复用的卸载实现来源。当前证据不足以支持用 Geek 替代 BCU 作为开发底座，也不能证明 Geek 的系统文件保护、残留识别或运行性能更好。迁移模块仍应独立评估 Viap。

建议保持 Rust 核心与 Windows 原生界面：向 Geek 学习聚焦、少入口、搜索与按需操作；向 BCU 学习卸载兼容性处理；迁移复用适用的 Viap 模块；清理流程独立实现。没有必要把上述工具整套打包。

## 本地样本身份

| 项目 | 本地测量 |
|---|---|
| 文件 | 用户提供的 geek.exe |
| 版本 | 1.5.2.165 |
| 文件大小 | 7,020,016 字节，约 6.70 MiB |
| SHA-256 | d96df1051e62aa40baefd51235be45f8038745582a5d3428b63123fd2ced60db |
| 外层格式 | PE32 / x86 |
| 内嵌程序 | BIN / 505 / 1033 资源中的 PE32+ / x64，版本同为 1.5.2.165 |
| 内嵌程序大小 | 3,865,072 字节 |
| 内嵌程序 SHA-256 | b8d968872fe7ed8de7eeb89ff6e1ce2029521f7c744c088ae2c4807b396d28ae |

本地版本与官网现行版本不同，官网新功能和修复不能直接归给本地旧样本。

## REA 静态分析证据

1. **Windows 原生依赖特征。** 外层导入表完整读取到 18 个 DLL、708 个导入项；内嵌 x64 程序为 18 个 DLL、725 个导入项。表中均为 Windows 系统组件，包含 USER32、COMCTL32、GDI32、ADVAPI32、SHELL32 等。没有看到 CLR、WebView2 或外部 MFC/CRT DLL 的直接导入；这不排除动态加载。外层证据：`ev_dfd231e3ca9104e8d0ba686ccf32682e64d1c5aebefd2f6962d25ffb3e887a85`；内嵌程序：`ev_133ec711658a7ec7450f7fa58e004079c412ae9e6bea2dbf364e62adfec0713b`。

2. **单文件分发内部包含两个架构。** REA 资源解析识别到外层 215 个资源，其中 BIN/505 是完整 x64 PE。根据资源记录的精确字节区间提取后，其 SHA-256 与资源摘要一致。函数 `0x408ba8` 的反编译结果包含 FindResourceW(BIN,505)、LoadResource、WriteFile，并调用 `0x479ed0`；后者包含 CreateProcessW 和进程句柄释放。因此“只分发一个 EXE”不等同于“执行期间完全不释放其他程序”。资源证据：`ev_a5ad9dd70ff11db2dbf01c3737b2864b08c7577abb42f9f8c9e756702eefcddd`；提取流程：`ev_adb5efb3e3065d5b48ff65003d982fd8bedbb82b3a6af34169a82c17d4f9c523`；启动帮助函数：`ev_6bb685ff4966384fdd9bfb7737c02b23b22c0f7a6ac57655367750e980e8856e`。未执行该流程，实际启动分支与落盘位置未动态验证。

3. **原生 C++ / MFC / ATL 特征。** Ghidra 字符串与类型信息包含 CWinApp、AfxWnd140su、ATL 字符串类型和 MFC 相关构建特征，与作者的 C++ 说明相符。没有外部 MFC DLL 的直接导入，支持静态链接原生组件的推断，不能据此测算内存。证据：`ev_67797434085594b2493e473b9416b9c82cfe2457c27145d5179385c45970438c`。

4. **卸载登记与 MSI 处理。** `0x4452d9` 的反编译结果读取注册表 InstallProperties、DisplayIcon、DisplayVersion、WindowsInstaller；另有 MSI、安装位置查找、普通与 MSI 扫描器以及 Windows Store PackageManager 的字符串线索。注册表读取是代码证据，其余线索不单独证明功能覆盖或可靠性。证据：`ev_b8d51e762d6aaeb53df78b601564d585e3b68322c88b27514bf160fab653013f`、`ev_fb7b2135b1ffbd8b19131d9a353502cc1def672bb5e826f74ae036e07b0b5ce6`。

5. **强制移除存在确认分支。** `0x43239c` 引用 msgForceRemovalQuestion，并根据确认结果进入后续流程。本次没有完整证明后续每项扫描、删除或恢复策略。证据：`ev_e01b1368dba0a759f910760d6af155241adcd9c5e186a6079567d899e3dc5e51`。

6. **系统文件识别能力仍未证明。** 对 Ghidra 已定义字符串库存的定向搜索未返回 SystemComponent、NoRemove、SfcIsFileProtected、WinVerifyTrust 等选定名称；部分注册表键在反编译中不是清晰字符串。此负面结果仅适用于该字符串库存，不能证明相关机制不存在，更不能据此宣布 Geek 无保护。本次没有建立它比 BCU 更可靠的证据。证据：`ev_67797434085594b2493e473b9416b9c82cfe2457c27145d5179385c45970438c`、`ev_1a368ea39a8e74beae83e8e6734c89c5dbc11acfd738f27b0c4118fa92d0fb8b`。

## 对照与开发取舍

| 维度 | Geek | BCUninstaller | 对本项目的取舍 |
|---|---|---|---|
| 轻量产品形态 | 小型原生 EXE，内部带双架构 | 当前 v6 依赖 .NET 8；自包含版附带运行时 | Geek 更适合做体积和交互参考；仍需统一实测 |
| 源码与复用依据 | 已查官方页面没有开放源码授权；Free 标为个人使用 | Apache-2.0，有公开源码 | BCU 更适合评估代码复用 |
| 卸载兼容性 | 本次确认部分登记读取和 MSI 线索 | 源码和文档覆盖多种卸载来源与方式 | 从 BCU 有选择地评估兼容处理，避免整套接入 |
| 系统组件与保护 | 本地样本的完整机制未证明 | 模型明确提供 SystemComponent、IsProtected、IsUpdate 等字段 | BCU 更便于审查；字段不等于任意文件均可安全删除 |
| 迁移及恢复 | 已查官方功能未提供本项目所需跨盘迁移流程 | 不是本次迁移候选 | 继续评估 Viap，与卸载和清理共享链接识别 |

## 验证范围与文件

- 使用 REA 6.8.0 的 PE 导入、资源解析和 Ghidra 12.1.4 静态分析，跟踪了选定函数。
- 未修改用户原始 EXE；未启动目标样本；未卸载或清理实际应用；未进行 Geek/BCU 的同条件内存、速度或误删率测试。
- 反编译结果为分析器恢复的伪代码，不是原始源码。符号、类型、间接调用和动态分支存在未知项。
- `rea-evidence.json` 保存本次 19 条完整 REA 证据；其中 unknowns 字段为空仅表示没有另外创建 REA unknown 记录，不表示所有问题已解决。
- `geek-embedded-505.exe` 是根据 REA 资源区间在本机提取的分析副本，未执行。
- `official-sources.md` 保存官方网页及 BCU 源码链接、授权查证范围和产品版本边界。

官方来源：[Geek About](https://geekuninstaller.com/about)、[Geek Download](https://geekuninstaller.com/download)、[BCU README](https://github.com/BCUninstaller/Bulk-Crap-Uninstaller/blob/master/README.md)、[BCU 条目模型](https://github.com/BCUninstaller/Bulk-Crap-Uninstaller/blob/master/source/UninstallTools/ApplicationUninstallerEntry.cs)。
