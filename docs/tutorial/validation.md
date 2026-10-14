# 教材验证记录与已知边界

基线提交：`1720acefc11fe38cfb9b3bb3d649832ef951d4a8`。验证日期：2026-10-14。仅文档改动；具体结果在完成检查后记录于此。

## 已完成的行为验证

- `cargo test --locked`：189个Rust测试通过，0失败、0忽略；包括单元、所有集成目标与文档测试（当前0项）。实际环境为macOS、rustc 1.88.0；未使用生产API key或外部模型。
- `demo read README.md --json`：临时workspace中的README副本，11事件；最终脚本文本read complete。
- `demo edit-bash note.txt --json`：临时文件before→after，16事件；最终脚本文本edit and bash complete。采用AllowAllPolicy，不验证审批。

## 交叉检查

待最终检查写入：JSONL脚本、edit审批fixture、导航与源码引用、Mermaid、mdBook、Ink类型检查。

## 测试能够证明什么

测试与Demo结果对应文中具体函数/状态。Mock和合法日志fixture能证明闭环与恢复接线，不证明真实模型任务推理正确；故障fixture证明对某日志尾部的分类，不证明在每条OS指令窗口做过硬崩溃注入。所有副作用实验使用临时目录。

## 尚未验证与明确限制

- 没有调用生产OpenAI/DeepSeek API；本地HTTP fixture通过不等于外部服务当日所有模型能力验证。
- 没有外部业务Benchmark。当前无runner、dataset、Harbor适配器；第17章介绍的是已有测试、可执行开发测量和未来评估要求。
- 本次没有在Linux/Windows运行平台分支，没有真人交互终端全键盘/布局验收；源码分析区分了平台与UI行为。
- 当前mdBook没有Mermaid渲染插件，HTML保留图代码块；支持Mermaid的Markdown阅读器可绘图，本次只验证语法及其与调用链的对应。
- 当前不提供OS sandbox、工具exactly-once、脱组后代清理保证、所有排队Turn ID跨重启恢复、自动续跑未完成Loop；这些限制已在相关章节明确。

## 验收对应位置

| 验收目标 | 教材位置 |
|---|---|
| 零基础架构与第一次请求 | 第0—2章 |
| 类型与生命周期区别 | 第3—4章 |
| 主循环与工具完整生命周期 | 第5—8章 |
| 事实/状态/replay与单写者 | 第9—10章 |
| 进程取消、Unknown、Checkpoint | 第11—12章 |
| 审批与安全范围 | 第13章 |
| CLI/协议/Rust与Ink | 第14—15章 |
| 队列/调度/资源/配置与验证 | 第16—17章 |
| 三条完整行为故事 | 第18章 |
| 背景、收益、成本、替代方案 | 第19章 |
| 添加Tool等五类扩展 | 第20章 |

返回[教材首页](README.md) · [源码审查记录](source-audit.md)。
