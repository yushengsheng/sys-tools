# 第三方来源与许可

## Viap

- 项目：https://github.com/Chunyu33/viap
- 来源文件：`src-tauri/src/migration/links.rs`（2026-10-26 查阅）
- Copyright (c) 2026 Evan Lau，MIT License，完整条款保存在 `third_party/viap-MIT.txt`。
- 本项目 `src/migration/links.rs` 根据其中的链接创建、预检、验证与仅移除链接的处理作了适配；改为仅使用 NTFS Junction，记录、复制校验和恢复流程独立实现。未引入 Viap 界面、自动更新和全部后端。

## Bulk Crap Uninstaller

- 项目：https://github.com/BCUninstaller/Bulk-Crap-Uninstaller
- 参考：`source/UninstallTools/ApplicationUninstallerEntry.cs`、`source/UninstallTools/Factory/InfoAdders/MsiUninstallStringGenerator.cs`、`UninstallerTypeAdder.cs`。
- 原文件 Copyright (c) 2017 Marcin Szeniak，Apache License 2.0；完整许可和原 NOTICE 保存在 `third_party/BCU-Apache-2.0.txt`、`third_party/BCU-NOTICE.txt`。
- 安装登记分类、MSI 缺失命令补全和安装器类型提示按 Rust 独立实现。本地版本只覆盖已验证的小范围，没有声称具备 BCU 的完整卸载兼容性，也未打包其 .NET 组件。

## Cargo 依赖

`Cargo.lock` 固定版本：Windows API 绑定 `windows-sys`、NTFS Junction 库 `junction`、本地记录序列化 `serde` / `serde_json`、完整性校验 `sha2` 及其传递依赖。许可证以各依赖发布包内文件为准；制作分发包前会汇总相应完整许可。

## 产品设计参考

Geek 用于观察轻量产品形态；LightC 用于了解风险提示、缓存与长期数据分离等通用设计。未复制这两个项目的源码、界面素材或规则表。
