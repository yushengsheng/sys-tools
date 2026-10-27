# Geek 与 BCUninstaller：官方来源核查

核查日期：2026-10-26。本文只整理官方网页和公开源码信息，不包含运行性能测试，不替代 REA 对本地文件的分析。

## 版本范围

- 本次本地样本版本为 **Geek Uninstaller 1.5.2.165**，由主分析流程提供；本文未独立读取样本版本资源。
- Geek 官网当前下载页列出 **1.6.0.200（2026-10-14）**。其中部分新功能和修复晚于本地样本，不能当成本地样本已经具备的能力。
- 官网发布记录将 1.5.2.165 对应到 2023-09-07。之后的记录包括更安全的残留识别、残留扫描崩溃修复、停止删除和列表构建、ARM64 原生支持等。
- 当前官网下载页显示的压缩包大小是现行发行包信息，不能代替本地 EXE 体积，也不能用于推断内存、启动时间或扫描速度。

## 官方来源与已确认内容

| 来源 | 已确认内容 | 适用范围与限制 |
|---|---|---|
| [Geek 作者介绍](https://geekuninstaller.com/about) | 作者说明 Geek 使用 C++；Pro 名为 Uninstall Tool，由 CrystalIDEA 基于其代码开发。 | 语言声明是作者信息；具体本地构建与依赖由 REA 证据确认。 |
| [Geek 官网](https://geekuninstaller.com/) | 宣传普通卸载、残留扫描、强制移除、Microsoft Store 应用管理、即时搜索和便携单 EXE。 | 功能宣传不证明本地旧样本已包含所有当前行为，也不证明准确性或性能优于 BCU。 |
| [Geek 下载与版本记录](https://geekuninstaller.com/download) | Free 标为个人使用；Pro 额外列出实时安装监控、启动项管理、批量卸载等。 | Free 与 Pro 必须分开；不能将 Pro 功能归给本地 Geek Free。 |
| [Uninstall Tool 官网](https://crystalidea.com/uninstall-tool) | Pro 展示桌面应用、系统组件、隐藏项目、Store 应用四类分组，以及批量卸载与安装跟踪。 | 这些是 Pro 产品声明，不能证明 Geek Free 有同等系统组件识别和保护。 |
| [BCU README](https://github.com/BCUninstaller/Bulk-Crap-Uninstaller/blob/master/README.md) | Apache-2.0；支持残留清理、孤立应用识别、Store、Steam、Windows Features，以及 NSIS、InnoSetup、MSI 等卸载方式。当前 v6 需要 .NET 8 Desktop Runtime；自包含便携版包含运行时。 | 有可审查源码与授权依据；复用时仍须检查具体文件、第三方依赖及许可证义务。 |
| [BCU 软件条目模型](https://github.com/BCUninstaller/Bulk-Crap-Uninstaller/blob/master/source/UninstallTools/ApplicationUninstallerEntry.cs) | 模型包含 SystemComponent、IsProtected、IsUpdate、IsOrphaned 等字段。 | 字段存在不等于可准确判定任意文件能否删除；分类来源和执行保护需进一步跟踪。 |
| [BCU 官网](https://www.bcuninstaller.com/) | 列出多来源软件检测、自动化卸载、证书验证和残留清理，并说明优先使用应用原卸载程序。 | 该页面系统要求保留旧版信息；不要把其中旧 .NET 或资源数字用于当前 v6 的比较。 |

## 授权核查边界

- 已查 Geek 首页、下载页、作者介绍与 Pro 官方页面。Geek Free 下载页明确限定个人使用。
- 在上述已查页面中未发现 Geek Free 的源码发布地址或开放源代码授权，因此**没有证据支持将其作为可移植、修改和随本项目发布的开源底座**。免费使用不自动产生代码复用权或捆绑分发权。
- **仍未知**：本地 Geek Free 附带的完整许可、二进制再分发或集成条款，以及作者可能单独授予的许可。需要查看对应版本附带协议或取得作者确认，不能凭本次有限网页搜索作绝对法律结论。
- [CrystalIDEA EULA](https://crystalidea.com/eula) 明确适用于 Uninstall Tool 等 CrystalIDEA 产品。它不能直接当成 Geek Free 本地样本的许可条款。
- BCU 的 Apache-2.0 为代码评估与合规复用提供依据，但不意味着可以忽略署名、NOTICE 或第三方组件条件。

## 对项目路线的含义

1. **Geek 适合作为产品形态参考**：单窗口、原生列表、即时搜索、少入口、便携和聚焦卸载。它是否达到更好的运行资源指标，应另行实测，本文不作比较。
2. **BCU 更适合作为可核查的卸载实现来源**：可以查阅安装来源识别、卸载方式适配和系统组件标记的实际实现，再按模块决定是否复用。
3. **两者都不能由本次资料证明满足整套迁移需求**：已查 Geek 功能页未列出软件与关联数据跨盘迁移、迁移恢复或通用磁盘清理；这不是对其内部不存在相关代码的证明。迁移仍应独立评估 Viap 等实现。
4. **不能从技术栈或文件大小推出安全性**：C++、单 EXE 或更小的包都不证明残留判断更可靠、保护更完整或卸载更兼容。

## 未解决项

- 本地样本的真实依赖、打包结构、功能入口及保护逻辑：由主 REA 分析流程处理。
- Geek Free 对共享运行库、系统组件、链接目录和迁移后软件的识别边界。
- Geek 与 BCU 在同一机器、同一应用集合、同一权限和缓存条件下的内存、启动与扫描对比。
- 迁移后启动、环境数据、更新、卸载和恢复的实际兼容性。

